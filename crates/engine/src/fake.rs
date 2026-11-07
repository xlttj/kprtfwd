//! A fake API server and kubelet on a local port, and builders for the
//! objects the watch caches hold, for unit tests.
//!
//! The server reads one request per connection and answers it as the test's
//! handler says: with JSON, or by accepting the port-forward upgrade and
//! acting as the kubelet behind it.

use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::{Client, Config};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

pub const VERSION: &str = r#"{"major":"1","minor":"33","gitVersion":"v1.33.0","gitCommit":"","gitTreeState":"","buildDate":"","goVersion":"","compiler":"","platform":""}"#;

/// A request as the fake API server saw it.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Without the query.
    pub path: String,
    pub query: String,
    /// The header lines, lowercased.
    pub head: String,
}

impl Request {
    /// `(pod, port)` of a port-forward request.
    pub fn port_forward(&self) -> Option<(String, u16)> {
        let rest = self.path.strip_suffix("/portforward")?;
        let (_, pod) = rest.rsplit_once("/pods/")?;
        let port = self
            .query
            .split('&')
            .find_map(|pair| pair.strip_prefix("ports="))?
            .parse()
            .ok()?;
        Some((pod.to_owned(), port))
    }
}

pub enum Reply {
    /// An HTTP answer with a JSON body.
    Json(u16, String),
    /// Accepts the port-forward upgrade and acts as the kubelet.
    Kubelet(Kubelet),
    /// Answers only when as many requests wait at the barrier as it counts,
    /// which shows that they run at the same time.
    Gated(Arc<tokio::sync::Barrier>, Box<Reply>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kubelet {
    /// Sends the port headers, then answers every data frame with the
    /// stream's tag `pod#n:` and the data.
    Echo,
    /// Sends the port headers and this text on the error channel.
    Error(String),
    /// Sends the port headers and this text as data, unasked.
    Greeting(String),
}

/// One port-forward stream the kubelet accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    pub pod: String,
    pub port: u16,
    /// Counts from 1 across all streams of the server.
    pub number: usize,
    pub open: bool,
}

type Handler = Arc<dyn Fn(&Request) -> Reply + Send + Sync>;

#[derive(Default)]
struct Log {
    requests: Vec<String>,
    streams: Vec<Stream>,
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

    pub fn port(&self) -> u16 {
        self.url.rsplit(':').next().unwrap().parse().unwrap()
    }

    /// A client without credentials for this server.
    pub fn client(&self) -> Client {
        Client::try_from(Config::new(self.url.parse().unwrap())).unwrap()
    }

    /// `METHOD /path?query` of every request so far.
    pub fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().requests.clone()
    }

