//! The kprtfwd daemon: owns the forwards and serves clients over a Unix
//! socket. A client starts it on demand. It exits after a while with nothing
//! to do, and when the last TUI quits, unless `config.toml` says to keep the
//! forwards running.

use std::collections::HashSet;
use std::fs::{self, DirBuilder, File, TryLockError};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use kprtfwd_core::filter::wildcard;
use kprtfwd_core::naming::{forward_id, propose_local_port};
use kprtfwd_core::state::derive;
use kprtfwd_core::svctype::classify;
use kprtfwd_core::validate::{validate_forward, validate_settings};
use kprtfwd_core::{Config, ContextState, Forward, ForwardId, Project, Store, StoreError, paths};
use kprtfwd_engine::{Engine, discovery, kubeconfig, port_is_free};
use kprtfwd_proto::{
    Client, ClientError, ClientMessage, ContextView, DiscoveredPort, EventEntry, ForwardView,
    LastError, PROTOCOL_VERSION, ProjectView, Request, Response, ServerMessage, Snapshot,
    read_frame, stop_daemon, write_message,
};
use rustix::fs::Mode;
use rustix::process::{Resource, Rlimit, geteuid, getrlimit, setrlimit, setsid, umask};
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Exit after this long without enabled forwards and without clients.
const IDLE_EXIT: Duration = Duration::from_secs(10 * 60);
/// How long a daemon waits for the lock of the data directory while the
/// daemon that holds it does not answer, as long as clients wait for a start.
const LOCK_WAIT: Duration = Duration::from_secs(10);
/// kube-rs logs every failed request as an error; outages are reported once,
/// by the context state, instead.
const DEFAULT_LOG_FILTER: &str =
    "info,kube=warn,kube_client=off,kube_runtime=off,hyper=warn,tower=warn,rustls=warn";
/// The soft limit of open files the daemon raises its own to. macOS takes
/// at most `kern.maxfilesperproc`, which is 10240 or more.
const OPEN_FILES: u64 = 10_240;

pub struct Options {
    /// Log to stderr instead of the log directory.
    pub foreground: bool,
}

pub fn run(options: Options) -> Result<()> {
    // A daemon in the session of the terminal its client was started from
    // is stopped with SIGTTIN, as a whole process group, as soon as an exec
    // plugin reads from that terminal (`/dev/tty`) while the client runs.
    // In a session of its own it has no terminal, so such a read fails at
    // once. A daemon typed into an interactive shell leads its process group
    // and cannot leave the session; it stays, and ctrl+c ends it.
    let detached = (!options.foreground).then(setsid);
    let data_dir = paths::data_dir().context("neither KPRTFWD_HOME nor HOME is set")?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&data_dir)
        .with_context(|| format!("create {}", data_dir.display()))?;
    let narrowed = secure_data_dir(&data_dir, geteuid().as_raw())?;
    let _log_guard = init_logging(&options, &data_dir)?;
    if !options.foreground {
        log_panics();
    }
    match detached {
        Some(Ok(_)) => debug!("in a session of its own"),
        Some(Err(err)) => debug!(%err, "stays in the session it was started in"),
        None => {}
    }
    if let Some(mode) = narrowed {
        info!(dir = %data_dir.display(), from = format!("{mode:o}"), "data directory narrowed to 700");
    }
    raise_open_files_limit();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let locked = runtime.block_on(lock_data_dir(&data_dir));
    let result = match locked {
        Ok(Some(lock)) => {
            let result = runtime.block_on(serve(&data_dir));
            // Tasks still running hold the database and the listeners of
            // forwards; they end before the next daemon gets the lock.
            drop(runtime);
            drop(lock);
            result
        }
        Ok(None) => Ok(()),
        Err(err) => Err(err),
    };
    if let Err(err) = &result {
        tracing::error!("{err:#}");
    }
    result
}

/// Makes sure that only this user (`uid`) can reach the data directory:
/// it must be theirs, and its mode is narrowed to 0700. Another user who
/// could write there could replace the socket or plant a database, whose
/// forwards would then run with this user's kubeconfig credentials. A
/// directory created by hand, by a dotfiles manager or under `/tmp` for
/// `KPRTFWD_HOME` may have another mode. Returns the mode it had if it was
/// narrowed.
fn secure_data_dir(dir: &Path, uid: u32) -> Result<Option<u32>> {
    let metadata = fs::metadata(dir).with_context(|| format!("read {}", dir.display()))?;
    if metadata.uid() != uid {
        bail!(
            "{} belongs to uid {}, not to this user (uid {uid}); use a directory of your own",
            dir.display(),
            metadata.uid()
        );
    }
    let mode = metadata.mode() & 0o7777;
    if mode == 0o700 {
        return Ok(None);
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("narrow the mode of {} to 700", dir.display()))?;
    Ok(Some(mode))
}

