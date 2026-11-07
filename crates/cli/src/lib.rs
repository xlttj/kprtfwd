//! The kprtfwd command line: everything except the TUI and `daemon run`.

mod json;
mod log_file;
mod table;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use kprtfwd_core::clock::local_date_time;
use kprtfwd_core::{ContextState, ForwardId, ForwardState, Store, filter, paths};
use kprtfwd_proto::{
    Client, ClientError, ForwardView, Request, Snapshot, connect_or_start, other_version,
    stop_daemon,
};

use crate::table::print_table;

/// Exit code of `status` when an enabled forward is not ready. 2 is clap's,
/// for wrong usage.
const EXIT_NOT_READY: u8 = 3;
/// Exit code when the daemon is not reachable: not running and not
/// startable, not answering, or gone during the command.
const EXIT_NO_DAEMON: u8 = 4;
/// How long `up` waits for forwards to come up; see [`settling`].
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
        /// Print the contexts and forwards as JSON, in the format of the README
        #[arg(long)]
        json: bool,
    },
    /// Switch forwards on: a project name, or words that must all match
    Up {
        #[arg(required = true, value_parser = word)]
        query: Vec<String>,
    },
    /// Switch forwards off: a project name, or words that must all match
    #[command(group = clap::ArgGroup::new("forwards").required(true).args(["all", "query"]))]
    Down {
        /// Switch every forward off
        #[arg(long)]
        all: bool,
        #[arg(value_parser = word)]
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
    /// Show the events of forwards (a project or words, as for `up`), or
    /// without arguments the daemon's log
    Logs {
        /// Keep printing new entries
        #[arg(short, long)]
        follow: bool,
        #[arg(value_parser = word)]
        query: Vec<String>,
    },
    /// Delete forwards whose service no longer exists in the cluster
    Prune {
        /// Kubeconfig context [default: the current context]
        #[arg(long)]
        context: Option<String>,
        /// Namespace filter with `*` wildcards, e.g. 'my-app-*'
        #[arg(long, default_value = "*")]
        namespace: String,
        /// Delete without asking
        #[arg(short = 'y')]
        yes: bool,
    },
    /// Show or control the daemon
    #[command(subcommand)]
    Daemon(DaemonCommand),
    /// Inspect the database directly, without the daemon
    #[command(subcommand)]
    Db(DbCommand),
}

/// A word of a query. An empty one, as an unset variable in a script gives,
/// would match every forward, so it is wrong usage.
fn word(text: &str) -> Result<String, String> {
    if text.trim().is_empty() {
        return Err("a word must not be empty".into());
    }
    Ok(text.to_owned())
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
        /// Database file [default: ~/.config/kprtfwd/kprtfwd.db]
        #[arg(long)]
        db: Option<PathBuf>,
    },
}

/// Runs a command; `daemon run` is handled by the binary.
pub async fn run(command: Command) -> ExitCode {
    match execute(command).await {
        Ok(code) => code,
        Err(err) if err.is::<StdoutClosed>() => ExitCode::SUCCESS,
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
            ExitCode::from(exit_code(&err))
        }
    }
}

/// Its reader closed stdout, e.g. `| head`: the command ends quietly.
#[derive(Debug)]
struct StdoutClosed;

impl std::fmt::Display for StdoutClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stdout closed")
    }
}

impl std::error::Error for StdoutClosed {}

/// Writes `bytes` to stdout at once. A closed stdout is [`StdoutClosed`],
/// not a panic as with `print!`.
fn write_stdout(bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(bytes).and_then(|()| stdout.flush()) {
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Err(StdoutClosed.into()),
        result => Ok(result?),
    }
}

/// [`StdoutClosed`] if the reader of stdout went away: a pipe whose other
/// end is closed polls as hung up (macOS) or failed (Linux). Lets
/// `logs -f | head` end while nothing new is written; a terminal or a file
/// never counts as closed.
fn check_stdout() -> Result<()> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let stdout = std::io::stdout();
    let mut fds = [PollFd::new(&stdout, PollFlags::OUT)];
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let closed = poll(&mut fds, Some(&now)).is_ok()
        && fds[0].revents().intersects(PollFlags::ERR | PollFlags::HUP);
    if closed {
        return Err(StdoutClosed.into());
    }
    Ok(())
}

