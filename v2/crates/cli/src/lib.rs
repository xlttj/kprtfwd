//! The kprtfwd command line: everything except the TUI and `daemon run`.

mod table;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use kprtfwd_core::{ForwardId, ForwardState, Store, filter, paths};
use kprtfwd_proto::{Client, ClientError, ForwardView, Request, Snapshot, connect_or_start};

use crate::table::print_table;

/// Exit code when the daemon is not reachable.
const EXIT_NO_DAEMON: u8 = 2;
/// Exit code of `status` when an enabled forward is not ready.
const EXIT_NOT_READY: u8 = 3;
/// How long `up` waits for forwards to leave "starting".
const SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
/// Errors older than this are not shown next to a ready forward.
const RECENT_ERROR: Duration = Duration::from_secs(60);

#[derive(Subcommand)]
pub enum Command {
    /// Show the state of contexts and enabled forwards
    Status {
        /// Include forwards that are off
        #[arg(long)]
        all: bool,
        /// Print the full state as JSON
        #[arg(long)]
        json: bool,
    },
    /// Switch forwards on: a project name, or words that must all match
    Up {
        #[arg(required = true)]
        query: Vec<String>,
    },
    /// Switch forwards off: a project name, or words that must all match
    Down {
        /// Switch every forward off
        #[arg(long, conflicts_with = "query")]
        all: bool,
        query: Vec<String>,
    },
    /// Switch a project's forwards on and all others off
    Switch {
        project: String,
    },
    /// Close all listeners; `resume` brings the enabled forwards back
    Pause,
    Resume,
    /// Reconnect a context from scratch
    Reconnect {
        context: String,
    },
    /// Show or control the daemon
    #[command(subcommand)]
    Daemon(DaemonCommand),
    /// Inspect the database directly, without the daemon
    #[command(subcommand)]
    Db(DbCommand),
}

#[derive(Subcommand)]
pub enum DaemonCommand {
    /// Run the daemon (started on demand; run it yourself to debug)
    Run {
        /// Log to the terminal instead of the log directory
        #[arg(long)]
        foreground: bool,
    },
    Status,
    Stop,
    Restart,
}

#[derive(Subcommand)]
pub enum DbCommand {
    /// List forwards and projects; reads a copy, the file is not changed
    List {
        /// Database file [default: ~/.kprtfwd/kprtfwd.db]
        #[arg(long)]
        db: Option<PathBuf>,
    },
}

/// Runs a command; `daemon run` is handled by the binary.
pub async fn run(command: Command) -> ExitCode {
    match execute(command).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("kprtfwd: {err:#}");
            let start_failed = err.downcast_ref::<ClientError>().is_some_and(|err| {
                matches!(
                    err,
                    ClientError::StartFailed(_) | ClientError::StartTimeout(_)
                )
            });
            if start_failed && let Some(logs) = paths::log_dir() {
                eprintln!("log: {}", logs.display());
            }
            let no_daemon = err.downcast_ref::<ClientError>().is_some_and(|err| {
                matches!(
                    err,
                    ClientError::NotRunning(_)
                        | ClientError::StartTimeout(_)
                        | ClientError::StartFailed(_)
                )
            });
            ExitCode::from(if no_daemon { EXIT_NO_DAEMON } else { 1 })
        }
    }
}