/// Takes `daemon.lock` in the data directory, which a daemon holds from
/// before it touches the socket until it has exited, so two daemons never
/// share the database. While another daemon holds it, waits: one that is
/// starting answers on the socket soon, and this one gives way (`None`); one
/// that is exiting releases the lock soon. The file stays; removing it would
/// let a daemon lock a file the next one no longer finds.
async fn lock_data_dir(data_dir: &Path) -> Result<Option<File>> {
    let path = data_dir.join("daemon.lock");
    let socket = data_dir.join("daemon.sock");
    let file = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(err)) => {
                return Err(err).with_context(|| format!("lock {}", path.display()));
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        match Client::connect_within(&socket, left.max(Duration::from_millis(50))).await {
            Ok(other) => {
                info!(pid = other.daemon_pid, "another daemon is running");
                return Ok(None);
            }
            Err(err @ ClientError::ProtocolMismatch { .. }) => {
                bail!("a daemon answers on {}, but: {err}", socket.display())
            }
            // Not bound yet, or no longer, or not answering yet.
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            bail!(
                "another daemon holds {} but does not answer on {}",
                path.display(),
                socket.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn init_logging(
    options: &Options,
    data_dir: &Path,
) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    let filter = EnvFilter::try_from_env("KPRTFWD_LOG")
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    if options.foreground {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_timer(LocalTime)
            .with_target(false)
            .init();
        return Ok(None);
    }
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("daemon")
        .filename_suffix("log")
        .max_log_files(7)
        .build(data_dir.join("logs"))
        .context("open log directory")?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(LocalTime)
        .with_target(false)
        .with_ansi(false)
        .with_writer(writer)
        .init();
    Ok(Some(guard))
}

/// The log's times: local with the offset, so that they agree with the
/// times the TUI and `kprtfwd logs` show; tracing's default is UTC.
struct LocalTime;

impl tracing_subscriber::fmt::time::FormatTime for LocalTime {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        w.write_str(&kprtfwd_core::clock::log_timestamp())
    }
}

/// Logs panics, then runs the hook installed before, which prints them to
/// stderr: for a daemon started on demand a file next to the log that
/// nobody looks at first. A panic in a task ends only that task, like the
/// connection of a client or the watcher of the kubeconfig, and the daemon
/// goes on; without this, without a trace in its log.
fn log_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("{info}");
        previous(info);
    }));
}

/// Raises the soft limit of open files, which the daemon inherits from the
/// shell that started its client: 256 in a macOS terminal. Every forward
/// holds two listeners and every connection two sockets, so a few dozen
/// forwards and some browser tabs reach it. Child processes, exec plugins
/// and `lsof`, inherit the raised limit.
fn raise_open_files_limit() {
    let limit = getrlimit(Resource::Nofile);
    let Some(raised) = open_files_limit(limit.current, limit.maximum) else {
        debug!(soft = ?limit.current, hard = ?limit.maximum, "open files limit kept");
        return;
    };
    let from = limit.current.unwrap_or_default();
    let new = Rlimit {
        current: Some(raised),
        maximum: limit.maximum,
    };
    match setrlimit(Resource::Nofile, new) {
        Ok(()) => info!(from, to = raised, "open files limit raised"),
        Err(err) => warn!(%err, from, to = raised, "cannot raise the open files limit"),
    }
}

/// The soft limit of open files to set, given the current soft and hard
/// limits (`None` is unlimited): [`OPEN_FILES`], or the hard limit if that is
/// lower. `None` keeps the current limit, which is never lowered.
fn open_files_limit(soft: Option<u64>, hard: Option<u64>) -> Option<u64> {
    let wanted = hard.map_or(OPEN_FILES, |hard| hard.min(OPEN_FILES));
    soft.filter(|soft| *soft < wanted).map(|_| wanted)
}