/// [`EXIT_NO_DAEMON`] if the daemon could not be reached: it is not running
/// and could not be started, it does not answer, or the connection failed or
/// closed during the command. 1 for any other error, also for a daemon of
/// another protocol: it answers, and a restart helps, not a start.
fn exit_code(err: &anyhow::Error) -> u8 {
    let no_daemon = err.downcast_ref::<ClientError>().is_some_and(|err| {
        matches!(
            err,
            ClientError::NotRunning(_)
                | ClientError::StartTimeout(_)
                | ClientError::StartFailed(_)
                | ClientError::NoAnswer(_)
                | ClientError::Closed
                | ClientError::Io(_)
        )
    });
    if no_daemon { EXIT_NO_DAEMON } else { 1 }
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
        Command::Prune {
            context,
            namespace,
            yes,
        } => prune(context, &namespace, yes).await,
        Command::Logs { follow, query } if query.is_empty() => daemon_log(follow).await,
        Command::Logs { follow, query } => events(&query, follow).await,
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
    paths::usable_socket_path().map_err(anyhow::Error::msg)
}

/// Connects to the daemon, starting it if needed.
async fn connect() -> Result<Client> {
    let exe = std::env::current_exe().context("find the kprtfwd binary")?;
    let client = connect_or_start(&socket()?, &exe).await?;
    warn_of_other_version(&client);
    Ok(client)
}

/// Tells on stderr, so output for scripts stays as it is, when the daemon
/// runs another version than this kprtfwd.
fn warn_of_other_version(client: &Client) {
    if let Some(hint) = other_version(&client.daemon_version) {
        eprintln!("kprtfwd: {hint}");
    }
}

async fn status(all: bool, json: bool) -> Result<ExitCode> {
    let snapshot = connect().await?.snapshot().await?;
    let shown: Vec<&ForwardView> = snapshot
        .forwards
        .iter()
        .filter(|view| all || view.forward.enabled)
        .collect();
    if json {
        let mut text = serde_json::to_string_pretty(&status_json(&snapshot, &shown))?;
        text.push('\n');
        write_stdout(text.as_bytes())?;
    } else {
        print_contexts(&snapshot);
        if shown.is_empty() {
            println!("No forward is on. `kprtfwd up <project or words>` switches some on.");
        } else {
            print_forwards(&snapshot, &shown);
        }
        if snapshot.paused {
            println!("\nPaused. `kprtfwd resume` brings the enabled forwards back.");
        }
        println!("\nkubeconfig: {}", snapshot.kubeconfig_files.join(", "));
    }
    Ok(if all_ready(&snapshot) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_NOT_READY)
    })
}

