//! The daemon's life cycle through the real binary: switching the forwards
//! off when the last TUI quits, and taking over `~/.kprtfwd`.
//!
//! Every command runs in a home directory of its own; see `common`.

mod common;

use std::time::Duration;

use common::{Home, database, enabled, wait_until};
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