async fn serve(data_dir: &Path) -> Result<()> {
    let socket = data_dir.join("daemon.sock");
    let Some(listener) = bind_socket(&socket).await? else {
        return Ok(());
    };
    // Removes the socket and the PID file however `serve` ends.
    let files = DaemonFiles {
        socket,
        pid: data_dir.join("daemon.pid"),
    };
    // Lets `kprtfwd daemon stop` end a daemon it cannot talk to: one of
    // another version, or one that does not answer, also while it starts.
    fs::write(&files.pid, format!("{}\n", std::process::id()))
        .with_context(|| format!("write {}", files.pid.display()))?;
    adopt_legacy_dir(data_dir).await?;
    let config_path = data_dir.join("config.toml");
    match Config::write_template(&config_path) {
        Ok(true) => info!(path = %config_path.display(), "wrote the default settings"),
        Ok(false) => {}
        Err(err) => warn!(%err, path = %config_path.display(), "cannot write the default settings"),
    }
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    // The terminal that started the daemon may close; that must not stop it.
    let mut hangup = signal(SignalKind::hangup())?;
    let store = Store::open(&data_dir.join("kprtfwd.db")).context("open the database")?;
    let daemon = Arc::new(Mutex::new(Daemon {
        store,
        engine: Engine::new(),
    }));
    // Starts nothing if it fails.
    daemon.lock().await.apply().await?;
    info!(
        version = VERSION,
        pid = std::process::id(),
        "daemon started"
    );
    tokio::spawn(watch_kubeconfig(daemon.clone()));

    let shutdown = Arc::new(Notify::new());
    let clients = Arc::new(AtomicUsize::new(0));
    let tuis = Arc::new(Tuis::default());
    let idle_exit = idle_exit(std::env::var("KPRTFWD_IDLE_EXIT_MS").ok());
    let mut idle_check = tokio::time::interval(idle_check(idle_exit));
    let mut idle_since: Option<Instant> = None;

    let last_tui_quit = loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tokio::spawn(serve_client(
                        stream,
                        daemon.clone(),
                        shutdown.clone(),
                        tuis.clone(),
                        ClientCount::enter(&clients),
                    ));
                }
                Err(err) => warn!(%err, "accept failed"),
            },
            _ = terminate.recv() => {
                info!("terminated");
                break false;
            }
            _ = interrupt.recv() => {
                info!("interrupted");
                break false;
            }
            _ = hangup.recv() => debug!("ignoring SIGHUP"),
            () = shutdown.notified() => {
                info!("shutdown requested");
                break false;
            }
            () = tuis.last_left.notified() => {
                if tuis.count.load(Ordering::SeqCst) > 0 {
                    continue; // another TUI attached in the meantime
                }
                if keep_forwards_running(&config_path) {
                    debug!("last TUI quit, forwards keep running");
                    continue;
                }
                break true;
            }
            _ = idle_check.tick() => {
                // A failed read must not end a daemon that may run forwards.
                let busy = clients.load(Ordering::Relaxed) > 0
                    || daemon.lock().await.has_enabled().unwrap_or_else(|err| {
                        warn!(%err, "cannot read the forwards, so not idle");
                        true
                    });
                if busy {
                    idle_since = None;
                } else if idle_since.get_or_insert_with(Instant::now).elapsed() >= idle_exit {
                    info!("nothing to do, exiting");
                    break false;
                }
            }
        }
    };

    // Closed first, before waiting for the lock: a client from now on finds
    // no daemon and starts the next one, which waits for the lock of the
    // data directory, instead of waiting for an answer that never comes.
    drop(listener);
    files.remove_socket();
    let mut daemon = daemon.lock().await;
    if last_tui_quit {
        // A TUI that attached while the lock was busy keeps the forwards,
        // and finds them on the next daemon.
        if tuis.count.load(Ordering::SeqCst) > 0 {
            info!("last TUI quit, another attached meanwhile; forwards keep running");
        } else {
            info!("last TUI quit, switching all forwards off");
            if let Err(err) = daemon.switch_all_off() {
                warn!(%err, "cannot switch the forwards off");
            }
        }
    }
    daemon.engine.shutdown();
    drop(daemon);
    drop(files);
    info!("daemon stopped");
    Ok(())
}

/// The socket and the PID file of the daemon that holds the lock; removed
/// when it ends, also on an error.
struct DaemonFiles {
    socket: PathBuf,
    pid: PathBuf,
}

impl DaemonFiles {
    fn remove_socket(&self) {
        let _ = fs::remove_file(&self.socket);
    }
}

impl Drop for DaemonFiles {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_file(&self.pid);
    }
}

/// How long the daemon stays without enabled forwards and clients:
/// [`IDLE_EXIT`], or `KPRTFWD_IDLE_EXIT_MS` milliseconds, for tests.
fn idle_exit(milliseconds: Option<String>) -> Duration {
    milliseconds
        .and_then(|ms| ms.trim().parse().ok())
        .map_or(IDLE_EXIT, Duration::from_millis)
}

/// How often the daemon checks whether it is idle: four times per idle
/// time, at most every 30 s.
fn idle_check(idle_exit: Duration) -> Duration {
    (idle_exit / 4).clamp(Duration::from_millis(10), Duration::from_secs(30))
}

/// Binds the socket. Returns `None` if another daemon already serves it: one
/// of a build without the lock. Called with the lock held, so a socket nobody
/// answers on is stale and no other daemon replaces it at the same time.
async fn bind_socket(path: &Path) -> Result<Option<UnixListener>> {
    if path.exists() {
        match Client::connect(path).await {
            Ok(other) => {
                info!(pid = other.daemon_pid, "another daemon is running");
                return Ok(None);
            }
            Err(ClientError::NotRunning(_)) => {
                fs::remove_file(path)
                    .with_context(|| format!("remove stale {}", path.display()))?;
            }
            Err(err) => bail!("cannot use {}: {err}", path.display()),
        }
    }
    // Created with mode 0700, never open to others before the chmod below.
    // Other threads that create files meanwhile only get stricter modes.
    let previous = umask(Mode::RWXG | Mode::RWXO);
    let bound = UnixListener::bind(path);
    umask(previous);
    let listener = match bound {
        Ok(listener) => listener,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            info!("another daemon started at the same time");
            return Ok(None);
        }
        Err(err) if err.kind() == std::io::ErrorKind::InvalidInput => {
            bail!(
                "cannot use {} as socket ({err}); Unix socket paths must be shorter than \
                 about 100 bytes, so set KPRTFWD_HOME to a shorter directory",
                path.display()
            )
        }
        Err(err) => return Err(err).with_context(|| format!("bind {}", path.display())),
    };
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(Some(listener))
}