/// What `status --json` prints: the contexts and `shown`, with the texts of
/// the table.
fn status_json<'a>(snapshot: &'a Snapshot, shown: &[&'a ForwardView]) -> json::Status<'a> {
    json::Status {
        version: json::VERSION,
        paused: snapshot.paused,
        contexts: snapshot
            .contexts
            .iter()
            .map(|context| json::Context {
                name: &context.name,
                state: context.state.summary(),
            })
            .collect(),
        forwards: shown
            .iter()
            .map(|view| {
                let forward = &view.forward;
                json::Forward {
                    id: forward.id.as_str(),
                    context: &forward.context,
                    namespace: &forward.namespace,
                    service: &forward.service,
                    remote_port: forward.remote_port,
                    local_port: forward.local_port,
                    enabled: forward.enabled,
                    state: view.state.label(),
                    info: info(view, &snapshot.context_state(&forward.context)),
                }
            })
            .collect(),
    }
}

/// Whether every enabled forward is ready; a paused one counts as ready.
fn all_ready(snapshot: &Snapshot) -> bool {
    snapshot.forwards.iter().all(|view| {
        !view.forward.enabled
            || matches!(
                view.state,
                ForwardState::Ready { .. } | ForwardState::Paused
            )
    })
}

/// What `up` and `down` act on.
enum Selection {
    Project(String),
    Forwards(Vec<ForwardId>),
}

/// The project or forwards that `query` names. An empty query, which would
/// match every forward, selects nothing.
fn select(snapshot: &Snapshot, query: &[String]) -> Result<Selection> {
    if query.iter().all(|word| word.trim().is_empty()) {
        bail!("no project or words given");
    }
    if let [name] = query
        && snapshot.projects.iter().any(|p| &p.name == name)
    {
        return Ok(Selection::Project(name.clone()));
    }
    let query = query.join(" ");
    let ids: Vec<ForwardId> = snapshot
        .forwards
        .iter()
        .filter(|view| {
            filter::matches_with(&query, &view.forward, |project| {
                snapshot
                    .projects
                    .iter()
                    .any(|p| p.name == project && p.forwards.contains(&view.forward.id))
            })
        })
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

/// Waits until the forwards have come up, then shows them.
async fn report(client: &mut Client, ids: &[ForwardId]) -> Result<ExitCode> {
    let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
    let snapshot = loop {
        let snapshot = client.snapshot().await?;
        let settling = snapshot
            .forwards
            .iter()
            .any(|view| ids.contains(&view.forward.id) && settling(&snapshot, view));
        if !settling || tokio::time::Instant::now() >= deadline {
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

/// Whether a forward is still coming up: its listener or cache is not ready,
/// or its context is connecting. A login may wait minutes for the browser,
/// so it is not waited for.
fn settling(snapshot: &Snapshot, view: &ForwardView) -> bool {
    match view.state {
        ForwardState::Starting => true,
        ForwardState::WaitingForContext => matches!(
            snapshot.context_state(&view.forward.context),
            ContextState::Idle | ContextState::Connecting
        ),
        _ => false,
    }
}

/// Lists the forwards whose service is gone, asks, and deletes exactly those
/// the user saw; the daemon spares any whose service is back by then.
async fn prune(context: Option<String>, namespace: &str, yes: bool) -> Result<ExitCode> {
    let mut client = connect().await?;
    let defaulted = context.is_none();
    let context = match context {
        Some(context) => context,
        None => match client.request(Request::ListContexts).await? {
            kprtfwd_proto::Response::Contexts {
                current: Some(current),
                ..
            } => current,
            _ => bail!("no current context in the kubeconfig; pass --context"),
        },
    };
    let scope = prune_scope(&context, defaulted, namespace);
    let request = |delete| Request::Prune {
        context: context.clone(),
        namespace_filter: namespace.to_owned(),
        delete,
    };
    let (gone, skipped) = match client.request(request(None)).await? {
        kprtfwd_proto::Response::Pruned { forwards, skipped } => (forwards, skipped),
        _ => bail!("unexpected answer from the daemon"),
    };
    if gone.is_empty() {
        println!("Nothing to prune in {scope}.");
        if let Some(note) = skipped_note(&skipped) {
            println!("{note}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    println!("Forwards whose service is gone, in {scope}:\n");
    let rows: Vec<Vec<String>> = gone
        .iter()
        .map(|f| {
            vec![
                format!("{}/{}:{}", f.namespace, f.service, f.remote_port),
                f.local_port.to_string(),
                f.id.to_string(),
            ]
        })
        .collect();
    print_table(&["SERVICE", "LOCAL", "ID"], &rows);
    if let Some(note) = skipped_note(&skipped) {
        println!("\n{note}");
    }
    let question = match gone.len() {
        1 => format!("\nDelete this forward of context {context}? [y/N] "),
        n => format!("\nDelete these {n} forwards of context {context}? [y/N] "),
    };
    if !yes && !confirm(&question)? {
        return Ok(ExitCode::SUCCESS);
    }
    let confirmed: Vec<ForwardId> = gone.into_iter().map(|f| f.id).collect();
    let deleted = match client.request(request(Some(confirmed.clone()))).await? {
        kprtfwd_proto::Response::Pruned { forwards, .. } => forwards,
        _ => bail!("unexpected answer from the daemon"),
    };
    for forward in &deleted {
        println!("Deleted {}", forward.id);
    }
    for id in confirmed
        .iter()
        .filter(|id| !deleted.iter().any(|f| &f.id == *id))
    {
        println!(
            "Not deleted: {id} (its service is back or cannot be checked, or the forward changed)"
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Names the namespaces prune left alone because the user may not list
/// their services; `None` if there are none.
fn skipped_note(skipped: &[String]) -> Option<String> {
    match skipped {
        [] => None,
        [namespace] => Some(format!(
            "Not checked: namespace {namespace}, whose services you may not list."
        )),
        _ => Some(format!(
            "Not checked: {} namespaces whose services you may not list: {}.",
            skipped.len(),
            skipped.join(", ")
        )),
    }
}

/// Names what `prune` works on: the context, whether it is the default, and
/// the namespace filter.
fn prune_scope(context: &str, defaulted: bool, namespace_filter: &str) -> String {
    let mut scope = format!("context {context}");
    if defaulted {
        scope.push_str(" (the daemon's current context)");
    }
    let filter = namespace_filter.trim();
    if !filter.is_empty() && filter != "*" {
        scope.push_str(&format!(", namespaces {filter}"));
    }
    scope
}

fn confirm(question: &str) -> Result<bool> {
    use std::io::Write;
    print!("{question}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// Prints the events of the selected forwards and their contexts.
async fn events(query: &[String], follow: bool) -> Result<ExitCode> {
    let mut client = connect().await?;
    let snapshot = client.snapshot().await?;
    let ids = match select(&snapshot, query)? {
        Selection::Project(name) => project_members(&snapshot, &name),
        Selection::Forwards(ids) => ids,
    };
    let width = snapshot
        .forwards
        .iter()
        .filter(|view| ids.contains(&view.forward.id))
        .flat_map(|view| [view.forward.id.as_str().len(), view.forward.context.len()])
        .max()
        .unwrap_or(0);
    let mut printed: HashSet<(SystemTime, String, String)> = HashSet::new();
    loop {
        let entries = match client.request(Request::Events { ids: ids.clone() }).await? {
            kprtfwd_proto::Response::Events { entries } => entries,
            _ => bail!("unexpected answer from the daemon"),
        };
        let mut lines = String::new();
        for entry in entries {
            let key = (entry.at, entry.source.clone(), entry.message.clone());
            if printed.insert(key) {
                lines.push_str(&format!(
                    "{}  {:<width$}  {}\n",
                    local_date_time(entry.at),
                    entry.source,
                    entry.message
                ));
            }
        }
        write_stdout(lines.as_bytes())?;
        if !follow {
            return Ok(ExitCode::SUCCESS);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        check_stdout()?;
    }
}

/// Prints the end of the daemon's newest log file, and with `follow` what
/// is appended, across the daily switch to a new file.
async fn daemon_log(follow: bool) -> Result<ExitCode> {
    const TAIL: usize = 50;
    let dir = paths::log_dir().context("neither KPRTFWD_HOME nor HOME is set")?;
    let mut log = log_file::Follower::new(&dir)
        .with_context(|| format!("no daemon log in {}", dir.display()))?;
    let mut text = log.read()?;
    if !follow {
        text.append(&mut log.rest());
    }
    write_stdout(log_file::last_lines(&text, TAIL))?;
    if !follow {
        return Ok(ExitCode::SUCCESS);
    }
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        check_stdout()?;
        let lines = log.read()?;
        if !lines.is_empty() {
            write_stdout(&lines)?;
        }
    }
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
            warn_of_other_version(&client);
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
    let pid_file = paths::data_dir()
        .context("neither KPRTFWD_HOME nor HOME is set")?
        .join("daemon.pid");
    if stop_daemon(&socket()?, &pid_file).await? {
        println!("daemon stopped");
    } else {
        println!("daemon not running");
    }
    Ok(ExitCode::SUCCESS)
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
fn info(view: &ForwardView, context: &ContextState) -> String {
    let reconnect = format!("kprtfwd reconnect {}", view.forward.context);
    let text = view.state.info(context, &reconnect);
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
    use kprtfwd_core::Forward;
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
            kubeconfig_files: vec![],
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
    fn an_empty_query_selects_nothing() {
        for query in [vec![], vec![String::new()], vec![" ".into(), "\t".into()]] {
            let err = select(&snapshot(), &query).err().expect("an error");
            assert_eq!(err.to_string(), "no project or words given", "{query:?}");
        }
        assert!(word("").is_err());
        assert!(word("  ").is_err());
        assert_eq!(word("api").unwrap(), "api");
    }

    #[test]
    fn down_needs_words_or_all() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(subcommand)]
            command: Command,
        }
        let parse = |args: &[&str]| {
            use clap::Parser;
            Cli::try_parse_from([&["kprtfwd"], args].concat()).map(|cli| cli.command)
        };
        for usage in [
            &["down"][..],
            &["down", ""],
            &["down", "--all", "api"],
            &["up"],
            &["up", " "],
            &["logs", ""],
        ] {
            let err = parse(usage).err().expect("wrong usage");
            assert_eq!(err.exit_code(), 2, "{usage:?}: {err}");
        }
        assert!(matches!(
            parse(&["down", "--all"]),
            Ok(Command::Down { all: true, .. })
        ));
        assert!(matches!(
            parse(&["down", "shop", "api"]),
            Ok(Command::Down { all: false, query }) if query == ["shop", "api"]
        ));
        assert!(matches!(parse(&["logs"]), Ok(Command::Logs { query, .. }) if query.is_empty()));
    }

    #[test]
    fn prune_names_the_namespaces_it_could_not_check() {
        assert_eq!(skipped_note(&[]), None);
        assert_eq!(
            skipped_note(&["web".into()]).unwrap(),
            "Not checked: namespace web, whose services you may not list."
        );
        assert_eq!(
            skipped_note(&["kube-system".into(), "web".into()]).unwrap(),
            "Not checked: 2 namespaces whose services you may not list: kube-system, web."
        );
    }

    #[test]
    fn prune_names_the_context_and_the_namespace_filter() {
        assert_eq!(prune_scope("staging", false, "*"), "context staging");
        assert_eq!(
            prune_scope("staging", true, ""),
            "context staging (the daemon's current context)"
        );
        assert_eq!(
            prune_scope("staging", false, " shop-* "),
            "context staging, namespaces shop-*"
        );
    }

    /// The format scripts rely on; a change here needs a new version.
    #[test]
    fn status_json_has_a_fixed_format() {
        let mut snapshot = snapshot();
        snapshot.contexts = vec![kprtfwd_proto::ContextView {
            name: "staging".into(),
            state: ContextState::Unreachable {
                since: SystemTime::UNIX_EPOCH,
                cause: kprtfwd_core::state::NetCause::Dns,
            },
        }];
        snapshot.forwards[0].forward.enabled = true;
        snapshot.forwards[0].state = ForwardState::WaitingForContext;
        let shown: Vec<&ForwardView> = snapshot.forwards[..2].iter().collect();
        let text = serde_json::to_string_pretty(&status_json(&snapshot, &shown)).unwrap();
        assert_eq!(
            text,
            r#"{
  "version": 1,
  "paused": false,
  "contexts": [
    {
      "name": "staging",
      "state": "unreachable (DNS)"
    }
  ],
  "forwards": [
    {
      "id": "stg-api",
      "context": "staging",
      "namespace": "shop",
      "service": "api",
      "remote_port": 80,
      "local_port": 18080,
      "enabled": true,
      "state": "waiting",
      "info": "API unreachable (DNS)"
    },
    {
      "id": "stg-db",
      "context": "staging",
      "namespace": "shop",
      "service": "mysql",
      "remote_port": 80,
      "local_port": 18080,
      "enabled": false,
      "state": "off",
      "info": ""
    }
  ]
}"#
        );
    }

    /// The CLI has no keys: a needed login points at the command.
    #[test]
    fn a_needed_login_points_at_kprtfwd_reconnect() {
        let mut waiting = view("stg-api", "staging", "api");
        waiting.state = ForwardState::WaitingForContext;
        let auth = ContextState::AuthFailed {
            since: SystemTime::UNIX_EPOCH,
            message: "token expired".into(),
        };
        assert_eq!(
            info(&waiting, &auth),
            "login needed: kprtfwd reconnect staging to log in"
        );
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

    #[test]
    fn only_an_unreachable_daemon_exits_with_4() {
        use std::io::{Error, ErrorKind};
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(EXIT_NO_DAEMON, 4);
        let no_daemon = [
            ClientError::NotRunning(Error::from(ErrorKind::NotFound)),
            ClientError::StartTimeout(Duration::from_secs(10)),
            ClientError::StartFailed(std::process::ExitStatus::from_raw(256)),
            ClientError::NoAnswer(Duration::from_secs(5)),
            // The daemon went away during the command.
            ClientError::Closed,
            ClientError::Io(Error::from(ErrorKind::ConnectionReset)),
            ClientError::Io(Error::from(ErrorKind::BrokenPipe)),
            ClientError::Io(Error::from(ErrorKind::PermissionDenied)),
        ];
        for err in no_daemon {
            let text = format!("{err:?}");
            assert_eq!(exit_code(&err.into()), EXIT_NO_DAEMON, "{text}");
        }
        let wrapped = anyhow::Error::from(ClientError::Closed).context("snapshot");
        assert_eq!(exit_code(&wrapped), EXIT_NO_DAEMON);

        let others = [
            anyhow::Error::from(ClientError::Daemon("unknown forward".into())),
            anyhow::Error::from(ClientError::Incompatible),
            anyhow::Error::from(ClientError::ProtocolMismatch {
                daemon: 1,
                client: 2,
                daemon_version: "1.0.0".into(),
                pid: 1,
            }),
            anyhow::anyhow!("no project or forward matches \"redis\""),
            // Not from talking to the daemon, e.g. reading its log.
            anyhow::Error::from(Error::from(ErrorKind::PermissionDenied)),
        ];
        for err in others {
            assert_eq!(exit_code(&err), 1, "{err}");
        }
    }

    #[test]
    fn status_is_ready_when_every_enabled_forward_is_ready_or_paused() {
        let with = |states: &[(bool, ForwardState)]| {
            let mut snapshot = snapshot();
            snapshot.forwards = states
                .iter()
                .map(|(enabled, state)| {
                    let mut view = view("stg-api", "staging", "api");
                    view.forward.enabled = *enabled;
                    view.state = state.clone();
                    view
                })
                .collect();
            all_ready(&snapshot)
        };
        let ready = ForwardState::Ready {
            pod: None,
            active_conns: 0,
        };
        let waiting = ForwardState::WaitingForPods { ready: 0, total: 1 };
        assert!(with(&[]));
        assert!(with(&[(true, ready.clone()), (true, ForwardState::Paused)]));
        assert!(with(&[
            (false, ForwardState::Disabled),
            (true, ready.clone())
        ]));
        for not_ready in [
            ForwardState::Starting,
            waiting,
            ForwardState::WaitingForContext,
            ForwardState::PortBusy { holder: None },
        ] {
            assert!(
                !with(&[(true, ready.clone()), (true, not_ready.clone())]),
                "{not_ready:?}"
            );
        }
    }

    /// `up` waits for a forward whose context is still connecting, as for
    /// one that is starting, but not for a login or an outage.
    #[test]
    fn up_waits_while_a_forward_starts_or_its_context_connects() {
        let mut snapshot = snapshot();
        let mut view = view("stg-api", "staging", "api");
        let with = |snapshot: &mut Snapshot, view: &mut ForwardView, state, context| {
            view.state = state;
            snapshot.contexts = vec![kprtfwd_proto::ContextView {
                name: "staging".into(),
                state: context,
            }];
            settling(snapshot, view)
        };
        let healthy = ContextState::Healthy {
            server_version: "v1".into(),
        };
        let down = ContextState::Unreachable {
            since: SystemTime::UNIX_EPOCH,
            cause: kprtfwd_core::state::NetCause::Dns,
        };
        let waiting = || ForwardState::WaitingForContext;
        assert!(with(
            &mut snapshot,
            &mut view,
            ForwardState::Starting,
            healthy.clone()
        ));
        for context in [ContextState::Connecting, ContextState::Idle] {
            assert!(with(&mut snapshot, &mut view, waiting(), context));
        }
        for context in [ContextState::LoggingIn, down] {
            assert!(!with(&mut snapshot, &mut view, waiting(), context));
        }
        let pods = ForwardState::WaitingForPods { ready: 0, total: 1 };
        assert!(!with(&mut snapshot, &mut view, pods, healthy));
    }
}
