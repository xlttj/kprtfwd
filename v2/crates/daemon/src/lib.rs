//! The kprtfwd daemon: owns the forwards and serves clients over a Unix
//! socket. A client starts it on demand; it exits after a while with nothing
//! to do.

use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use kprtfwd_core::state::derive;
use kprtfwd_core::{ContextState, Forward, Project, Store, StoreError, paths};
use kprtfwd_engine::Engine;
use kprtfwd_proto::{
    Client, ClientError, ClientMessage, ContextView, ForwardView, LastError, PROTOCOL_VERSION,
    ProjectView, Request, Response, ServerMessage, Snapshot, read_message, write_message,
};
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Exit after this long without enabled forwards and without clients.
const IDLE_EXIT: Duration = Duration::from_secs(10 * 60);
/// kube-rs logs every failed request as an error; outages are reported once,
/// by the context state, instead.
const DEFAULT_LOG_FILTER: &str =
    "info,kube=warn,kube_client=off,kube_runtime=off,hyper=warn,tower=warn,rustls=warn";

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
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(serve(&data_dir));
    if let Err(err) = &result {
        tracing::error!("{err:#}");
    }
    result
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

async fn serve(data_dir: &Path) -> Result<()> {
    let socket = data_dir.join("daemon.sock");
    let Some(listener) = bind_socket(&socket).await? else {
        return Ok(());
    };
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

    let shutdown = Arc::new(Notify::new());
    let clients = Arc::new(AtomicUsize::new(0));
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
    info!("daemon stopped");
    Ok(())
}

/// Binds the socket. Returns `None` if another daemon already serves it.
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
                self.engine.reconnect(&name, &wanted).await;
                info!(context = %name, "reconnected");
                return Ok(Response::Done);
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
        })
    }
}

/// The socket path the daemon uses, for clients.
pub fn socket_path() -> Result<PathBuf> {
    paths::socket_path().context("neither KPRTFWD_HOME nor HOME is set")
}