/// The TUIs connected right now; tells the main loop when the last one leaves.
#[derive(Default)]
struct Tuis {
    count: AtomicUsize,
    last_left: Notify,
}

/// Held by the connection of a TUI.
struct Attached(Arc<Tuis>);

impl Attached {
    fn new(tuis: &Arc<Tuis>) -> Self {
        tuis.count.fetch_add(1, Ordering::SeqCst);
        Self(tuis.clone())
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.last_left.notify_one();
        }
    }
}

/// Reads the setting when it is needed, so edits apply without a restart.
fn keep_forwards_running(config_path: &Path) -> bool {
    match Config::load(config_path) {
        Ok(config) => config.keep_forwards_running,
        Err(err) => {
            warn!(%err, "using the default settings");
            Config::default().keep_forwards_running
        }
    }
}

/// Counts connected clients for the idle exit.
struct ClientCount(Arc<AtomicUsize>);

impl ClientCount {
    fn enter(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::Relaxed);
        Self(count.clone())
    }
}

impl Drop for ClientCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn serve_client(
    stream: UnixStream,
    daemon: Arc<Mutex<Daemon>>,
    shutdown: Arc<Notify>,
    tuis: Arc<Tuis>,
    _count: ClientCount,
) {
    // Only processes of this user can reach the socket in its directory;
    // checked again in case the modes were circumvented.
    match stream.peer_cred() {
        Ok(peer) if peer.uid() == geteuid().as_raw() => {}
        Ok(peer) => {
            warn!(uid = peer.uid(), "refused a connection of another user");
            return;
        }
        Err(err) => {
            warn!(%err, "cannot tell whose connection it is; refused");
            return;
        }
    }
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    match next_request(&mut reader, &mut writer).await {
        Some(ClientMessage {
            id,
            request: Request::Hello { protocol, .. },
        }) => {
            let welcome = ServerMessage::Response {
                id,
                response: Response::Welcome {
                    protocol: PROTOCOL_VERSION,
                    daemon_version: VERSION.to_owned(),
                    pid: std::process::id(),
                },
            };
            if write_message(&mut writer, &welcome).await.is_err() || protocol != PROTOCOL_VERSION {
                return;
            }
        }
        Some(message) => {
            let refusal = ServerMessage::Error {
                id: message.id,
                message: "send hello first".into(),
            };
            let _ = write_message(&mut writer, &refusal).await;
            return;
        }
        None => return,
    }

    // Dropped when the connection ends, however it ends.
    let mut attached = None;
    loop {
        let Some(message) = next_request(&mut reader, &mut writer).await else {
            return;
        };
        let id = message.id;
        let shutdown_requested = matches!(message.request, Request::Shutdown);
        let result = match message.request {
            Request::Shutdown => Ok(Response::Done),
            Request::Attach => {
                attached.get_or_insert_with(|| Attached::new(&tuis));
                Ok(Response::Done)
            }
            // These talk to a cluster and may take seconds; they must not
            // hold the lock that snapshot requests need.
            Request::ListContexts => {
                discovery::contexts().map(|(names, current)| Response::Contexts { names, current })
            }
            Request::Discover {
                context,
                namespace_filter,
            } => discover(&daemon, &context, &namespace_filter).await,
            Request::Prune {
                context,
                namespace_filter,
                delete,
            } => prune(&daemon, &context, &namespace_filter, delete.as_deref()).await,
            request => daemon.lock().await.handle(request).await,
        };
        let reply = match result {
            Ok(response) => ServerMessage::Response { id, response },
            Err(message) => ServerMessage::Error { id, message },
        };
        let written = write_message(&mut writer, &reply).await;
        // Answered first: the daemon exits at once, and ending the runtime
        // ends this task too.
        if shutdown_requested {
            shutdown.notify_one();
        }
        if written.is_err() {
            return;
        }
    }
}

/// The client's next request; `None` once the connection is closed or
/// broken. Input that is no request is answered with an error, and the
/// client may go on: a line longer than [`kprtfwd_proto::MAX_FRAME`], which
/// is skipped, and a line that does not parse. Closing the connection
/// instead would count as the TUI quitting.
async fn next_request(
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &mut OwnedWriteHalf,
) -> Option<ClientMessage> {
    loop {
        let refusal = match read_frame(reader).await {
            Ok(Some(line)) => match ClientMessage::parse(&line) {
                Ok(message) => return Some(message),
                Err((id, message)) => ServerMessage::Error { id, message },
            },
            Ok(None) => return None,
            Err(err) if err.kind() == std::io::ErrorKind::InvalidData => ServerMessage::Error {
                id: 0,
                message: format!("cannot read the request: {err}"),
            },
            Err(err) => {
                debug!(%err, "cannot read from a client");
                return None;
            }
        };
        debug!(?refusal, "client sent no request");
        write_message(writer, &refusal).await.ok()?;
    }
}

struct Daemon {
    store: Store,
    engine: Engine,
}

impl Daemon {
    /// Runs the enabled forwards, or none while paused.
    async fn apply(&mut self) -> Result<(), StoreError> {
        let wanted = self.wanted()?;
        self.engine.reconcile(&wanted).await;
        Ok(())
    }

