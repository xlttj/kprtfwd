//! What the binary tests share: a home directory of their own, databases to
//! start from, and a fake API server.
//!
//! Every command runs with `HOME` set to a fresh directory and without
//! `KPRTFWD_HOME` and `XDG_CONFIG_HOME`, so nothing touches the real data.
//! `KUBECONFIG` is removed too, unless a test gives its own files. Daemons are
//! only ever started through [`Home::command`]: one started from the test
//! process would inherit the real `HOME`.

// Each test file uses a part.
#![allow(dead_code)]

use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use kprtfwd_core::{Forward, ForwardId, Store};
use kprtfwd_proto::{Client, Request};

pub struct Home {
    dir: tempfile::TempDir,
    /// `KUBECONFIG` for commands.
    kubeconfig: Option<OsString>,
}

impl Home {
    pub fn new() -> Self {
        // Short, so the socket path fits.
        let dir = tempfile::Builder::new()
            .prefix("h")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        Self {
            dir,
            kubeconfig: None,
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Writes a kubeconfig that commands, and so the daemons they start, use.
    pub fn set_kubeconfig(&mut self, yaml: &str) {
        let path = self.kubeconfig_file();
        std::fs::write(&path, yaml).unwrap();
        self.kubeconfig = Some(path.into());
    }

    /// The file [`Home::set_kubeconfig`] writes.
    pub fn kubeconfig_file(&self) -> PathBuf {
        self.path().join("kubeconfig")
    }

    /// Sets `KUBECONFIG` for commands to a list of files.
    pub fn set_kubeconfig_files(&mut self, files: &[PathBuf]) {
        self.kubeconfig = Some(std::env::join_paths(files).unwrap());
    }

    pub fn data_dir(&self) -> PathBuf {
        self.path().join(".config/kprtfwd")
    }

    pub fn legacy_dir(&self) -> PathBuf {
        self.path().join(".kprtfwd")
    }

    pub fn db(&self) -> PathBuf {
        self.data_dir().join("kprtfwd.db")
    }

    pub fn socket(&self) -> PathBuf {
        self.data_dir().join("daemon.sock")
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kprtfwd"));
        command
            .args(args)
            .env("HOME", self.path())
            .env_remove("KPRTFWD_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("KUBECONFIG");
        if let Some(kubeconfig) = &self.kubeconfig {
            command.env("KUBECONFIG", kubeconfig);
        }
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run kprtfwd")
    }

    /// Runs with `input` on stdin, as typed by the user.
    pub fn run_with_input(&self, args: &[&str], input: &str) -> Output {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run kprtfwd");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    pub fn start_daemon(&self) {
        let output = self.run(&["daemon", "restart"]);
        assert!(output.status.success(), "{output:?}");
    }

    /// Connects like the TUI does.
    pub async fn attach(&self) -> Client {
        let mut client = Client::connect(&self.socket()).await.expect("connect");
        client.act(Request::Attach).await.expect("attach");
        client
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        // Daemons a failed test left behind, in both directories.
        let _ = self.run(&["daemon", "stop"]);
        let _ = self
            .command(&["daemon", "stop"])
            .env("KPRTFWD_HOME", self.legacy_dir())
            .output();
    }
}

/// A forward `<context>.<namespace>.<service>-80` on a free local port.
pub fn forward(context: &str, namespace: &str, service: &str, enabled: bool) -> Forward {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .unwrap()
        .port();
    Forward {
        id: ForwardId::new(format!("{context}.{namespace}.{service}-80")),
        context: context.into(),
        namespace: namespace.into(),
        service: service.into(),
        remote_port: 80,
        local_port: port,
        enabled,
    }
}

pub fn write_database(path: &Path, forwards: &[Forward], paused: bool) {
    let mut store = Store::open(path).unwrap();
    for forward in forwards {
        store.upsert_forward(forward).unwrap();
    }
    store.set_paused(paused).unwrap();
}

/// Writes a database with one enabled forward to a context that does not
/// exist, and with the pause on.
pub fn database(path: &Path) {
    write_database(path, &[forward("test", "shop", "api", true)], true);
}

/// The IDs of the forwards in the database, sorted, and of those enabled.
pub fn forward_ids(path: &Path) -> (Vec<String>, Vec<String>) {
    let mut forwards = Store::snapshot(path).unwrap().forwards().unwrap();
    forwards.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    let ids = |enabled_only: bool| {
        forwards
            .iter()
            .filter(|f| !enabled_only || f.enabled)
            .map(|f| f.id.to_string())
            .collect()
    };
    (ids(false), ids(true))
}

pub fn enabled(path: &Path) -> bool {
    Store::snapshot(path).unwrap().forwards().unwrap()[0].enabled
}

pub async fn wait_until(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(10), "not {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A status line and a JSON body.
pub type Answer = (&'static str, &'static str);

pub const NOT_FOUND: Answer = (
    "404 Not Found",
    r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":404}"#,
);

/// An API server in this process that answers every request with what
/// `answer` returns for its path. Returns its port.
pub fn fake_api(answer: fn(&str) -> Answer) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for socket in listener.incoming() {
            let Ok(mut socket) = socket else { continue };
            let mut head = Vec::new();
            let mut chunk = [0; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match socket.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&chunk[..n]),
                }
            }
            let head = String::from_utf8_lossy(&head);
            let path = head.split_whitespace().nth(1).unwrap_or_default();
            let (status, body) = answer(path);
            let _ = write!(
                socket,
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    port
}
