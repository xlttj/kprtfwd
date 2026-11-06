//! The command line through the real binary: exit codes, switching forwards,
//! prune and logs.
//!
//! Every command runs in a home directory of its own; see `common`. Prune
//! asks a fake API server in this process.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::process::{Child, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::{Home, NOT_FOUND, forward, forward_ids, write_database};
use kprtfwd_proto::{PROTOCOL_VERSION, Response, ServerMessage, VERSION};

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn daemon_status_without_a_daemon_exits_with_4() {
    let home = Home::new();
    let output = home.run(&["daemon", "status"]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert_eq!(stdout(&output), "daemon not running\n");
}

#[test]
fn wrong_usage_exits_with_2_without_starting_a_daemon() {
    let home = Home::new();
    let output = home.run(&["status", "--jsno"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr(&output).contains("--jsno"), "{output:?}");
    assert!(!home.socket().exists());

    let output = home.run(&["--help"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        stdout(&output).contains("4  the daemon is not reachable"),
        "{output:?}"
    );
}

#[test]
fn a_daemon_that_cannot_start_exits_with_4() {
    let home = Home::new();
    // The daemon cannot create its log directory, so it exits at once.
    std::fs::create_dir_all(home.data_dir()).unwrap();
    std::fs::write(home.data_dir().join("logs"), "").unwrap();

    let output = home.run(&["status"]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let err = stderr(&output);
    assert!(
        err.contains("the daemon exited right after starting"),
        "{err}"
    );
    let logs = home.data_dir().join("logs");
    assert!(err.contains(&format!("log: {}", logs.display())), "{err}");
}

/// A daemon that welcomes one client and closes the connection on its first
/// request, as one that crashes does; its socket goes with it.
fn daemon_that_dies(home: &Home) -> std::thread::JoinHandle<()> {
    std::fs::create_dir_all(home.data_dir()).unwrap();
    let socket = home.socket();
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut hello = String::new();
        reader.read_line(&mut hello).unwrap();
        let welcome = ServerMessage::Response {
            id: 1,
            response: Response::Welcome {
                protocol: PROTOCOL_VERSION,
                daemon_version: VERSION.into(),
                pid: std::process::id(),
            },
        };
        writeln!(stream, "{}", serde_json::to_string(&welcome).unwrap()).unwrap();
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        std::fs::remove_file(&socket).unwrap();
    })
}

#[test]
fn a_daemon_that_goes_away_during_a_command_exits_with_4() {
    let home = Home::new();
    let daemon = daemon_that_dies(&home);
    let output = home.run(&["status"]);
    daemon.join().unwrap();
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert!(
        stderr(&output).contains("the daemon closed the connection"),
        "{output:?}"
    );
}

#[test]
fn status_exits_with_3_until_every_enabled_forward_is_ready() {
    let home = Home::new();
    // Its context is in no kubeconfig, so it cannot become ready.
    write_database(&home.db(), &[forward("test", "shop", "api", true)], false);

    let output = home.run(&["status"]);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(stdout(&output).contains("shop/api:80"), "{output:?}");
    let output = home.run(&["status", "--json"]);
    assert_eq!(output.status.code(), Some(3), "{output:?}");

    // Paused counts as ready.
    assert!(home.run(&["pause"]).status.success());
    let output = home.run(&["status"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
}

/// The documented format: the forwards the table shows, with its texts.
#[test]
fn status_json_shows_what_the_table_shows() {
    let home = Home::new();
    let api = forward("test", "shop", "api", true);
    let local_port = api.local_port;
    write_database(
        &home.db(),
        &[api, forward("test", "shop", "web", false)],
        false,
    );
    let json = |args: &[&str]| -> serde_json::Value {
        let output = home.run(args);
        assert_eq!(output.status.code(), Some(3), "{output:?}");
        serde_json::from_slice(&output.stdout).expect("JSON")
    };

    let status = json(&["status", "--json"]);
    assert_eq!(status["version"], 1);
    assert_eq!(status["paused"], false);
    assert_eq!(status["contexts"][0]["name"], "test");
    assert!(status["contexts"][0]["state"].is_string(), "{status}");
    let forwards = status["forwards"].as_array().unwrap();
    assert_eq!(forwards.len(), 1, "{status}");
    let api = forwards[0].as_object().unwrap();
    let mut keys: Vec<&str> = api.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "context",
            "enabled",
            "id",
            "info",
            "local_port",
            "namespace",
            "remote_port",
            "service",
            "state"
        ]
    );
    assert_eq!(api["id"], "test.shop.api-80");
    assert_eq!(api["local_port"], local_port);
    assert_eq!(api["enabled"], true);
    // Its context is in no kubeconfig.
    assert_eq!(api["state"], "waiting");
    assert!(!api["info"].as_str().unwrap().is_empty(), "{status}");

    let status = json(&["status", "--json", "--all"]);
    let ids: Vec<&str> = status["forwards"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["test.shop.api-80", "test.shop.web-80"]);
    assert_eq!(status["forwards"][1]["state"], "off");
}

#[test]
fn up_and_down_switch_the_forwards_that_match() {
    let home = Home::new();
    let api = forward("test", "shop", "api", false);
    let web = forward("test", "shop", "web", false);
    write_database(&home.db(), &[api, web], true);
    let enabled = || forward_ids(&home.db()).1;

    let output = home.run(&["up", "redis"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        stderr(&output).contains("no project or forward matches \"redis\""),
        "{output:?}"
    );

    assert!(home.run(&["up", "api"]).status.success());
    assert_eq!(enabled(), ["test.shop.api-80"]);
    // All words must match; "test" matches both by context.
    assert!(home.run(&["up", "test", "web"]).status.success());
    assert_eq!(enabled(), ["test.shop.api-80", "test.shop.web-80"]);
    assert!(home.run(&["down", "api"]).status.success());
    assert_eq!(enabled(), ["test.shop.web-80"]);

    // No words, or only empty ones as from an unset variable, are wrong
    // usage, not every forward.
    for args in [&["down"][..], &["down", ""], &["up", " "]] {
        let output = home.run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert_eq!(enabled(), ["test.shop.web-80"], "{args:?}");
    }
    assert!(
        stderr(&home.run(&["down"])).contains("--all"),
        "the usage names --all"
    );

    let output = home.run(&["down", "--all"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(stdout(&output), "Switched 1 forwards off.\n");
    assert!(enabled().is_empty());
}

const SERVICES: &str = r#"{"kind":"ServiceList","apiVersion":"v1","metadata":{"resourceVersion":"1"},"items":[{"metadata":{"name":"api","namespace":"shop"},"spec":{"ports":[{"name":"http","port":80,"protocol":"TCP"}]}}]}"#;
const NO_SERVICES: &str =
    r#"{"kind":"ServiceList","apiVersion":"v1","metadata":{"resourceVersion":"1"},"items":[]}"#;

/// An API server with one service, shop/api, listed across the cluster or
/// per namespace.
fn fake_api() -> u16 {
    common::fake_api(|path| {
        if path.starts_with("/api/v1/services?")
            || path.starts_with("/api/v1/namespaces/shop/services?")
        {
            ("200 OK", SERVICES)
        } else if path.starts_with("/api/v1/namespaces/") && path.contains("/services?") {
            ("200 OK", NO_SERVICES)
        } else {
            NOT_FOUND
        }
    })
}

const NAMESPACES: &str = r#"{"kind":"NamespaceList","apiVersion":"v1","metadata":{"resourceVersion":"1"},"items":[{"metadata":{"name":"shop"}},{"metadata":{"name":"web"}}]}"#;
const FORBIDDEN: &str = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"services is forbidden","reason":"Forbidden","code":403}"#;

/// An API server where the user may list the namespaces, shop and web, but
/// services only in shop.
fn fake_api_with_rights_in_shop() -> u16 {
    common::fake_api(|path| {
        if path.starts_with("/api/v1/namespaces?") {
            ("200 OK", NAMESPACES)
        } else if path.starts_with("/api/v1/namespaces/shop/services?") {
            ("200 OK", SERVICES)
        } else if path.starts_with("/api/v1/services?")
            || path.starts_with("/api/v1/namespaces/web/services?")
        {
            ("403 Forbidden", FORBIDDEN)
        } else {
            NOT_FOUND
        }
    })
}

fn kubeconfig(port: u16) -> String {
    format!(
        r#"apiVersion: v1
kind: Config
current-context: test
clusters:
- name: test
  cluster:
    server: http://127.0.0.1:{port}
users:
- name: test
  user: {{}}
contexts:
- name: test
  context:
    cluster: test
    user: test
"#
    )
}

#[test]
fn prune_deletes_only_what_the_user_confirmed() {
    let mut home = Home::new();
    home.set_kubeconfig(&kubeconfig(fake_api()));
    write_database(
        &home.db(),
        &[
            forward("test", "shop", "api", false),
            forward("test", "shop", "old", false),
            forward("test", "web", "legacy", false),
            forward("other", "shop", "old", false),
        ],
        false,
    );
    let ids = || forward_ids(&home.db()).0;
    let all = ids();

    // Listed, then declined: nothing is deleted.
    let output = home.run_with_input(&["prune", "--context", "test"], "n\n");
    assert!(output.status.success(), "{output:?}");
    let out = stdout(&output);
    assert!(
        out.contains("Forwards whose service is gone, in context test:"),
        "{out}"
    );
    for id in ["test.shop.old-80", "test.web.legacy-80"] {
        assert!(out.contains(id), "{out}");
    }
    for id in ["test.shop.api-80", "other.shop.old-80"] {
        assert!(!out.contains(id), "{out}");
    }
    assert!(
        out.contains("Delete these 2 forwards of context test? [y/N]"),
        "{out}"
    );
    assert_eq!(ids(), all);

    // Confirmed, in the daemon's current context and one namespace.
    let output = home.run_with_input(&["prune", "--namespace", "web"], "y\n");
    let out = stdout(&output);
    assert!(
        out.contains("in context test (the daemon's current context), namespaces web:"),
        "{out}"
    );
    assert!(out.contains("Deleted test.web.legacy-80"), "{out}");
    assert!(!ids().contains(&"test.web.legacy-80".to_owned()));

    // -y does not ask.
    let output = home
        .command(&["prune", "-y", "--context", "test"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let out = stdout(&output);
    assert!(!out.contains("[y/N]"), "{out}");
    assert!(out.contains("Deleted test.shop.old-80"), "{out}");
    assert_eq!(ids(), ["other.shop.old-80", "test.shop.api-80"]);

    let output = home.run(&["prune", "-y"]);
    assert_eq!(
        stdout(&output),
        "Nothing to prune in context test (the daemon's current context).\n"
    );
}

/// Whether the services of a namespace the user may not read still exist is
/// unknown, so its forwards are left alone and the namespace is named.
#[test]
fn prune_leaves_namespaces_alone_whose_services_are_forbidden() {
    let mut home = Home::new();
    home.set_kubeconfig(&kubeconfig(fake_api_with_rights_in_shop()));
    write_database(
        &home.db(),
        &[
            forward("test", "shop", "api", false),
            forward("test", "shop", "old", false),
            forward("test", "web", "legacy", false),
        ],
        false,
    );

    let output = home.run_with_input(&["prune", "--context", "test"], "y\n");
    assert!(output.status.success(), "{output:?}");
    let out = stdout(&output);
    assert!(out.contains("test.shop.old-80"), "{out}");
    assert!(!out.contains("test.web.legacy-80"), "{out}");
    assert!(
        out.contains("Not checked: namespace web, whose services you may not list."),
        "{out}"
    );
    assert!(out.contains("Deleted test.shop.old-80"), "{out}");
    assert_eq!(
        forward_ids(&home.db()).0,
        ["test.shop.api-80", "test.web.legacy-80"]
    );

    let output = home.run(&["prune", "-y", "--context", "test"]);
    assert_eq!(
        stdout(&output),
        "Nothing to prune in context test.\n\
         Not checked: namespace web, whose services you may not list.\n"
    );
}

#[test]
fn a_kubeconfig_file_that_does_not_exist_is_skipped() {
    let mut home = Home::new();
    home.set_kubeconfig(&kubeconfig(fake_api()));
    // A stale entry in KUBECONFIG, as kubectl tolerates it.
    let gone = home.path().join("gone");
    let file = home.kubeconfig_file();
    home.set_kubeconfig_files(&[gone.clone(), file.clone()]);
    write_database(&home.db(), &[forward("test", "shop", "old", false)], false);

    // The current context and its client come from the file that exists.
    let output = home.run(&["prune", "-y"]);
    assert!(output.status.success(), "{output:?}");
    let out = stdout(&output);
    assert!(
        out.contains("in context test (the daemon's current context)"),
        "{out}"
    );
    assert!(out.contains("Deleted test.shop.old-80"), "{out}");

    let out = stdout(&home.run(&["status"]));
    assert!(
        out.contains(&format!(
            "kubeconfig: {} (missing), {}\n",
            gone.display(),
            file.display()
        )),
        "{out}"
    );
}

/// Waits for a line of `lines` that ends with `end`.
fn wait_for_line(lines: &mpsc::Receiver<String>, end: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match lines.recv_timeout(left) {
            Ok(line) if line.ends_with(end) => return,
            Ok(_) => {}
            Err(_) => panic!("no line ending with {end:?}"),
        }
    }
}

#[test]
fn logs_show_the_daemon_log_and_the_events_of_forwards() {
    let home = Home::new();
    write_database(&home.db(), &[forward("test", "shop", "api", true)], false);
    home.start_daemon();

    // The daemon writes its log in the background.
    let started = Instant::now();
    while !stdout(&home.run(&["logs"])).contains("daemon started") {
        assert!(started.elapsed() < Duration::from_secs(10), "no daemon log");
        std::thread::sleep(Duration::from_millis(100));
    }

    let output = home.run(&["logs", "api"]);
    assert!(output.status.success(), "{output:?}");
    let out = stdout(&output);
    assert!(
        out.lines()
            .any(|l| l.ends_with("  test.shop.api-80  started")),
        "{out}"
    );
    // The context's events come along.
    assert!(
        out.lines().any(|l| {
            let words: Vec<&str> = l.split_whitespace().collect();
            words.get(1) == Some(&"test") && words.get(2) == Some(&"kubeconfig:")
        }),
        "{out}"
    );
    assert_eq!(home.run(&["logs", "redis"]).status.code(), Some(1));

    // -f keeps printing.
    let mut follow = home
        .command(&["logs", "-f", "api"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (tx, lines) = mpsc::channel();
    let reader = BufReader::new(follow.stdout.take().unwrap());
    std::thread::spawn(move || {
        for line in reader.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    wait_for_line(&lines, "started");
    assert!(home.run(&["down", "api"]).status.success());
    wait_for_line(&lines, "stopped");
    follow.kill().unwrap();
    follow.wait().unwrap();

    // A reader that goes away, as `| head` does, ends it quietly at the
    // next event.
    let mut follow = home
        .command(&["logs", "-f", "api"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(follow.stdout.take());
    assert!(home.run(&["up", "api"]).status.success());
    let (status, err) = exit_of(follow);
    assert_eq!(status.code(), Some(0), "{err}");
    assert_eq!(err, "");
}

/// The exit status and stderr of `child`, which must exit within 10 s.
fn exit_of(mut child: Child) -> (ExitStatus, String) {
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(10) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("still running after 10 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut err = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        stderr.read_to_string(&mut err).unwrap();
    }
    (status, err)
}

/// `logs -f` follows the daemon log by the bytes it read, so it needs no
/// daemon; one is never started here.
#[test]
fn logs_follow_the_daemon_log_until_the_reader_goes_away() {
    let home = Home::new();
    let logs = home.data_dir().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let append = |name: &str, text: &str| {
        std::fs::File::options()
            .create(true)
            .append(true)
            .open(logs.join(name))
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    };
    append("daemon.2026-11-06.log", "monday\n");

    let mut follow = home
        .command(&["logs", "-f"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(follow.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line, "monday\n");

    // A line in two writes, the rest of the old file, then the new day's.
    append("daemon.2026-11-06.log", "late mon");
    std::thread::sleep(Duration::from_millis(700));
    append("daemon.2026-11-06.log", "day\n");
    append("daemon.2026-11-07.log", "tuesday\n");
    for expected in ["late monday\n", "tuesday\n"] {
        line.clear();
        out.read_line(&mut line).unwrap();
        assert_eq!(line, expected);
    }

    // The reader goes away, as with `| head`.
    drop(out);
    append("daemon.2026-11-07.log", "unread\n");
    let (status, err) = exit_of(follow);
    assert_eq!(status.code(), Some(0), "{err}");
    assert_eq!(err, "");
    assert!(!home.socket().exists(), "a daemon was started");
}