    fn wanted(&self) -> Result<Vec<Forward>, StoreError> {
        if self.store.paused()? {
            return Ok(Vec::new());
        }
        Ok(self
            .store
            .forwards()?
            .into_iter()
            .filter(|f| f.enabled)
            .collect())
    }

    /// Switches every forward off and lifts a pause, so the next start begins
    /// with nothing running.
    fn switch_all_off(&mut self) -> Result<(), StoreError> {
        self.store.set_enabled_exactly(&[])?;
        self.store.set_paused(false)
    }

    fn has_enabled(&self) -> Result<bool, StoreError> {
        Ok(self.store.forwards()?.iter().any(|f| f.enabled))
    }

    fn project(&self, name: &str) -> Result<Project, String> {
        self.store
            .projects()
            .map_err(|err| err.to_string())?
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("no project named {name:?}"))
    }

    /// Why no running forward uses `context`, so a reconnect would rebuild
    /// nothing: no forward uses it, its forwards are off, or all are paused.
    fn not_connected(&self, context: &str) -> Result<String, StoreError> {
        let forwards: Vec<Forward> = self
            .store
            .forwards()?
            .into_iter()
            .filter(|f| f.context == context)
            .collect();
        let why = if forwards.is_empty() {
            "no forward uses it"
        } else if !forwards.iter().any(|f| f.enabled) {
            "its forwards are off"
        } else {
            "the forwards are paused"
        };
        Ok(format!("the context {context:?} is not connected: {why}"))
    }

    async fn handle(&mut self, request: Request) -> Result<Response, String> {
        let text = |err: StoreError| err.to_string();
        match request {
            Request::Hello { .. } => return Err("already connected".into()),
            Request::Shutdown => return Ok(Response::Done),
            Request::Snapshot => return self.snapshot().map(Response::Snapshot).map_err(text),
            Request::SetEnabled { ids, enabled } => {
                self.store.set_enabled(&ids, enabled).map_err(text)?;
                info!(count = ids.len(), enabled, "forwards switched");
            }
            Request::SetProjectEnabled { name, enabled } => {
                let project = self.project(&name)?;
                self.store
                    .set_enabled(&project.forwards, enabled)
                    .map_err(text)?;
                info!(project = %name, enabled, "project switched");
            }
            Request::SwitchProject { name } => {
                let project = self.project(&name)?;
                self.store
                    .set_enabled_exactly(&project.forwards)
                    .map_err(text)?;
                info!(project = %name, "switched to project");
            }
            Request::SetPaused { paused } => {
                self.store.set_paused(paused).map_err(text)?;
                info!(paused, "pause changed");
            }
            Request::ReconnectContext { name } => {
                let wanted = self.wanted().map_err(text)?;
                if !wanted.iter().any(|f| f.context == name) {
                    return Err(self.not_connected(&name).map_err(text)?);
                }
                self.engine.reconnect(&name, &wanted, None).await;
                info!(context = %name, "reconnected");
                return Ok(Response::Done);
            }
            Request::AddForwards { forwards } => {
                for forward in &forwards {
                    validate_forward(forward).map_err(|err| format!("{}: {err}", forward.id))?;
                }
                self.store.add_forwards(&forwards).map_err(text)?;
                info!(count = forwards.len(), "forwards added");
            }
            Request::EditForward { id, settings } => {
                validate_settings(&settings).map_err(|err| format!("{id}: {err}"))?;
                let saved = self.store.edit_forward(&id, &settings).map_err(text)?;
                if saved != id {
                    self.engine.rename(&id, &saved);
                }
                self.apply().await.map_err(text)?;
                info!(%id, %saved, "forward edited");
                return Ok(Response::Saved { id: saved });
            }
            Request::DeleteForward { id } => {
                if !self.store.delete_forward(&id).map_err(text)? {
                    return Err(format!("unknown forward {id}"));
                }
                self.apply().await.map_err(text)?;
                self.engine.forget(&id);
                info!(%id, "forward deleted");
                return Ok(Response::Done);
            }
            Request::UpsertProject { project } => {
                self.store.upsert_project(&project).map_err(text)?;
                info!(project = %project.name, members = project.forwards.len(), "project saved");
                return Ok(Response::Done);
            }
            Request::RenameProject { name, to } => {
                if !self.store.rename_project(&name, &to).map_err(text)? {
                    return Err(format!("no project named {name:?}"));
                }
                info!(project = %name, %to, "project renamed");
                return Ok(Response::Done);
            }
            Request::DeleteProject { name } => {
                if !self.store.delete_project(&name).map_err(text)? {
                    return Err(format!("no project named {name:?}"));
                }
                info!(project = %name, "project deleted");
                return Ok(Response::Done);
            }
            Request::Events { ids } => {
                let mut entries = Vec::new();
                let mut contexts = HashSet::new();
                for id in &ids {
                    let forward = self
                        .store
                        .forward(id)
                        .map_err(text)?
                        .ok_or_else(|| format!("unknown forward {id}"))?;
                    let source = id.to_string();
                    entries.extend(self.engine.forward_events(id).into_iter().map(|e| {
                        EventEntry {
                            at: e.at,
                            source: source.clone(),
                            message: e.message,
                        }
                    }));
                    contexts.insert(forward.context);
                }
                for context in contexts {
                    entries.extend(self.engine.context_events(&context).into_iter().map(|e| {
                        EventEntry {
                            at: e.at,
                            source: context.clone(),
                            message: e.message,
                        }
                    }));
                }
                entries.sort_by_key(|entry| entry.at);
                return Ok(Response::Events { entries });
            }
            Request::Attach
            | Request::ListContexts
            | Request::Discover { .. }
            | Request::Prune { .. } => {
                return Err("handled outside the lock".into());
            }
        }
        self.apply().await.map_err(text)?;
        Ok(Response::Done)
    }

    fn snapshot(&self) -> Result<Snapshot, StoreError> {
        let paused = self.store.paused()?;
        let forwards = self.store.forwards()?;

        let mut names: Vec<&str> = forwards.iter().map(|f| f.context.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        let contexts = names
            .into_iter()
            .map(|name| ContextView {
                name: name.to_owned(),
                state: self
                    .engine
                    .context_state(name)
                    .unwrap_or(ContextState::Idle),
            })
            .collect();

        let projects = self
            .store
            .projects()?
            .into_iter()
            .map(|project| {
                let enabled = project
                    .forwards
                    .iter()
                    .filter(|id| forwards.iter().any(|f| &f.id == *id && f.enabled))
                    .count();
                ProjectView {
                    name: project.name,
                    forwards: project.forwards,
                    enabled,
                }
            })
            .collect();

        let forwards = forwards
            .into_iter()
            .map(|forward| {
                let observed = self.engine.observe(&forward.id);
                ForwardView {
                    state: derive(forward.enabled, paused, observed.as_ref()),
                    last_error: self.engine.last_error(&forward.id).map(|e| LastError {
                        at: e.at,
                        message: e.message,
                    }),
                    forward,
                }
            })
            .collect();

        Ok(Snapshot {
            daemon_version: VERSION.to_owned(),
            paused,
            contexts,
            forwards,
            projects,
            kubeconfig_files: kubeconfig::files()
                .iter()
                .map(|path| {
                    if path.exists() {
                        path.display().to_string()
                    } else {
                        format!("{} (missing)", path.display())
                    }
                })
                .collect(),
        })
    }
}

