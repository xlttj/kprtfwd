//! The daemon through the real binary: switching the forwards off when the
//! last TUI quits, taking over `~/.kprtfwd`, and watching the kubeconfig.
//!
//! Every command runs in a home directory of its own; see `common`.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use common::{Home, NOT_FOUND, database, enabled, fake_api, forward, wait_until, write_database};
use kprtfwd_core::Store;
use kprtfwd_core::config::TEMPLATE;
use kprtfwd_proto::{Client, Snapshot};

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

#[test]
fn a_failed_take_over_is_tried_again() {
    let home = Home::new();
    let legacy_db = home.legacy_dir().join("kprtfwd.db");
    std::fs::create_dir_all(home.legacy_dir()).unwrap();
    // SQLite refuses to read it, so the copy fails.
    std::fs::write(&legacy_db, [0x55; 4096]).unwrap();

    let output = home.run(&["status", "--json"]);
    assert!(!output.status.success(), "{output:?}");
    let db = home.data_dir().join("kprtfwd.db");
    assert!(!db.exists(), "a failed copy looks like a database");

    std::fs::remove_file(&legacy_db).unwrap();
    database(&legacy_db);
    let output = home.run(&["status", "--json"]);
    let snapshot: Snapshot =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|err| panic!("{err}: {output:?}"));
    assert_eq!(snapshot.forwards.len(), 1);
}

const VERSION: &str = r#"{"major":"1","minor":"33","gitVersion":"v1.33.0","gitCommit":"","gitTreeState":"","buildDate":"","goVersion":"","compiler":"","platform":""}"#;

/// An API server that answers `GET /version` only.
fn version_api() -> u16 {
    fake_api(|path| {
        if path == "/version" {
            ("200 OK", VERSION)
        } else {
            NOT_FOUND
        }
    })
}

/// An exec plugin that appends a line to `$RUNS` for every login.
fn plugin(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("plugin");
    std::fs::write(
        &path,
        r#"#!/bin/sh
echo login >> "$RUNS"
echo '{"apiVersion":"client.authentication.k8s.io/v1","kind":"ExecCredential","status":{"token":"good","expirationTimestamp":"2999-01-01T00:00:00Z"}}'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Contexts `test` and `other` on the API server at `port`, both with a user
/// that logs in through `plugin` and has env entries; `extra` goes into the
/// context `test`.
fn exec_kubeconfig(home: &Home, plugin: &Path, port: u16, current: &str, extra: &str) -> String {
    format!(
        r#"apiVersion: v1
kind: Config
current-context: {current}
clusters:
- name: test
  cluster:
    server: http://127.0.0.1:{port}
users:
- name: test
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1
      command: {plugin}
      env:
      - name: RUNS
        value: {runs}
      - name: PROFILE
        value: shop
      - name: REGION
        value: eu
contexts:
- name: test
  context:
    cluster: test
    user: test{extra}
- name: other
  context:
    cluster: test
    user: test
"#,
        plugin = plugin.display(),
        runs = home.path().join("runs").display(),
    )
}

/// All of the daemon's log files.
fn daemon_log(home: &Home) -> String {
    let mut files: Vec<_> = std::fs::read_dir(home.data_dir().join("logs"))
        .map(|dir| {
            dir.filter_map(|entry| entry.ok().map(|e| e.path()))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
        .iter()
        .filter_map(|file| std::fs::read_to_string(file).ok())
        .collect()
}

#[tokio::test]
async fn only_new_client_settings_reconnect_a_context() {
    let mut home = Home::new();
    let plugin = plugin(home.path());
    let first = version_api();
    home.set_kubeconfig(&exec_kubeconfig(&home, &plugin, first, "test", ""));
    write_database(&home.db(), &[forward("test", "shop", "api", true)], false);
    let output = home
        .command(&["daemon", "restart"])
        .env("KPRTFWD_LOG", "info,kprtfwd_engine::kubeconfig=debug")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let logins = || {
        std::fs::read_to_string(home.path().join("runs"))
            .map(|runs| runs.lines().count())
            .unwrap_or(0)
    };
    let count = |text: &str| daemon_log(&home).matches(text).count();
    wait_until("watching", || count("watching kubeconfig") == 1).await;
    wait_until("logged in", || logins() == 1).await;

    // `kubectl config use-context`, `kubens` and a rewrite of the user with
    // its env entries: the same client, so no reconnect and no login.
    let file = home.kubeconfig_file();
    let kubens = "\n    namespace: web";
    let same = exec_kubeconfig(&home, &plugin, first, "other", kubens);
    std::fs::write(&file, same).unwrap();
    wait_until("read", || count("kubeconfig read") == 1).await;

    // A moved cluster reconnects the context, which logs in again.
    let second = version_api();
    let moved = exec_kubeconfig(&home, &plugin, second, "other", kubens);
    std::fs::write(&file, moved).unwrap();
    wait_until("read again", || count("kubeconfig read") == 2).await;
    wait_until("logged in again", || logins() >= 2).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(count("kubeconfig changed, reconnecting"), 1);
    assert_eq!(logins(), 2);
}