    pub fn streams(&self) -> Vec<Stream> {
        self.log.lock().unwrap().streams.clone()
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
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let request = Request {
        method,
        path: path.to_owned(),
        query: query.to_owned(),
        head: head.to_lowercase(),
    };
    log.lock()
        .unwrap()
        .requests
        .push(format!("{} {target}", request.method));

    let mut reply = handler(&request);
    while let Reply::Gated(gate, inner) = reply {
        gate.wait().await;
        reply = *inner;
    }
    match reply {
        Reply::Gated(..) => unreachable!("opened above"),
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
        Reply::Kubelet(kubelet) => {
            let Some((pod, port)) = request.port_forward() else {
                return;
            };
            // The key is case-sensitive, so it comes from the original head.
            let Some(key) = head.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("sec-websocket-key")
                    .then(|| value.trim().to_owned())
            }) else {
                return;
            };
            let response = format!(
                "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\n\
                 connection: Upgrade\r\nsec-websocket-accept: {}\r\n\
                 sec-websocket-protocol: v4.channel.k8s.io\r\n\r\n",
                derive_accept_key(key.as_bytes())
            );
            if socket.write_all(response.as_bytes()).await.is_err() {
                return;
            }
            let number = {
                let mut log = log.lock().unwrap();
                let number = log.streams.len() + 1;
                log.streams.push(Stream {
                    pod: pod.clone(),
                    port,
                    number,
                    open: true,
                });
                number
            };
            let ws = WebSocketStream::from_raw_socket(socket, Role::Server, None).await;
            kubelet_stream(ws, &kubelet, &format!("{pod}#{number}:"), port).await;
            log.lock().unwrap().streams[number - 1].open = false;
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

fn frame(channel: u8, payload: &[u8]) -> Message {
    let mut frame = vec![channel];
    frame.extend_from_slice(payload);
    Message::binary(frame)
}

async fn kubelet_stream(
    mut ws: WebSocketStream<TcpStream>,
    kubelet: &Kubelet,
    tag: &str,
    port: u16,
) {
    for channel in [0, 1] {
        if ws.send(frame(channel, &port.to_le_bytes())).await.is_err() {
            return;
        }
    }
    let first = match kubelet {
        Kubelet::Echo => None,
        Kubelet::Error(text) => Some(frame(1, text.as_bytes())),
        Kubelet::Greeting(text) => Some(frame(0, text.as_bytes())),
    };
    if let Some(first) = first
        && ws.send(first).await.is_err()
    {
        return;
    }
    while let Some(Ok(message)) = ws.next().await {
        match message {
            Message::Binary(data) if *kubelet == Kubelet::Echo && data.first() == Some(&0) => {
                let mut answer = tag.as_bytes().to_vec();
                answer.extend_from_slice(&data[1..]);
                if ws.send(frame(0, &answer)).await.is_err() {
                    return;
                }
            }
            Message::Close(_) => return,
            _ => {}
        }
    }
}

/// A local port that refuses connections. It lies below the ports the
/// system hands out to sockets bound to port 0 and to outgoing connections
/// (from 32768 on Linux, 49152 on macOS), so unlike one of [`free_port`] no
/// other socket of the tests can take it. It is checked to refuse.
pub fn closed_port() -> u16 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    // Test processes running at the same time start at different ports.
    let seed = std::process::id() % 1000 * 11;
    for _ in 0..100 {
        let offset = (seed + NEXT.fetch_add(1, Ordering::Relaxed)) % 12_000;
        let port = 20_000 + u16::try_from(offset).unwrap();
        let refused = std::net::TcpStream::connect_timeout(
            &([127, 0, 0, 1], port).into(),
            std::time::Duration::from_secs(1),
        )
        .is_err_and(|err| err.kind() == std::io::ErrorKind::ConnectionRefused);
        if refused {
            return port;
        }
    }
    panic!("no closed port from 20000 on");
}

/// Whether a process other than this one listens on `port`. macOS has no
/// atomic close-on-exec for sockets, so a process that a test starts just
/// as another test creates a socket inherits it and may hold that test's
/// port for as long as it runs.
pub async fn held_by_another_process(port: u16) -> bool {
    let own = format!("(pid {})", std::process::id());
    crate::forward::port_holder(port)
        .await
        .is_some_and(|holder| !holder.ends_with(&own))
}

/// Whether `port` is free within a second. A process that a test starts
/// holds copies of the test process's sockets until it has started, so a
/// listener just closed may keep its port a moment. Polls without yielding,
/// so the tasks of a single-threaded test runtime do not run meanwhile.
pub fn freed(port: u16, free: impl Fn(u16) -> bool) -> bool {
    let started = std::time::Instant::now();
    while !free(port) {
        if started.elapsed() > std::time::Duration::from_secs(1) {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    true
}

/// A local port that was free a moment ago. Another socket, such as the
/// local end of a connection of a test running in parallel, may take it
/// before it is bound.
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

/// A TCP port; an empty name is left out, as for a service's only port.
fn port_json(name: &str, port: u16) -> Value {
    let mut json = json!({"port": port, "protocol": "TCP"});
    if !name.is_empty() {
        json["name"] = json!(name);
    }
    json
}

/// A service in namespace `shop` with TCP ports `(name, port)`.
pub fn service(name: &str, ports: &[(&str, u16)]) -> Service {
    serde_json::from_value(service_json("shop", name, ports)).unwrap()
}

fn service_json(namespace: &str, name: &str, ports: &[(&str, u16)]) -> Value {
    let ports: Vec<Value> = ports
        .iter()
        .map(|(name, port)| port_json(name, *port))
        .collect();
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {"name": name, "namespace": namespace},
        "spec": {"ports": ports},
    })
}

/// An EndpointSlice of `service` with container ports `(name, port)` and
/// endpoints `(pod, ready)`; `None` leaves `ready` unset.
pub fn slice(
    name: &str,
    service: &str,
    ports: &[(&str, u16)],
    endpoints: &[(&str, Option<bool>)],
) -> EndpointSlice {
    let ports: Vec<Value> = ports
        .iter()
        .map(|(name, port)| port_json(name, *port))
        .collect();
    let endpoints: Vec<Value> = endpoints
        .iter()
        .enumerate()
        .map(|(i, (pod, ready))| {
            let mut conditions = json!({});
            if let Some(ready) = ready {
                conditions["ready"] = json!(ready);
            }
            json!({
                "addresses": [format!("10.0.0.{}", i + 1)],
                "conditions": conditions,
                "targetRef": {"kind": "Pod", "name": pod, "namespace": "shop"},
            })
        })
        .collect();
    serde_json::from_value(json!({
        "apiVersion": "discovery.k8s.io/v1",
        "kind": "EndpointSlice",
        "metadata": {
            "name": name,
            "namespace": "shop",
            "labels": {"kubernetes.io/service-name": service},
        },
        "addressType": "IPv4",
        "ports": ports,
        "endpoints": endpoints,
    }))
    .unwrap()
}