/// Takes over `~/.kprtfwd`, where kprtfwd v1 and the first v2 builds kept
/// their data, while this directory has no database yet: stops a daemon still
/// running there and copies its database. The copy gets its final name only
/// once it is complete, so after a failed one the next start tries again. The
/// old directory is left as it is, so kprtfwd v1 keeps working on it.
async fn adopt_legacy_dir(data_dir: &Path) -> Result<()> {
    let Some(legacy) = paths::legacy_dir() else {
        return Ok(());
    };
    let db = data_dir.join("kprtfwd.db");
    let legacy_db = legacy.join("kprtfwd.db");
    if db.exists() || !legacy_db.exists() {
        return Ok(());
    }
    let stopped = stop_daemon(&legacy.join("daemon.sock"), &legacy.join("daemon.pid"))
        .await
        .with_context(|| format!("stop the daemon running in {}", legacy.display()))?;
    if stopped {
        info!(dir = %legacy.display(), "stopped the daemon of the old directory");
    }
    Store::copy_file(&legacy_db, &db)
        .with_context(|| format!("copy {} to {}", legacy_db.display(), db.display()))?;
    info!(from = %legacy_db.display(), to = %db.display(), "copied the database");
    Ok(())
}

/// Reconnects contexts whose kubeconfig settings changed: a renewed token, a
/// moved cluster, a context that did not exist before. Checks the files every
/// 2 s and reads them once a change has settled; see [`kubeconfig::Watcher`].
/// A change elsewhere in the file, like `kubectl config use-context` or
/// `kubens`, touches no running context.
async fn watch_kubeconfig(daemon: Arc<Mutex<Daemon>>) {
    let mut watcher = kubeconfig::Watcher::new(kubeconfig::files());
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let Some(change) = watcher.check() else {
            continue;
        };
        if change.contexts.is_empty() {
            continue;
        }
        let mut daemon = daemon.lock().await;
        for name in daemon.engine.context_names() {
            if !change.contexts.contains(&name) {
                continue;
            }
            info!(context = %name, "kubeconfig changed, reconnecting");
            match daemon.wanted() {
                Ok(wanted) => {
                    let kubeconfig = Some(&change.kubeconfig);
                    daemon.engine.reconnect(&name, &wanted, kubeconfig).await;
                }
                Err(err) => warn!(%err, "cannot read forwards"),
            }
        }
    }
}

