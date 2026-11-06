//! The kprtfwd daemon: owns the forwards and serves clients over a Unix
//! socket. A client starts it on demand. It exits after a while with nothing
//! to do, and when the last TUI quits, unless `config.toml` says to keep the
//! forwards running.

use std::collections::HashSet;
use std::fs::{self, DirBuilder, File, TryLockError};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use kprtfwd_core::filter::wildcard;
use kprtfwd_core::naming::{forward_id, propose_local_port};
use kprtfwd_core::state::derive;
use kprtfwd_core::svctype::classify;
use kprtfwd_core::validate::validate_forward;
use kprtfwd_core::{Config, ContextState, Forward, ForwardId, Project, Store, StoreError, paths};
use kprtfwd_engine::{Engine, discovery, kubeconfig, port_is_free};
use kprtfwd_proto::{
    Client, ClientError, ClientMessage, ContextView, DiscoveredPort, EventEntry, ForwardView,
    LastError, PROTOCOL_VERSION, ProjectView, Request, Response, ServerMessage, Snapshot,
    read_message, stop_daemon, write_message,
};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
use tokio::io::BufReader;
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
    let data_dir = paths::data_dir().context("neither KPRTFWD_HOME nor HOME is set")?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&data_dir)
        .with_context(|| format!("create {}", data_dir.display()))?;
    let _log_guard = init_logging(&options, &data_dir)?;
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
        match Client::connect(&socket).await {
            Ok(other) => {
                info!(pid = other.daemon_pid, "another daemon is running");
                return Ok(None);
            }
            Err(err @ ClientError::ProtocolMismatch { .. }) => {
                bail!("a daemon answers on {}, but: {err}", socket.display())
            }
            // Not bound yet, or no longer.
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
        .with_target(false)
        .with_ansi(false)
        .with_writer(writer)
        .init();
    Ok(Some(guard))
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
    if let Err(err) = adopt_legacy_dir(data_dir).await {
        let _ = fs::remove_file(&socket);
        return Err(err);
    }
    // Lets `kprtfwd daemon stop` end a daemon of another version, which it
    // cannot talk to.
    let pid_file = data_dir.join("daemon.pid");
    fs::write(&pid_file, format!("{}\n", std::process::id()))
        .with_context(|| format!("write {}", pid_file.display()))?;
    let config_path = data_dir.join("config.toml");
    match Config::write_template(&config_path) {
        Ok(true) => info!(path = %config_path.display(), "wrote the default settings"),
        Ok(false) => {}
        Err(err) => warn!(%err, path = %config_path.display(), "cannot write the default settings"),
    }
    let store = Store::open(&data_dir.join("kprtfwd.db")).context("open the database")?;
    let daemon = Arc::new(Mutex::new(Daemon {
        store,
        engine: Engine::new(),
    }));
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
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    // The terminal that started the daemon may close; that must not stop it.
    let mut hangup = signal(SignalKind::hangup())?;
    let mut idle_check = tokio::time::interval(Duration::from_secs(30));
    let mut idle_since: Option<Instant> = None;

    loop {
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
                break;
            }
            _ = interrupt.recv() => {
                info!("interrupted");
                break;
            }
            _ = hangup.recv() => debug!("ignoring SIGHUP"),
            () = shutdown.notified() => {
                info!("shutdown requested");
                break;
            }
            () = tuis.last_left.notified() => {
                if tuis.count.load(Ordering::SeqCst) > 0 {
                    continue; // another TUI attached in the meantime
                }
                if keep_forwards_running(&config_path) {
                    debug!("last TUI quit, forwards keep running");
                    continue;
                }
                info!("last TUI quit, switching all forwards off");
                if let Err(err) = daemon.lock().await.switch_all_off() {
                    warn!(%err, "cannot switch the forwards off");
                }
                break;
            }
            _ = idle_check.tick() => {
                let busy = clients.load(Ordering::Relaxed) > 0
                    || daemon.lock().await.has_enabled()?;
                if busy {
                    idle_since = None;
                } else if idle_since.get_or_insert_with(Instant::now).elapsed() >= IDLE_EXIT {
                    info!("nothing to do, exiting");
                    break;
                }
            }
        }
    }

    daemon.lock().await.engine.shutdown();
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_file(&pid_file);
    info!("daemon stopped");
    Ok(())
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
            Err(err) => bail!("a daemon answers on {}, but: {err}", path.display()),
        }
    }
    let listener = match UnixListener::bind(path) {
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
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    match read_message::<_, ClientMessage>(&mut reader).await {
        Ok(Some(ClientMessage {
            id,
            request: Request::Hello { protocol, .. },
        })) => {
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
        Ok(Some(message)) => {
            let refusal = ServerMessage::Error {
                id: message.id,
                message: "send hello first".into(),
            };
            let _ = write_message(&mut writer, &refusal).await;
            return;
        }
        Ok(None) | Err(_) => return,
    }

    // Dropped when the connection ends, however it ends.
    let mut attached = None;
    loop {
        let message = match read_message::<_, ClientMessage>(&mut reader).await {
            Ok(Some(message)) => message,
            Ok(None) => return,
            Err(err) => {
                debug!(%err, "client sent garbage");
                return;
            }
        };
        let id = message.id;
        let result = match message.request {
            Request::Shutdown => {
                shutdown.notify_one();
                Ok(Response::Done)
            }
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
        if write_message(&mut writer, &reply).await.is_err() {
            return;
        }
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
                self.engine.reconnect(&name, &wanted, None).await;
                info!(context = %name, "reconnected");
                return Ok(Response::Done);
            }
            Request::UpsertForwards { forwards } => {
                // Check all first, so a bad entry adds none.
                for forward in &forwards {
                    validate_forward(forward).map_err(|err| format!("{}: {err}", forward.id))?;
                }
                for forward in &forwards {
                    self.store.upsert_forward(forward).map_err(text)?;
                }
                info!(count = forwards.len(), "forwards saved");
            }
            Request::DeleteForward { id } => {
                if !self.store.delete_forward(&id).map_err(text)? {
                    return Err(format!("unknown forward {id}"));
                }
                info!(%id, "forward deleted");
            }
            Request::UpsertProject { project } => {
                self.store.upsert_project(&project).map_err(text)?;
                info!(project = %project.name, members = project.forwards.len(), "project saved");
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
    let services = discovery::services(context, namespace_filter).await?;
    let forwards = daemon
        .lock()
        .await
        .store
        .forwards()
        .map_err(|err| err.to_string())?;

    let mut taken_ids: HashSet<ForwardId> = forwards.iter().map(|f| f.id.clone()).collect();
    let mut taken_ports: HashSet<u16> = forwards.iter().map(|f| f.local_port).collect();

    let mut ports = Vec::new();
    for service in services {
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
    Ok(Response::Discovered { ports })
}

/// Lists the forwards in `context` whose service no longer exists. With
/// `delete`, deletes those of these IDs that are still gone and returns them:
/// a forward the user was not shown, or whose service is back, stays. If the
/// cluster cannot be listed, nothing is deleted.
async fn prune(
    daemon: &Mutex<Daemon>,
    context: &str,
    namespace_filter: &str,
    delete: Option<&[ForwardId]>,
) -> Result<Response, String> {
    let services = discovery::services(context, namespace_filter).await?;
    let existing: HashSet<(String, String)> = services
        .into_iter()
        .map(|s| (s.namespace, s.name))
        .collect();

    let mut daemon = daemon.lock().await;
    let forwards = daemon.store.forwards().map_err(|err| err.to_string())?;
    let gone = gone_forwards(forwards, context, namespace_filter, &existing, delete);
    if delete.is_some() && !gone.is_empty() {
        for forward in &gone {
            daemon
                .store
                .delete_forward(&forward.id)
                .map_err(|err| err.to_string())?;
        }
        daemon.apply().await.map_err(|err| err.to_string())?;
        info!(context, count = gone.len(), "pruned forwards");
    }
    Ok(Response::Pruned { forwards: gone })
}

/// The forwards in `context` and in a namespace matching `namespace_filter`
/// whose service is not among `existing` (namespace, name); with `only`, just
/// those of these IDs.
fn gone_forwards(
    forwards: Vec<Forward>,
    context: &str,
    namespace_filter: &str,
    existing: &HashSet<(String, String)>,
    only: Option<&[ForwardId]>,
) -> Vec<Forward> {
    let filter = namespace_filter.trim();
    let all = filter.is_empty() || filter == "*";
    forwards
        .into_iter()
        .filter(|f| f.context == context)
        .filter(|f| all || wildcard(filter, &f.namespace))
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
    fn lists_the_gone_forwards_of_the_context_and_filter() {
        let services = existing(&[("shop", "api")]);
        let gone = gone_forwards(forwards(), "staging", "*", &services, None);
        assert_eq!(ids(&gone), ["web", "db"]);
        let gone = gone_forwards(forwards(), "staging", "shop", &services, None);
        assert_eq!(ids(&gone), ["web"]);
        let gone = gone_forwards(forwards(), "staging", "shop-*", &services, None);
        assert_eq!(ids(&gone), ["db"]);
    }

    #[test]
    fn deletes_only_confirmed_forwards_that_are_still_gone() {
        // The user saw and confirmed "web" and "db". Meanwhile "api" lost its
        // service and "db" got its service back.
        let confirmed = [ForwardId::new("web"), ForwardId::new("db")];
        let services = existing(&[("shop-data", "mysql")]);
        let gone = gone_forwards(forwards(), "staging", "*", &services, Some(&confirmed));
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
            Some(&confirmed),
        );
        assert!(gone.is_empty(), "{:?}", ids(&gone));
        let gone = gone_forwards(forwards(), "staging", "*", &existing(&[]), Some(&[]));
        assert!(gone.is_empty(), "{:?}", ids(&gone));
    }
}
