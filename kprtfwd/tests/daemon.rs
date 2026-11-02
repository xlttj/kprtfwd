//! The daemon's life cycle through the real binary: switching the forwards
//! off when the last TUI quits, and taking over `~/.kprtfwd`.
//!
//! Every command runs with `HOME` set to a fresh directory and without
//! `KPRTFWD_HOME`, `XDG_CONFIG_HOME` and `KUBECONFIG`, so nothing touches the
//! real data or a real cluster. Daemons are only ever started through
//! [`Home::command`]: one started from this process would inherit the real
//! `HOME`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use kprtfwd_core::config::TEMPLATE;
use kprtfwd_core::{Forward, ForwardId, Store};
use kprtfwd_proto::{Client, Request, Snapshot};

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        // Short, so the socket path fits.
        let dir = tempfile::Builder::new()
            .prefix("h")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        Self { dir }
    }

    fn data_dir(&self) -> PathBuf {
        self.dir.path().join(".config/kprtfwd")
    }

    fn legacy_dir(&self) -> PathBuf {
        self.dir.path().join(".kprtfwd")
    }

    fn socket(&self) -> PathBuf {
        self.data_dir().join("daemon.sock")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kprtfwd"));
        command
            .args(args)
            .env("HOME", self.dir.path())
            .env_remove("KPRTFWD_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("KUBECONFIG");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run kprtfwd")
    }

    fn start_daemon(&self) {
        let output = self.run(&["daemon", "restart"]);
        assert!(output.status.success(), "{output:?}");
    }

    /// Connects like the TUI does.
    async fn attach(&self) -> Client {
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

/// Writes a database with one enabled forward to a context that does not
/// exist, and with the pause on.
fn database(path: &Path) {
    let mut store = Store::open(path).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .unwrap()
        .port();
    store
        .upsert_forward(&Forward {
            id: ForwardId::new("test.shop.api-80"),
            context: "test".into(),
            namespace: "shop".into(),
            service: "api".into(),
            remote_port: 80,
            local_port: port,
            enabled: true,
        })
        .unwrap();
    store.set_paused(true).unwrap();
}

fn enabled(path: &Path) -> bool {
    Store::snapshot(path).unwrap().forwards().unwrap()[0].enabled
}

async fn wait_until(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(10), "not {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn quitting_the_last_tui_switches_the_forwards_off() {
    let home = Home::new();
    let db = home.data_dir().join("kprtfwd.db");
    database(&db);
    home.start_daemon();

    let first = home.attach().await;
    let mut second = home.attach().await;
    drop(first);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let snapshot = second.snapshot().await.expect("daemon still there");
    assert!(snapshot.forwards[0].forward.enabled);

    drop(second);
    wait_until("stopped", || !home.socket().exists()).await;
    let store = Store::snapshot(&db).unwrap();
    assert!(!store.forwards().unwrap()[0].enabled);
    assert!(!store.paused().unwrap(), "the pause stays on");
    assert_eq!(
        std::fs::read_to_string(home.data_dir().join("config.toml")).unwrap(),
        TEMPLATE
    );
}

#[tokio::test]
async fn other_clients_and_the_setting_keep_forwards_running() {
    let home = Home::new();
    let db = home.data_dir().join("kprtfwd.db");
    database(&db);
    std::fs::write(
        home.data_dir().join("config.toml"),
        "keep_forwards_running = true\n",
    )
    .unwrap();
    // `daemon restart` connects without attaching, and leaves the daemon running.
    home.start_daemon();

    drop(home.attach().await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut client = Client::connect(&home.socket())
        .await
        .expect("daemon still there");
    client.snapshot().await.unwrap();
    assert!(enabled(&db));
}

#[tokio::test]
async fn takes_over_the_old_directory() {
    let home = Home::new();
    let legacy_db = home.legacy_dir().join("kprtfwd.db");
    database(&legacy_db);
    // A daemon still running in the old directory, as after an upgrade.
    let output = home
        .command(&["daemon", "restart"])
        .env("KPRTFWD_HOME", home.legacy_dir())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let legacy_socket = home.legacy_dir().join("daemon.sock");
    assert!(legacy_socket.exists());

    let output = home.run(&["status", "--json"]);
    let snapshot: Snapshot =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|err| panic!("{err}: {output:?}"));

    assert_eq!(snapshot.forwards.len(), 1);
    assert!(snapshot.forwards[0].forward.enabled);
    assert!(snapshot.paused);
    assert!(!legacy_socket.exists(), "the old daemon still runs");
    assert!(legacy_db.exists(), "the old database is gone");
    assert!(home.data_dir().join("kprtfwd.db").exists());
}