/// Lists the service ports in a context, with the ID and local port each would
/// get, or already has.
async fn discover(
    daemon: &Mutex<Daemon>,
    context: &str,
    namespace_filter: &str,
) -> Result<Response, String> {
    let listing = discovery::services(context, namespace_filter).await?;
    let forwards = daemon
        .lock()
        .await
        .store
        .forwards()
        .map_err(|err| err.to_string())?;

    let mut taken_ids: HashSet<ForwardId> = forwards.iter().map(|f| f.id.clone()).collect();
    let mut taken_ports: HashSet<u16> = forwards.iter().map(|f| f.local_port).collect();

    let mut ports = Vec::new();
    for service in listing.services {
        for port in service.ports {
            let existing = forwards.iter().find(|f| {
                f.context == context
                    && f.namespace == service.namespace
                    && f.service == service.name
                    && f.remote_port == port.port
            });
            let (id, local_port) = match existing {
                Some(forward) => (forward.id.clone(), forward.local_port),
                None => {
                    let id = forward_id(
                        context,
                        &service.namespace,
                        &service.name,
                        port.port,
                        &taken_ids,
                    );
                    // Free on both loopback addresses, as the forward needs it.
                    let local =
                        propose_local_port(port.port, &taken_ports, port_is_free).unwrap_or(0);
                    taken_ids.insert(id.clone());
                    taken_ports.insert(local);
                    (id, local)
                }
            };
            ports.push(DiscoveredPort {
                namespace: service.namespace.clone(),
                service: service.name.clone(),
                port: port.port,
                port_name: port.name,
                service_type: classify(&service.name, port.port),
                existing: existing.map(|f| f.id.clone()),
                id,
                local_port,
            });
        }
    }
    Ok(Response::Discovered {
        ports,
        skipped: listing.skipped,
    })
}

/// Lists the forwards in `context` whose service no longer exists. With
/// `delete`, deletes those of these IDs that are still gone and returns them:
/// a forward the user was not shown, or whose service is back, stays. If the
/// cluster cannot be listed, nothing is deleted; forwards in namespaces whose
/// services the user may not list are neither listed nor deleted.
async fn prune(
    daemon: &Mutex<Daemon>,
    context: &str,
    namespace_filter: &str,
    delete: Option<&[ForwardId]>,
) -> Result<Response, String> {
    let listing = discovery::services(context, namespace_filter).await?;
    let existing: HashSet<(String, String)> = listing
        .services
        .into_iter()
        .map(|s| (s.namespace, s.name))
        .collect();

    let mut daemon = daemon.lock().await;
    let forwards = daemon.store.forwards().map_err(|err| err.to_string())?;
    let gone = gone_forwards(
        forwards,
        context,
        namespace_filter,
        &existing,
        &listing.skipped,
        delete,
    );
    if delete.is_some() && !gone.is_empty() {
        for forward in &gone {
            daemon
                .store
                .delete_forward(&forward.id)
                .map_err(|err| err.to_string())?;
        }
        daemon.apply().await.map_err(|err| err.to_string())?;
        for forward in &gone {
            daemon.engine.forget(&forward.id);
        }
        info!(context, count = gone.len(), "pruned forwards");
    }
    Ok(Response::Pruned {
        forwards: gone,
        skipped: listing.skipped,
    })
}

/// The forwards in `context` and in a namespace matching `namespace_filter`
/// whose service is not among `existing` (namespace, name); with `only`, just
/// those of these IDs. Forwards in a `skipped` namespace are unknown, not
/// gone.
fn gone_forwards(
    forwards: Vec<Forward>,
    context: &str,
    namespace_filter: &str,
    existing: &HashSet<(String, String)>,
    skipped: &[String],
    only: Option<&[ForwardId]>,
) -> Vec<Forward> {
    let filter = namespace_filter.trim();
    let all = filter.is_empty() || filter == "*";
    forwards
        .into_iter()
        .filter(|f| f.context == context)
        .filter(|f| all || wildcard(filter, &f.namespace))
        .filter(|f| !skipped.contains(&f.namespace))
        .filter(|f| !existing.contains(&(f.namespace.clone(), f.service.clone())))
        .filter(|f| only.is_none_or(|ids| ids.contains(&f.id)))
        .collect()
}