async fn execute(command: Command) -> Result<ExitCode> {
    match command {
        Command::Status { all, json } => status(all, json).await,
        Command::Up { query } => switch(&query, true).await,
        Command::Down { all: true, .. } => switch_all_off().await,
        Command::Down { query, .. } => switch(&query, false).await,
        Command::Switch { project } => switch_project(&project).await,
        Command::Pause => pause(true).await,
        Command::Resume => pause(false).await,
        Command::Reconnect { context } => {
            connect()
                .await?
                .act(Request::ReconnectContext { name: context })
                .await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Daemon(DaemonCommand::Run { .. }) => {
            bail!("`daemon run` is handled by the kprtfwd binary")
        }
        Command::Daemon(DaemonCommand::Status) => daemon_status().await,
        Command::Daemon(DaemonCommand::Stop) => daemon_stop().await,
        Command::Daemon(DaemonCommand::Restart) => {
            daemon_stop().await?;
            let client = connect().await?;
            println!(
                "daemon {} running (pid {})",
                client.daemon_version, client.daemon_pid
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Db(DbCommand::List { db }) => db_list(db),
    }
}

fn socket() -> Result<PathBuf> {
    // macOS allows 103 bytes for a Unix socket path, Linux 107.
    const MAX_SOCKET_PATH: usize = 103;
    let path = paths::socket_path().context("neither KPRTFWD_HOME nor HOME is set")?;
    if path.as_os_str().len() > MAX_SOCKET_PATH {
        bail!(
            "the socket path {} is too long for a Unix socket; set KPRTFWD_HOME to a shorter directory",
            path.display()
        );
    }
    Ok(path)
}

/// Connects to the daemon, starting it if needed.
async fn connect() -> Result<Client> {
    let exe = std::env::current_exe().context("find the kprtfwd binary")?;
    Ok(connect_or_start(&socket()?, &exe).await?)
}

async fn status(all: bool, json: bool) -> Result<ExitCode> {
    let snapshot = connect().await?.snapshot().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    } else {
        print_contexts(&snapshot);
        let shown: Vec<&ForwardView> = snapshot
            .forwards
            .iter()
            .filter(|view| all || view.forward.enabled)
            .collect();
        if shown.is_empty() {
            println!("No forward is on. `kprtfwd up <project or words>` switches some on.");
        } else {
            print_forwards(&snapshot, &shown);
        }
        if snapshot.paused {
            println!("\nPaused. `kprtfwd resume` brings the enabled forwards back.");
        }
    }
    let not_ready = snapshot.forwards.iter().any(|view| {
        view.forward.enabled
            && !matches!(
                view.state,
                ForwardState::Ready { .. } | ForwardState::Paused
            )
    });
    Ok(if not_ready {
        ExitCode::from(EXIT_NOT_READY)
    } else {
        ExitCode::SUCCESS
    })
}

/// What `up` and `down` act on.
enum Selection {
    Project(String),
    Forwards(Vec<ForwardId>),
}

fn select(snapshot: &Snapshot, query: &[String]) -> Result<Selection> {
    if let [name] = query
        && snapshot.projects.iter().any(|p| &p.name == name)
    {
        return Ok(Selection::Project(name.clone()));
    }
    let query = query.join(" ");
    let ids: Vec<ForwardId> = snapshot
        .forwards
        .iter()
        .filter(|view| filter::matches(&query, &view.forward))
        .map(|view| view.forward.id.clone())
        .collect();
    if ids.is_empty() {
        bail!("no project or forward matches {query:?}");
    }
    Ok(Selection::Forwards(ids))
}

async fn switch(query: &[String], enabled: bool) -> Result<ExitCode> {
    let mut client = connect().await?;
    let snapshot = client.snapshot().await?;
    let ids = match select(&snapshot, query)? {
        Selection::Project(name) => {
            client
                .act(Request::SetProjectEnabled {
                    name: name.clone(),
                    enabled,
                })
                .await?;
            project_members(&snapshot, &name)
        }
        Selection::Forwards(ids) => {
            client
                .act(Request::SetEnabled {
                    ids: ids.clone(),
                    enabled,
                })
                .await?;
            ids
        }
    };
    report(&mut client, &ids).await
}

async fn switch_all_off() -> Result<ExitCode> {
    let mut client = connect().await?;
    let snapshot = client.snapshot().await?;
    let ids: Vec<ForwardId> = snapshot
        .forwards
        .iter()
        .filter(|view| view.forward.enabled)
        .map(|view| view.forward.id.clone())
        .collect();
    client
        .act(Request::SetEnabled {
            ids: ids.clone(),
            enabled: false,
        })
        .await?;
    println!("Switched {} forwards off.", ids.len());
    Ok(ExitCode::SUCCESS)
}

async fn switch_project(project: &str) -> Result<ExitCode> {
    let mut client = connect().await?;
    client
        .act(Request::SwitchProject {
            name: project.to_owned(),
        })
        .await?;
    let snapshot = client.snapshot().await?;
    let ids = project_members(&snapshot, project);
    report(&mut client, &ids).await
}

fn project_members(snapshot: &Snapshot, name: &str) -> Vec<ForwardId> {
    snapshot
        .projects
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.forwards.clone())
        .unwrap_or_default()
}

/// Waits until the forwards have left "starting", then shows them.
async fn report(client: &mut Client, ids: &[ForwardId]) -> Result<ExitCode> {
    let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
    let snapshot = loop {
        let snapshot = client.snapshot().await?;
        let starting = snapshot
            .forwards
            .iter()
            .any(|view| ids.contains(&view.forward.id) && view.state == ForwardState::Starting);
        if !starting || tokio::time::Instant::now() >= deadline {
            break snapshot;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let shown: Vec<&ForwardView> = snapshot
        .forwards
        .iter()
        .filter(|view| ids.contains(&view.forward.id))
        .collect();
    print_forwards(&snapshot, &shown);
    Ok(ExitCode::SUCCESS)
}

async fn pause(paused: bool) -> Result<ExitCode> {
    connect().await?.act(Request::SetPaused { paused }).await?;
    println!("{}", if paused { "Paused." } else { "Resumed." });
    Ok(ExitCode::SUCCESS)
}

async fn daemon_status() -> Result<ExitCode> {
    let socket = socket()?;
    match Client::connect(&socket).await {
        Ok(client) => {
            println!(
                "daemon {} running (pid {}), socket {}",
                client.daemon_version,
                client.daemon_pid,
                socket.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(ClientError::NotRunning(_)) => {
            println!("daemon not running");
            Ok(ExitCode::from(EXIT_NO_DAEMON))
        }
        Err(err) => Err(err.into()),
    }
}

async fn daemon_stop() -> Result<ExitCode> {
    let socket = socket()?;
    let mut client = match Client::connect(&socket).await {
        Ok(client) => client,
        Err(ClientError::NotRunning(_)) => {
            println!("daemon not running");
            return Ok(ExitCode::SUCCESS);
        }
        Err(err) => return Err(err.into()),
    };
    client.act(Request::Shutdown).await?;
    // The daemon removes its socket as the last step.
    for _ in 0..100 {
        if !socket.exists() {
            println!("daemon stopped");
            return Ok(ExitCode::SUCCESS);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!("the daemon did not stop within 5 s")
}

fn print_contexts(snapshot: &Snapshot) {
    let mut parts: Vec<(bool, String)> = snapshot
        .contexts
        .iter()
        .map(|c| {
            let idle = c.state == kprtfwd_core::ContextState::Idle;
            (idle, format!("{} {}", c.name, c.state.summary()))
        })
        .collect();
    // Contexts in use first; idle ones last.
    parts.sort_by_key(|(idle, _)| *idle);
    let line: Vec<String> = parts.into_iter().map(|(_, text)| text).collect();
    if !line.is_empty() {
        println!("{}\n", line.join(" · "));
    }
}

fn print_forwards(snapshot: &Snapshot, views: &[&ForwardView]) {
    let states: HashMap<&str, kprtfwd_core::ContextState> = snapshot
        .contexts
        .iter()
        .map(|c| (c.name.as_str(), c.state.clone()))
        .collect();
    let rows: Vec<Vec<String>> = views
        .iter()
        .map(|view| {
            let forward = &view.forward;
            let context = states
                .get(forward.context.as_str())
                .cloned()
                .unwrap_or(kprtfwd_core::ContextState::Idle);
            vec![
                view.state.label().to_owned(),
                forward.context.clone(),
                format!(
                    "{}/{}:{}",
                    forward.namespace, forward.service, forward.remote_port
                ),
                forward.local_port.to_string(),
                info(view, &context),
            ]
        })
        .collect();
    print_table(&["STATE", "CONTEXT", "SERVICE", "LOCAL", "INFO"], &rows);
}

/// Why a forward is not ready, or its last error if that is recent.
fn info(view: &ForwardView, context: &kprtfwd_core::ContextState) -> String {
    let text = view.state.info(&view.forward.context, context);
    if !text.is_empty() {
        return text;
    }
    match &view.last_error {
        Some(error)
            if SystemTime::now()
                .duration_since(error.at)
                .is_ok_and(|age| age < RECENT_ERROR) =>
        {
            error.message.clone()
        }
        _ => String::new(),
    }
}

fn db_list(db: Option<PathBuf>) -> Result<ExitCode> {
    let path = match db {
        Some(path) => path,
        None => paths::default_db_path().context("neither KPRTFWD_HOME nor HOME is set")?,
    };
    if !path.exists() {
        bail!("no database at {}", path.display());
    }
    let store = Store::snapshot(&path).with_context(|| format!("read {}", path.display()))?;
    let forwards = store.forwards()?;
    let projects = store.projects()?;

    let mut member_of: HashMap<&ForwardId, Vec<&str>> = HashMap::new();
    for project in &projects {
        for id in &project.forwards {
            member_of.entry(id).or_default().push(&project.name);
        }
    }
    let rows: Vec<Vec<String>> = forwards
        .iter()
        .map(|f| {
            vec![
                if f.enabled { "on" } else { "off" }.to_owned(),
                f.context.clone(),
                format!("{}/{}:{}", f.namespace, f.service, f.remote_port),
                f.local_port.to_string(),
                member_of
                    .get(&f.id)
                    .map(|names| names.join(","))
                    .unwrap_or_default(),
                f.id.to_string(),
            ]
        })
        .collect();
    print_table(
        &["STATE", "CONTEXT", "SERVICE", "LOCAL", "PROJECTS", "ID"],
        &rows,
    );

    if !projects.is_empty() {
        println!();
        let rows: Vec<Vec<String>> = projects
            .iter()
            .map(|p| vec![p.name.clone(), p.forwards.len().to_string()])
            .collect();
        print_table(&["PROJECT", "FORWARDS"], &rows);
    }

    let mut contexts: Vec<&str> = forwards.iter().map(|f| f.context.as_str()).collect();
    contexts.dedup();
    println!(
        "\n{}, {}, {} (read from {})",
        count(forwards.len(), "forward"),
        count(contexts.len(), "context"),
        count(projects.len(), "project"),
        path.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kprtfwd_core::{ContextState, Forward};
    use kprtfwd_proto::ProjectView;

    fn view(id: &str, context: &str, service: &str) -> ForwardView {
        ForwardView {
            forward: Forward {
                id: ForwardId::new(id),
                context: context.into(),
                namespace: "shop".into(),
                service: service.into(),
                remote_port: 80,
                local_port: 18080,
                enabled: false,
            },
            state: ForwardState::Disabled,
            last_error: None,
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            daemon_version: "test".into(),
            paused: false,
            contexts: vec![],
            forwards: vec![
                view("stg-api", "staging", "api"),
                view("stg-db", "staging", "mysql"),
                view("prd-api", "production", "api"),
            ],
            projects: vec![ProjectView {
                name: "api".into(),
                forwards: vec![ForwardId::new("prd-api")],
                enabled: 0,
            }],
        }
    }

    fn ids(selection: Selection) -> Vec<String> {
        match selection {
            Selection::Forwards(ids) => ids.iter().map(ToString::to_string).collect(),
            Selection::Project(name) => vec![format!("project {name}")],
        }
    }

    #[test]
    fn a_single_word_naming_a_project_selects_the_project() {
        let selection = select(&snapshot(), &["api".into()]).unwrap();
        assert_eq!(ids(selection), ["project api"]);
    }

    #[test]
    fn words_select_every_matching_forward() {
        let selection = select(&snapshot(), &["stag".into(), "api".into()]).unwrap();
        assert_eq!(ids(selection), ["stg-api"]);
        let selection = select(&snapshot(), &["staging".into()]).unwrap();
        assert_eq!(ids(selection), ["stg-api", "stg-db"]);
    }

    #[test]
    fn no_match_is_an_error() {
        assert!(select(&snapshot(), &["redis".into()]).is_err());
    }

    #[test]
    fn info_falls_back_to_a_recent_error() {
        let mut ready = view("stg-api", "staging", "api");
        ready.state = ForwardState::Ready {
            pod: None,
            active_conns: 0,
        };
        let healthy = ContextState::Healthy {
            server_version: "v1".into(),
        };
        assert_eq!(info(&ready, &healthy), "");
        ready.last_error = Some(kprtfwd_proto::LastError {
            at: SystemTime::now(),
            message: "connection refused".into(),
        });
        assert_eq!(info(&ready, &healthy), "connection refused");
        ready.last_error.as_mut().unwrap().at = SystemTime::UNIX_EPOCH;
        assert_eq!(info(&ready, &healthy), "");
    }
}
