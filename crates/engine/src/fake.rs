//! A fake API server on a local port, for unit tests.
//!
//! The server reads one request per connection and answers it as the test's
//! handler says.

use std::sync::{Arc, Mutex};

use kube::{Client, Config};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

pub const VERSION: &str = r#"{"major":"1","minor":"33","gitVersion":"v1.33.0","gitCommit":"","gitTreeState":"","buildDate":"","goVersion":"","compiler":"","platform":""}"#;

/// A request as the fake API server saw it.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Without the query.
    pub path: String,
    /// The header lines, lowercased.
    pub head: String,
}

pub enum Reply {
    /// An HTTP answer with a JSON body.
    Json(u16, String),
}

type Handler = Arc<dyn Fn(&Request) -> Reply + Send + Sync>;

#[derive(Default)]
struct Log {
    requests: Vec<String>,
}

pub struct FakeApi {
    pub url: String,
    log: Arc<Mutex<Log>>,
    task: JoinHandle<()>,
}

impl FakeApi {
    pub async fn start(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        Self::start_at(0, handler).await
    }

    /// Listens on a given port, e.g. one a client tried before.
    pub async fn start_at(
        port: u16,
        handler: impl Fn(&Request) -> Reply + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Log::default()));
        let handler: Handler = Arc::new(handler);
        let task = tokio::spawn(serve(listener, handler, log.clone()));
        Self { url, log, task }
    }

    /// A client without credentials for this server.
    pub fn client(&self) -> Client {
        Client::try_from(Config::new(self.url.parse().unwrap())).unwrap()
    }

    /// `METHOD /path?query` of every request so far.
    pub fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().requests.clone()
    }
}

impl Drop for FakeApi {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(listener: TcpListener, handler: Handler, log: Arc<Mutex<Log>>) {
    while let Ok((socket, _)) = listener.accept().await {
        tokio::spawn(answer(socket, handler.clone(), log.clone()));
    }
}

async fn answer(mut socket: TcpStream, handler: Handler, log: Arc<Mutex<Log>>) {
    let mut head = Vec::new();
    let mut chunk = [0; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&chunk[..n]),
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut words = head.split_whitespace();
    let method = words.next().unwrap_or_default().to_owned();
    let target = words.next().unwrap_or_default().to_owned();
    let path = target
        .split_once('?')
        .map_or(target.as_str(), |(path, _)| path);
    let request = Request {
        method,
        path: path.to_owned(),
        head: head.to_lowercase(),
    };
    log.lock()
        .unwrap()
        .requests
        .push(format!("{} {target}", request.method));

    match handler(&request) {
        Reply::Json(status, body) => {
            let response = format!(
                "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                reason(status),
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Status",
    }
}

/// A local port that was free a moment ago.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A `Status` object as the API server sends it with an error.
pub fn status(code: u16, message: &str) -> String {
    json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": message,
        "reason": reason(code),
        "code": code,
    })
    .to_string()
}

/// A list answer, e.g. `ServiceList`.
pub fn list(kind: &str, items: &[Value]) -> String {
    json!({
        "kind": kind,
        "apiVersion": "v1",
        "metadata": {"resourceVersion": "1"},
        "items": items,
    })
    .to_string()
}