/// The socket path the daemon uses, for clients.
pub fn socket_path() -> Result<PathBuf> {
    paths::socket_path().context("neither KPRTFWD_HOME nor HOME is set")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forward(id: &str, context: &str, namespace: &str, service: &str) -> Forward {
        Forward {
            id: ForwardId::new(id),
            context: context.into(),
            namespace: namespace.into(),
            service: service.into(),
            remote_port: 80,
            local_port: 18080,
            enabled: false,
        }
    }

    fn forwards() -> Vec<Forward> {
        vec![
            forward("api", "staging", "shop", "api"),
            forward("web", "staging", "shop", "frontend"),
            forward("db", "staging", "shop-data", "mysql"),
            forward("other", "production", "shop", "web"),
        ]
    }

    fn ids(forwards: &[Forward]) -> Vec<&str> {
        forwards.iter().map(|f| f.id.as_str()).collect()
    }

    fn existing(services: &[(&str, &str)]) -> HashSet<(String, String)> {
        services
            .iter()
            .map(|(ns, name)| ((*ns).to_owned(), (*name).to_owned()))
            .collect()
    }

    #[test]
    fn panics_are_logged() {
        #[derive(Clone, Default)]
        struct Log(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Log {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let log = Log::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _default = tracing::subscriber::set_default(subscriber);

        log_panics();
        let panicked = std::panic::catch_unwind(|| panic!("the watcher broke"));
        // The default hook again.
        drop(std::panic::take_hook());

        assert!(panicked.is_err());
        let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert!(text.contains("ERROR"), "{text}");
        assert!(text.contains("panicked at"), "{text}");
        assert!(text.contains("the watcher broke"), "{text}");
    }

    #[test]
    fn the_data_dir_must_be_the_users_and_is_narrowed_to_700() {
        let dir = tempfile::tempdir().unwrap();
        let uid = geteuid().as_raw();
        let mode = || fs::metadata(dir.path()).unwrap().mode() & 0o7777;
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o775)).unwrap();
        assert_eq!(secure_data_dir(dir.path(), uid).unwrap(), Some(0o775));
        assert_eq!(mode(), 0o700);
        assert_eq!(secure_data_dir(dir.path(), uid).unwrap(), None);

        let err = secure_data_dir(dir.path(), uid + 1)
            .unwrap_err()
            .to_string();
        assert!(
            err.ends_with(&format!(
                "belongs to uid {uid}, not to this user (uid {}); use a directory of your own",
                uid + 1
            )),
            "{err}"
        );
    }

    #[test]
    fn raises_the_open_files_limit_up_to_the_hard_limit_only() {
        // A macOS terminal: 256, hard limit unlimited.
        assert_eq!(open_files_limit(Some(256), None), Some(OPEN_FILES));
        assert_eq!(
            open_files_limit(Some(1024), Some(524_288)),
            Some(OPEN_FILES)
        );
        assert_eq!(open_files_limit(Some(256), Some(4096)), Some(4096));
        // Already enough, or as high as allowed: kept, never lowered.
        assert_eq!(open_files_limit(Some(OPEN_FILES), None), None);
        assert_eq!(open_files_limit(Some(1_048_576), None), None);
        assert_eq!(open_files_limit(None, None), None);
        assert_eq!(open_files_limit(Some(4096), Some(4096)), None);
    }

    #[test]
    fn the_idle_exit_can_be_shortened_for_tests() {
        assert_eq!(idle_exit(None), IDLE_EXIT);
        assert_eq!(idle_exit(Some("400".into())), Duration::from_millis(400));
        assert_eq!(idle_exit(Some("soon".into())), IDLE_EXIT);
        assert_eq!(idle_check(IDLE_EXIT), Duration::from_secs(30));
        assert_eq!(
            idle_check(Duration::from_millis(400)),
            Duration::from_millis(100)
        );
        // tokio's interval refuses a period of 0.
        assert_eq!(idle_check(Duration::ZERO), Duration::from_millis(10));
    }

    #[test]
    fn lists_the_gone_forwards_of_the_context_and_filter() {
        let services = existing(&[("shop", "api")]);
        let gone = gone_forwards(forwards(), "staging", "*", &services, &[], None);
        assert_eq!(ids(&gone), ["web", "db"]);
        let gone = gone_forwards(forwards(), "staging", "shop", &services, &[], None);
        assert_eq!(ids(&gone), ["web"]);
        let gone = gone_forwards(forwards(), "staging", "shop-*", &services, &[], None);
        assert_eq!(ids(&gone), ["db"]);
    }

    #[test]
    fn forwards_in_skipped_namespaces_are_never_gone() {
        // The user may not list the services in shop-data: whether mysql is
        // still there is unknown, so "db" is neither listed nor deleted.
        let skipped = ["shop-data".to_owned()];
        let services = existing(&[("shop", "api")]);
        let gone = gone_forwards(forwards(), "staging", "*", &services, &skipped, None);
        assert_eq!(ids(&gone), ["web"]);
        let confirmed = [ForwardId::new("web"), ForwardId::new("db")];
        let gone = gone_forwards(
            forwards(),
            "staging",
            "*",
            &services,
            &skipped,
            Some(&confirmed),
        );
        assert_eq!(ids(&gone), ["web"]);
    }

    #[test]
    fn deletes_only_confirmed_forwards_that_are_still_gone() {
        // The user saw and confirmed "web" and "db". Meanwhile "api" lost its
        // service and "db" got its service back.
        let confirmed = [ForwardId::new("web"), ForwardId::new("db")];
        let services = existing(&[("shop-data", "mysql")]);
        let gone = gone_forwards(forwards(), "staging", "*", &services, &[], Some(&confirmed));
        assert_eq!(ids(&gone), ["web"]);
    }

    #[test]
    fn confirmed_ids_outside_the_context_or_filter_are_not_deleted() {
        let confirmed = [ForwardId::new("other"), ForwardId::new("db")];
        let gone = gone_forwards(
            forwards(),
            "staging",
            "shop",
            &existing(&[]),
            &[],
            Some(&confirmed),
        );
        assert!(gone.is_empty(), "{:?}", ids(&gone));
        let gone = gone_forwards(forwards(), "staging", "*", &existing(&[]), &[], Some(&[]));
        assert!(gone.is_empty(), "{:?}", ids(&gone));
    }
}
