//! One enabled forward: listens on the local port and forwards every
//! connection to a ready pod behind the service.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use kprtfwd_core::Forward;
use kprtfwd_core::state::{Bind, ContextState, Observed, Target};
use kprtfwd_core::svctype::speaks_http;
use rustix::io::Errno;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, info, warn};

use crate::cache::Endpoint;
use crate::context::Context;
use crate::errors::{Failure, classify};
use crate::events::EventLog;
use crate::portforward::{self, KubeIo, PortForward, PortForwardError};

const BIND_RETRY: Duration = Duration::from_secs(5);
/// A port busy at the first attempt is tried again this soon before it
/// counts as busy; see [`bind`].
const BIND_FIRST_RETRY: Duration = Duration::from_millis(100);
/// The pause after a failed accept, so that a lasting error does not spin.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);
/// The pause while no file descriptor is left: accepting works again only
/// once connections are closed.
const ACCEPT_RETRY_NO_FDS: Duration = Duration::from_secs(1);
const NO_FDS: &str = "cannot accept connections: too many open files";
/// A warm stream is replaced after this long, in case it died silently.
const WARM_MAX_AGE: Duration = Duration::from_secs(60);

/// The last thing that went wrong with a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastError {
    pub at: SystemTime,
    pub message: String,
}

pub struct ForwardHandle {
    /// The settings the forward runs with.
    pub spec: Forward,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl ForwardHandle {
    pub fn start(spec: Forward, context: Arc<Context>, events: EventLog) -> Self {
        events.push("started");
        let shared = Arc::new(Shared::new(events));
        let task = tokio::spawn(run(spec.clone(), context, shared.clone()));
        Self { spec, shared, task }
    }

    /// Tells the forward's task to close the listeners and all connections.
    /// They are closed once a worker thread gets to the task, which may be
    /// after a forward started next; [`ForwardHandle::close`] waits for it.
    pub fn stop(&self) {
        if !self.shared.stopped.swap(true, Ordering::Relaxed) {
            self.shared.events.push("stopped");
        }
        self.shared.warm.lock().unwrap().take();
        self.task.abort();
    }

    /// Stops the forward and waits until its listeners and connections are
    /// closed, so that its port is free for the next forward.
    pub async fn close(mut self) {
        self.stop();
        // An aborted task drops its future, which owns the listeners,
        // before its handle completes.
        let _ = (&mut self.task).await;
    }

    pub fn observe(&self, context: &Context) -> Observed {
        let forbidden = self.shared.forbidden.lock().unwrap().clone();
        let target = match forbidden {
            Some(message) => Target::Forbidden(message),
            None => context
                .namespace(&self.spec.namespace)
                .map_or(Target::Syncing, |cache| {
                    cache.target(&self.spec.service, self.spec.remote_port)
                }),
        };
        Observed {
            bind: self.shared.bind.lock().unwrap().clone(),
            context: context.state(),
            target,
            pod: self.shared.pod.lock().unwrap().clone(),
            active_conns: self.shared.active.load(Ordering::Relaxed),
        }
    }

    pub fn last_error(&self) -> Option<LastError> {
        self.shared.last_error.lock().unwrap().clone()
    }
}

impl Drop for ForwardHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// State shared between the forward's tasks and its handle.
struct Shared {
    bind: Mutex<Bind>,
    /// The pod connections go to while it stays ready.
    pod: Mutex<Option<String>>,
    active: AtomicU32,
    /// Set when the API server refused the port-forward with 403.
    forbidden: Mutex<Option<String>>,
    last_error: Mutex<Option<LastError>>,
    warm: Mutex<Option<Warm>>,
    /// A warm stream is being opened; parallel connections do not open more.
    refilling: AtomicBool,
    stopped: AtomicBool,
    events: EventLog,
}

impl Shared {
    fn new(events: EventLog) -> Self {
        Self {
            bind: Mutex::new(Bind::Pending),
            pod: Mutex::new(None),
            active: AtomicU32::new(0),
            forbidden: Mutex::new(None),
            last_error: Mutex::new(None),
            warm: Mutex::new(None),
            refilling: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            events,
        }
    }

    /// Stays on the current pod while it is ready, otherwise switches.
    fn choose(&self, endpoints: &[Endpoint], spec: &Forward) -> Endpoint {
        let mut pod = self.pod.lock().unwrap();
        if let Some(current) = pod.as_deref()
            && let Some(endpoint) = endpoints.iter().find(|e| e.pod == current)
        {
            return endpoint.clone();
        }
        let next = endpoints[0].clone();
        info!(id = %spec.id, pod = %next.pod, previous = ?pod.as_deref(), "pod changed");
        self.events.push(match pod.as_deref() {
            Some(previous) => format!("pod {previous} gone, now {}", next.pod),
            None => format!("pod {}", next.pod),
        });
        *pod = Some(next.pod.clone());
        next
    }

    fn record_error(&self, spec: &Forward, message: String) {
        warn!(id = %spec.id, %message, "connection failed");
        self.events.push(format!("connection failed: {message}"));
        self.set_last_error(message);
    }

    fn set_last_error(&self, message: String) {
        *self.last_error.lock().unwrap() = Some(LastError {
            at: SystemTime::now(),
            message,
        });
    }

    /// Hands out the warm stream if it goes to `target`.
    async fn take_warm(&self, target: &Endpoint) -> Option<PortForward<KubeIo>> {
        let Warm {
            target: warm_target,
            request,
            keeper,
        } = self.warm.lock().unwrap().take()?;
        if warm_target != *target {
            return None;
        }
        let (reply, answer) = oneshot::channel();
        request.send(reply).ok()?;
        let stream = answer.await.ok();
        drop(keeper);
        stream
    }

    fn store_warm(&self, target: Endpoint, stream: PortForward<KubeIo>) {
        if self.stopped.load(Ordering::Relaxed) {
            return;
        }
        let (request, requests) = oneshot::channel();
        let keeper = AbortOnDrop(tokio::spawn(keep_warm(stream, requests)));
        *self.warm.lock().unwrap() = Some(Warm {
            target,
            request,
            keeper,
        });
    }
}

/// A stream opened ahead of time, waiting in its keeper task.
struct Warm {
    target: Endpoint,
    request: oneshot::Sender<oneshot::Sender<PortForward<KubeIo>>>,
    keeper: AbortOnDrop,
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Holds a warm stream until it is requested, becomes unusable or too old.
async fn keep_warm(
    mut stream: PortForward<KubeIo>,
    requests: oneshot::Receiver<oneshot::Sender<PortForward<KubeIo>>>,
) {
    let reply = tokio::select! {
        reply = requests => reply.ok(),
        reason = stream.idle() => {
            debug!(%reason, "warm stream ended");
            None
        }
        () = tokio::time::sleep(WARM_MAX_AGE) => None,
    };
    if let Some(reply) = reply {
        let _ = reply.send(stream);
    }
}

struct Listeners {
    v4: TcpListener,
    /// `localhost` may resolve to ::1 first, as it does for kubectl's
    /// listeners. `None` only where IPv6 is unavailable.
    v6: Option<TcpListener>,
}

impl Listeners {
    async fn accept(&self) -> std::io::Result<(TcpStream, SocketAddr)> {
        match &self.v6 {
            Some(v6) => tokio::select! {
                accepted = self.v4.accept() => accepted,
                accepted = v6.accept() => accepted,
            },
            None => self.v4.accept().await,
        }
    }
}

async fn run(spec: Forward, context: Arc<Context>, shared: Arc<Shared>) {
    let listeners = bind(&spec, &shared).await;
    // Warm streams help clients that open many short connections, like
    // browsers. For other protocols an idle connection to the server is
    // just noise in its logs.
    let warm = speaks_http(&spec.service, spec.remote_port);
    let mut context_state = context.subscribe();
    let mut healthy = context_state.borrow_and_update().is_healthy();
    // The outage took a warm stream, so the recovery opens one again.
    let mut rewarm = false;
    // Owned here, so stopping the forward also ends its connections.
    let mut connections = JoinSet::new();
    let mut failures = AcceptFailures::default();
    loop {
        tokio::select! {
            accepted = listeners.accept() => match accepted {
                Ok((socket, _)) => {
                    failures.accepted(&spec, &shared);
                    // Each answer from the pod is written as it comes, not
                    // held back until the client acknowledged the last one.
                    if let Err(err) = socket.set_nodelay(true) {
                        debug!(id = %spec.id, %err, "cannot set TCP_NODELAY");
                    }
                    connections.spawn(connection(
                        socket,
                        spec.clone(),
                        context.clone(),
                        shared.clone(),
                        warm,
                    ));
                }
                Err(err) => {
                    let pause = failures.failed(&spec, &shared, &err);
                    tokio::time::sleep(pause).await;
                }
            },
            Ok(()) = context_state.changed() => {
                let state = context_state.borrow_and_update().clone();
                if !state.is_healthy() {
                    // Opened before the outage, so most likely dead.
                    let dropped = shared.warm.lock().unwrap().take().is_some();
                    rewarm |= dropped || shared.refilling.load(Ordering::Acquire);
                } else if !healthy {
                    recovered(&spec, &context, &shared, std::mem::take(&mut rewarm));
                }
                if matches!(state, ContextState::Unreachable { .. }) {
                    close_connections(&spec, &shared, &mut connections, &state);
                }
                healthy = state.is_healthy();
            }
            Some(_) = connections.join_next() => {}
        }
    }
}

/// Failed accepts. Running out of file descriptors lasts until connections
/// are closed, so it is reported once, as a warning and an event, and as the
/// last error, which the INFO column of a ready forward shows while it is
/// recent; it stays recent while accepting fails.
#[derive(Default)]
struct AcceptFailures {
    out_of_fds: bool,
}

impl AcceptFailures {
    /// Records a failed accept. Returns how long to wait before the next.
    fn failed(&mut self, spec: &Forward, shared: &Shared, err: &std::io::Error) -> Duration {
        if !out_of_fds(err) {
            warn!(id = %spec.id, %err, "accept failed");
            return ACCEPT_RETRY;
        }
        if !std::mem::replace(&mut self.out_of_fds, true) {
            warn!(id = %spec.id, %err, "cannot accept connections");
            shared.events.push(NO_FDS);
        }
        shared.set_last_error(NO_FDS.to_owned());
        ACCEPT_RETRY_NO_FDS
    }

    fn accepted(&mut self, spec: &Forward, shared: &Shared) {
        if std::mem::take(&mut self.out_of_fds) {
            info!(id = %spec.id, "accepting connections again");
            shared.events.push("accepting connections again");
        }
    }
}

/// Whether the process (EMFILE) or the system (ENFILE) has no file
/// descriptor left.
fn out_of_fds(err: &std::io::Error) -> bool {
    matches!(Errno::from_io_error(err), Some(Errno::MFILE | Errno::NFILE))
}

/// After an outage (docs §6.2, §6.5): a 403 from before is checked afresh
/// by the next port-forward, and a warm stream the outage took is opened
/// again. The context restarted its watches, but the cache keeps what they
/// listed until they list again, so the pod is the one from before the
/// outage; if it is gone, opening the stream fails as any refill may.
fn recovered(spec: &Forward, context: &Arc<Context>, shared: &Arc<Shared>, rewarm: bool) {
    if shared.forbidden.lock().unwrap().take().is_some() {
        debug!(id = %spec.id, "port-forward to be checked again");
    }
    if !rewarm {
        return;
    }
    let Some(endpoints) = context
        .namespace(&spec.namespace)
        .and_then(|cache| cache.endpoints(&spec.service, spec.remote_port).ok())
    else {
        return;
    };
    let target = shared.choose(&endpoints, spec);
    refill(context.clone(), spec, target, shared.clone());
}

/// Ends the open connections once the context is declared unreachable. They
/// go over the API server's connection, which is most likely dead and might
/// not fail for minutes, or ever while neither side sends; closed, clients
/// such as database pools notice and connect again. A login state does not
/// close them: a stream that is open stays authorized.
fn close_connections(
    spec: &Forward,
    shared: &Shared,
    connections: &mut JoinSet<()>,
    state: &ContextState,
) {
    while connections.try_join_next().is_some() {}
    let open = connections.len();
    if open == 0 {
        return;
    }
    connections.abort_all();
    info!(id = %spec.id, open, "connections closed: context unreachable");
    let plural = if open == 1 { "" } else { "s" };
    shared.events.push(format!(
        "{open} connection{plural} closed: API server {}",
        state.summary()
    ));
}

/// Binds the local port, retrying while it is busy. A port busy at the
/// first attempt is tried again after 100 ms before it counts as busy: a
/// process the daemon is starting just then, such as `lsof` or an exec
/// plugin, holds a copy of every listener until it has started, so a
/// listener closed meanwhile keeps its port for a moment.
async fn bind(spec: &Forward, shared: &Shared) -> Listeners {
    let port = spec.local_port;
    let mut first = true;
    loop {
        match listen(spec).await {
            Ok(listeners) => {
                *shared.bind.lock().unwrap() = Bind::Bound;
                info!(id = %spec.id, port, ipv6 = listeners.v6.is_some(), "listening");
                shared.events.push(match listeners.v6 {
                    Some(_) => format!("listening on port {port}"),
                    None => format!("listening on port {port}, 127.0.0.1 only: no IPv6"),
                });
                return listeners;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse && first => {
                debug!(id = %spec.id, port, "port busy, trying again");
                first = false;
                tokio::time::sleep(BIND_FIRST_RETRY).await;
                continue;
            }
            Err(err) => {
                first = false;
                let (failed, event) = failed_bind(port, &err).await;
                let mut bind = shared.bind.lock().unwrap();
                if *bind != failed {
                    warn!(id = %spec.id, port, ?failed, "cannot listen");
                    shared.events.push(event);
                    *bind = failed;
                }
            }
        }
        tokio::time::sleep(BIND_RETRY).await;
    }
}

/// What a failed bind on `port` shows, and its event: a busy port, with the
/// process that holds it if `lsof` can tell, or another error as what it is,
/// e.g. permission denied for a port below 1024 on Linux. Either is tried
/// again, as it may pass.
async fn failed_bind(port: u16, err: &std::io::Error) -> (Bind, String) {
    if err.kind() != std::io::ErrorKind::AddrInUse {
        let error = err.to_string();
        let event = format!("cannot listen on port {port}: {error}");
        return (Bind::Failed { error }, event);
    }
    let holder = port_holder(port).await;
    let event = match &holder {
        Some(holder) => format!("port {port} busy: {holder}"),
        None => format!("port {port} busy"),
    };
    (Bind::Busy { holder }, event)
}

/// Listens on the local port on 127.0.0.1 and ::1. A client may resolve
/// `localhost` to either, so the port is busy if either address is taken:
/// with ::1 left to another process, `localhost` could reach that one while
/// the forward shows ready. The forward then holds neither address.
async fn listen(spec: &Forward) -> std::io::Result<Listeners> {
    let port = spec.local_port;
    let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let v6 = match TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await {
        Ok(v6) => Some(v6),
        Err(err) if ipv6_unavailable(&err) => {
            info!(id = %spec.id, %err, "no IPv6 listener");
            None
        }
        Err(err) => return Err(err),
    };
    Ok(Listeners { v4, v6 })
}

/// Whether binding ::1 failed because this host has no IPv6: the address
/// is not configured, or the address family is not supported at all.
fn ipv6_unavailable(err: &std::io::Error) -> bool {
    matches!(
        Errno::from_io_error(err),
        Some(Errno::ADDRNOTAVAIL | Errno::AFNOSUPPORT)
    )
}

/// Whether a forward could listen on `port` now: nothing listens on it on
/// 127.0.0.1, nor on ::1 unless this host has no IPv6.
pub fn port_is_free(port: u16) -> bool {
    let v4 = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port));
    v4.is_ok()
        && match std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
            Ok(_) => true,
            Err(err) => ipv6_unavailable(&err),
        }
}

/// The process listening on `port`, as "name (pid N)", if `lsof` can tell.
pub(crate) async fn port_holder(port: u16) -> Option<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::process::Command::new("lsof")
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpc"])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let pid = text.lines().find_map(|line| line.strip_prefix('p'))?;
    let command = text.lines().find_map(|line| line.strip_prefix('c'))?;
    Some(format!("{command} (pid {pid})"))
}

async fn connection(
    socket: TcpStream,
    spec: Forward,
    context: Arc<Context>,
    shared: Arc<Shared>,
    warm: bool,
) {
    // Not ready: close right away. The forward's state says why.
    let Some(client) = context.client().cloned() else {
        return;
    };
    if !context.state().is_healthy() {
        debug!(id = %spec.id, "connection closed: context not healthy");
        shared.events.push(format!(
            "connection closed: API server {}",
            context.state().summary()
        ));
        return;
    }
    let Some(cache) = context.namespace(&spec.namespace) else {
        return;
    };
    let endpoints = match cache.endpoints(&spec.service, spec.remote_port) {
        Ok(endpoints) => endpoints,
        Err(target) => {
            debug!(id = %spec.id, ?target, "connection closed: no ready pod");
            let reason = match target {
                Target::Syncing => "starting".to_owned(),
                Target::ServiceNotFound => "service not found".to_owned(),
                Target::PortNotOnService => "port not on service".to_owned(),
                Target::Forbidden(message) => format!("forbidden: {message}"),
                Target::Endpoints { ready, total } => format!("{ready}/{total} pods ready"),
            };
            shared.events.push(format!("connection closed: {reason}"));
            return;
        }
    };
    let target = shared.choose(&endpoints, &spec);

    let started = Instant::now();
    let (stream, was_warm) = match shared.take_warm(&target).await {
        Some(stream) => (stream, true),
        None => match portforward::open(&client, &spec.namespace, &target.pod, target.port).await {
            Ok(stream) => (stream, false),
            Err(err) => {
                open_failed(&spec, &shared, &err);
                return;
            }
        },
    };
    shared.forbidden.lock().unwrap().take();
    if warm {
        refill(context.clone(), &spec, target.clone(), shared.clone());
    }

    shared.active.fetch_add(1, Ordering::Relaxed);
    let _active = Decrement(&shared.active);
    debug!(
        id = %spec.id,
        pod = %target.pod,
        warm = was_warm,
        setup_ms = started.elapsed().as_millis(),
        "connected"
    );
    match stream.pump(socket).await {
        Ok(transfer) => debug!(
            id = %spec.id,
            sent = transfer.sent,
            received = transfer.received,
            "closed"
        ),
        Err(err) => shared.record_error(&spec, err.to_string()),
    }
}

fn open_failed(spec: &Forward, shared: &Shared, err: &PortForwardError) {
    if !forbidden(spec, shared, err) {
        shared.record_error(spec, err.to_string());
    }
}

/// Remembers a port-forward the API server refused with 403, so the target
/// shows it until a port-forward works or the context recovers. Returns
/// whether it was one.
fn forbidden(spec: &Forward, shared: &Shared, err: &PortForwardError) -> bool {
    let PortForwardError::Connect(kube_err) = err else {
        return false;
    };
    let Failure::Forbidden(error) = classify(kube_err) else {
        return false;
    };
    // kube drops the API server's text when it refuses the upgrade, so
    // its error has only the status line; what was forbidden is known
    // here.
    let message = format!("port-forward in namespace {}", spec.namespace);
    warn!(id = %spec.id, %message, %error, "port-forward forbidden");
    shared.events.push(format!("forbidden: {message}"));
    *shared.forbidden.lock().unwrap() = Some(message);
    true
}

/// Opens the next warm stream in the background, unless one is already being
/// opened (a browser opens several connections at once). It is kept only
/// while the context is healthy: one that opened as an outage began goes over
/// a connection that is most likely dead.
fn refill(context: Arc<Context>, spec: &Forward, target: Endpoint, shared: Arc<Shared>) {
    let Some(client) = context.client().cloned() else {
        return;
    };
    if shared.refilling.swap(true, Ordering::AcqRel) {
        return;
    }
    let spec = spec.clone();
    tokio::spawn(async move {
        match portforward::open(&client, &spec.namespace, &target.pod, target.port).await {
            Ok(_) if !context.state().is_healthy() => {
                debug!(id = %spec.id, "warm stream dropped: context not healthy");
            }
            Ok(stream) => shared.store_warm(target, stream),
            Err(err) => {
                if !forbidden(&spec, &shared, &err) {
                    debug!(id = %spec.id, %err, "cannot open warm stream");
                }
            }
        }
        shared.refilling.store(false, Ordering::Release);
    });
}

struct Decrement<'a>(&'a AtomicU32);

impl Drop for Decrement<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use k8s_openapi::api::discovery::v1::EndpointSlice;
    use kprtfwd_core::ForwardId;
    use kprtfwd_core::state::NetCause;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::cache::NamespaceCache;
    use crate::fake::{self, FakeApi, Kubelet, Reply, closed_port, free_port, slice, status};

    const CONTAINER_PORT: u16 = 8080;

    fn healthy() -> ContextState {
        ContextState::Healthy {
            server_version: "v1.33.0".into(),
        }
    }

    fn unreachable() -> ContextState {
        ContextState::Unreachable {
            since: SystemTime::now(),
            cause: NetCause::Refused,
        }
    }

    fn echo(_: &fake::Request) -> Reply {
        Reply::Kubelet(Kubelet::Echo)
    }

    fn spec(service: &str, remote_port: u16, local_port: u16) -> Forward {
        Forward {
            id: ForwardId::new(format!("test.shop.{service}-{remote_port}")),
            context: "test".into(),
            namespace: "shop".into(),
            service: service.into(),
            remote_port,
            local_port,
            enabled: true,
        }
    }

    fn endpoint(pod: &str) -> Endpoint {
        Endpoint {
            pod: pod.into(),
            port: CONTAINER_PORT,
        }
    }

    /// The service's pods, with the container port of its port "web".
    fn pods(service: &str, ready: &[(&str, bool)]) -> Vec<EndpointSlice> {
        let endpoints: Vec<(&str, Option<bool>)> = ready
            .iter()
            .map(|(pod, ready)| (*pod, Some(*ready)))
            .collect();
        vec![slice(
            service,
            service,
            &[("web", CONTAINER_PORT)],
            &endpoints,
        )]
    }

    /// A forward in a healthy fake context, through a fake API server.
    struct Test {
        api: FakeApi,
        context: Arc<Context>,
        cache: Arc<NamespaceCache>,
        events: EventLog,
        handle: ForwardHandle,
        port: u16,
    }

    impl Test {
        /// Forwards port `remote_port` (named "web") of `service`.
        async fn start(
            service: &str,
            remote_port: u16,
            ready: &[(&str, bool)],
            reply: impl Fn(&fake::Request) -> Reply + Send + Sync + 'static,
        ) -> Self {
            let api = FakeApi::start(reply).await;
            let context = Context::fake(api.client(), healthy());
            let cache = NamespaceCache::listed(
                "shop",
                vec![fake::service(service, &[("web", remote_port)])],
                pods(service, ready),
            );
            context.put_namespace("shop", cache.clone());
            // Another test's socket may take a free port before the forward
            // binds it. Then the forward starts again on another port, so
            // its events show one clean start.
            for _ in 0..5 {
                let port = free_port();
                let events = EventLog::default();
                let handle = ForwardHandle::start(
                    spec(service, remote_port, port),
                    context.clone(),
                    events.clone(),
                );
                if first_bind(&handle, &context).await == Bind::Bound {
                    return Self {
                        api,
                        context,
                        cache,
                        events,
                        handle,
                        port,
                    };
                }
                handle.stop();
            }
            panic!("no free port in five attempts");
        }

        fn observe(&self) -> Observed {
            self.handle.observe(&self.context)
        }

        fn events(&self) -> Vec<String> {
            messages(&self.events)
        }

        /// The numbers of the port-forward streams still open.
        fn open_streams(&self) -> Vec<usize> {
            self.api
                .streams()
                .into_iter()
                .filter(|stream| stream.open)
                .map(|stream| stream.number)
                .collect()
        }

        /// The port-forward requests so far; a recovery also restarts the
        /// watches, whose requests do not count.
        fn port_forwards(&self) -> usize {
            self.api
                .requests()
                .iter()
                .filter(|request| request.contains("/portforward"))
                .count()
        }

        /// A warm stream is stored and no other is being opened.
        fn warm_ready(&self) -> bool {
            self.handle.shared.warm.lock().unwrap().is_some()
                && !self.handle.shared.refilling.load(Ordering::Acquire)
        }

        async fn wait(&self, what: &str, done: impl Fn(&Self) -> bool) {
            let started = Instant::now();
            while !done(self) {
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "not {what}: {:?}, {:?}, {:?}",
                    self.observe(),
                    self.events(),
                    self.api.streams()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        /// Sends `text` on a new connection to 127.0.0.1.
        async fn send(&self, text: &str) -> String {
            self.send_to("127.0.0.1", text).await
        }

        /// The kubelet's answer to `text`, or "" if the forward closed the
        /// connection.
        async fn send_to(&self, host: &str, text: &str) -> String {
            let mut socket = TcpStream::connect((host, self.port)).await.unwrap();
            let _ = socket.write_all(text.as_bytes()).await;
            read_answer(&mut socket, text).await
        }
    }

    /// The outcome of the forward's first attempt to bind its port.
    async fn first_bind(handle: &ForwardHandle, context: &Context) -> Bind {
        let started = Instant::now();
        loop {
            let bind = handle.observe(context).bind;
            if bind != Bind::Pending {
                return bind;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "no bind attempt"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn messages(events: &EventLog) -> Vec<String> {
        events
            .entries()
            .into_iter()
            .map(|event| event.message)
            .collect()
    }

    /// Reads until the answer ends with `text` or the connection closes.
    async fn read_answer(socket: &mut TcpStream, text: &str) -> String {
        let mut answer = Vec::new();
        let mut chunk = [0; 1024];
        let read = async {
            loop {
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        answer.extend_from_slice(&chunk[..n]);
                        if answer.ends_with(text.as_bytes()) {
                            break;
                        }
                    }
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .expect("an answer or a close within 5 s");
        String::from_utf8(answer).unwrap()
    }

    #[test]
    fn only_a_missing_ipv6_stack_lets_a_forward_skip_ipv6() {
        let error = |errno: Errno| std::io::Error::from_raw_os_error(errno.raw_os_error());
        assert!(ipv6_unavailable(&error(Errno::ADDRNOTAVAIL)));
        assert!(ipv6_unavailable(&error(Errno::AFNOSUPPORT)));
        for busy in [Errno::ADDRINUSE, Errno::ACCESS, Errno::MFILE] {
            assert!(!ipv6_unavailable(&error(busy)), "{busy:?}");
        }
        assert!(!ipv6_unavailable(&std::io::Error::other("other")));
    }

    #[test]
    fn a_port_is_free_only_on_both_loopback_addresses() {
        let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(!port_is_free(v4.local_addr().unwrap().port()));
        if let Ok(v6) = std::net::TcpListener::bind("[::1]:0") {
            assert!(!port_is_free(v6.local_addr().unwrap().port()));
        }
        // Not a port just closed, which a process started by another test
        // may still hold (see `fake::held_by_another_process`).
        assert!(port_is_free(closed_port()));
    }

    /// Closing a forward frees its port at once, without the retry of a
    /// forward bound next.
    #[tokio::test]
    async fn closing_a_forward_frees_its_port() {
        let api = FakeApi::start(echo).await;
        let context = Context::fake(api.client(), healthy());
        for _ in 0..3 {
            // Not one of port 0, which another socket may take once free.
            let port = closed_port();
            let spec = spec("db", 5432, port);
            let handle = ForwardHandle::start(spec, context.clone(), EventLog::default());
            assert_eq!(first_bind(&handle, &context).await, Bind::Bound);
            handle.close().await;
            if fake::freed(port, port_is_free) {
                return;
            }
            assert!(
                fake::held_by_another_process(port).await,
                "port {port} still held"
            );
        }
        panic!("ports held by other processes in three attempts");
    }

    #[test]
    fn running_out_of_file_descriptors_shows_once_and_slows_accepting() {
        let events = EventLog::default();
        let shared = Shared::new(events.clone());
        let spec = spec("db", 5432, 15432);
        let mut failures = AcceptFailures::default();
        let error = |errno: Errno| std::io::Error::from_raw_os_error(errno.raw_os_error());
        let last_error = || shared.last_error.lock().unwrap().clone().unwrap();

        // A client that gave up before the accept: a short pause, no event.
        let aborted = error(Errno::CONNABORTED);
        assert_eq!(failures.failed(&spec, &shared, &aborted), ACCEPT_RETRY);
        assert!(messages(&events).is_empty());
        assert!(shared.last_error.lock().unwrap().is_none());

        assert_eq!(
            failures.failed(&spec, &shared, &error(Errno::MFILE)),
            ACCEPT_RETRY_NO_FDS
        );
        let first = last_error();
        assert_eq!(first.message, NO_FDS);
        std::thread::sleep(Duration::from_millis(5));
        for errno in [Errno::MFILE, Errno::NFILE] {
            let pause = failures.failed(&spec, &shared, &error(errno));
            assert_eq!(pause, ACCEPT_RETRY_NO_FDS);
        }
        assert_eq!(messages(&events), [NO_FDS]);
        // Kept recent, so the INFO column shows it while it lasts.
        assert!(last_error().at > first.at);

        failures.accepted(&spec, &shared);
        failures.accepted(&spec, &shared);
        assert_eq!(messages(&events), [NO_FDS, "accepting connections again"]);
        failures.failed(&spec, &shared, &error(Errno::MFILE));
        assert_eq!(
            messages(&events),
            [NO_FDS, "accepting connections again", NO_FDS]
        );
    }

    #[test]
    fn stays_on_its_pod_while_it_is_ready() {
        let events = EventLog::default();
        let shared = Shared::new(events.clone());
        let spec = spec("api", 80, 18080);
        let both = [endpoint("api-0"), endpoint("api-1")];
        assert_eq!(shared.choose(&both, &spec), endpoint("api-0"));
        assert_eq!(
            shared.choose(&[endpoint("api-1"), endpoint("api-0")], &spec),
            endpoint("api-0")
        );
        assert_eq!(
            shared.choose(&[endpoint("api-1")], &spec),
            endpoint("api-1")
        );
        assert_eq!(shared.choose(&both, &spec), endpoint("api-1"));
        assert_eq!(
            messages(&events),
            ["pod api-0", "pod api-0 gone, now api-1"]
        );
    }

    #[tokio::test]
    async fn connections_stay_on_one_pod_and_move_when_it_goes() {
        // Not HTTP, so no warm streams.
        let test = Test::start("db", 5432, &[("db-0", true), ("db-1", true)], echo).await;
        assert_eq!(test.send("a").await, "db-0#1:a");
        assert_eq!(test.send("b").await, "db-0#2:b");
        let observed = test.observe();
        assert_eq!(observed.pod.as_deref(), Some("db-0"));
        assert_eq!(observed.target, Target::Endpoints { ready: 2, total: 2 });
        assert!(
            test.api
                .streams()
                .iter()
                .all(|stream| stream.pod == "db-0" && stream.port == CONTAINER_PORT),
            "{:?}",
            test.api.streams()
        );

        test.cache
            .set_slices(pods("db", &[("db-0", false), ("db-1", true)]));
        assert_eq!(test.send("c").await, "db-1#3:c");
        // Ready again, the old pod does not get the connections back.
        test.cache
            .set_slices(pods("db", &[("db-0", true), ("db-1", true)]));
        assert_eq!(test.send("d").await, "db-1#4:d");
        assert_eq!(
            test.events(),
            [
                "started".to_owned(),
                format!("listening on port {}", test.port),
                "pod db-0".to_owned(),
                "pod db-0 gone, now db-1".to_owned(),
            ]
        );
        assert_eq!(test.handle.last_error(), None);
    }

    #[tokio::test]
    async fn counts_open_connections_and_listens_on_both_loopback_addresses() {
        let test = Test::start("db", 5432, &[("db-0", true)], echo).await;
        let mut socket = TcpStream::connect(("127.0.0.1", test.port)).await.unwrap();
        socket.write_all(b"a").await.unwrap();
        assert_eq!(read_answer(&mut socket, "a").await, "db-0#1:a");
        assert_eq!(test.observe().active_conns, 1);
        drop(socket);
        test.wait("no connection", |t| t.observe().active_conns == 0)
            .await;

        // Clients may resolve localhost to ::1 first.
        if std::net::TcpListener::bind("[::1]:0").is_ok() {
            assert_eq!(test.send_to("::1", "b").await, "db-0#2:b");
        }
    }

    #[tokio::test]
    async fn connections_are_closed_at_once_while_not_ready() {
        let test = Test::start("db", 5432, &[("db-0", false), ("db-1", false)], echo).await;
        assert_eq!(test.send("a").await, "");
        test.cache.set_services(vec![]);
        assert_eq!(test.send("b").await, "");
        test.context
            .put_namespace("shop", NamespaceCache::new("shop"));
        assert_eq!(test.send("c").await, "");
        test.context.force_state(unreachable());
        assert_eq!(test.send("d").await, "");

        assert!(test.api.requests().is_empty(), "{:?}", test.api.requests());
        let closed: Vec<String> = test
            .events()
            .into_iter()
            .filter(|message| message.starts_with("connection closed"))
            .collect();
        assert_eq!(
            closed,
            [
                "connection closed: 0/2 pods ready",
                "connection closed: service not found",
                "connection closed: starting",
                "connection closed: API server unreachable (refused)",
            ]
        );
    }

    /// Open connections go over a connection to the API server that is most
    /// likely dead once the context is declared unreachable, so they are
    /// closed then. A login state leaves them open.
    #[tokio::test]
    async fn open_connections_close_when_the_context_turns_unreachable() {
        let test = Test::start("db", 5432, &[("db-0", true)], echo).await;
        let mut first = TcpStream::connect(("127.0.0.1", test.port)).await.unwrap();
        first.write_all(b"a").await.unwrap();
        assert_eq!(read_answer(&mut first, "a").await, "db-0#1:a");
        let mut second = TcpStream::connect(("127.0.0.1", test.port)).await.unwrap();
        second.write_all(b"b").await.unwrap();
        assert_eq!(read_answer(&mut second, "b").await, "db-0#2:b");

        test.context.force_state(ContextState::AuthFailed {
            since: SystemTime::now(),
            message: "token expired".into(),
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        first.write_all(b"c").await.unwrap();
        assert_eq!(read_answer(&mut first, "c").await, "db-0#1:c");

        test.context.force_state(unreachable());
        for socket in [&mut first, &mut second] {
            assert_eq!(read_answer(socket, "never").await, "", "not closed");
        }
        test.wait("no connection", |t| {
            t.observe().active_conns == 0 && t.open_streams().is_empty()
        })
        .await;
        let closed: Vec<String> = test
            .events()
            .into_iter()
            .filter(|message| message.contains("closed"))
            .collect();
        assert_eq!(
            closed,
            ["2 connections closed: API server unreachable (refused)"]
        );
        assert_eq!(test.handle.last_error(), None);

        test.context.force_state(healthy());
        assert_eq!(test.send("d").await, "db-0#3:d");
    }

    #[tokio::test]
    async fn a_forbidden_port_forward_shows_in_the_target_until_one_works() {
        let forbidden = Arc::new(AtomicBool::new(true));
        let reply = {
            let forbidden = forbidden.clone();
            move |_: &fake::Request| {
                if forbidden.load(Ordering::Relaxed) {
                    Reply::Json(403, status(403, "pods \"db-0\" is forbidden"))
                } else {
                    Reply::Kubelet(Kubelet::Echo)
                }
            }
        };
        let test = Test::start("db", 5432, &[("db-0", true)], reply).await;
        assert_eq!(test.send("a").await, "");
        let message = "port-forward in namespace shop";
        assert_eq!(test.observe().target, Target::Forbidden(message.into()));
        assert!(
            test.events().contains(&format!("forbidden: {message}")),
            "{:?}",
            test.events()
        );
        assert_eq!(test.handle.last_error(), None);

        forbidden.store(false, Ordering::Relaxed);
        assert_eq!(test.send("b").await, "db-0#1:b");
        assert_eq!(
            test.observe().target,
            Target::Endpoints { ready: 1, total: 1 }
        );
    }

    /// A 403 from before an outage is checked afresh after it: the target no
    /// longer shows it, and the next port-forward finds whether it holds.
    #[tokio::test]
    async fn a_forbidden_port_forward_is_checked_again_after_an_outage() {
        let forbidden = Arc::new(AtomicBool::new(true));
        let reply = {
            let forbidden = forbidden.clone();
            // Only port-forwards: the watches that a recovery restarts
            // would show a 403 of their own.
            move |request: &fake::Request| {
                if forbidden.load(Ordering::Relaxed) && request.port_forward().is_some() {
                    Reply::Json(403, status(403, "pods \"db-0\" is forbidden"))
                } else {
                    Reply::Kubelet(Kubelet::Echo)
                }
            }
        };
        let test = Test::start("db", 5432, &[("db-0", true)], reply).await;
        let is_forbidden = |t: &Test| matches!(t.observe().target, Target::Forbidden(_));
        assert_eq!(test.send("a").await, "");
        assert!(is_forbidden(&test));

        // A login state is no outage of the API server, but it is not
        // healthy either; the recovery from it checks again too.
        for outage in [unreachable(), ContextState::LoggingIn] {
            test.context.force_state(outage);
            // The forward sees the outage before the recovery.
            tokio::time::sleep(Duration::from_millis(100)).await;
            test.context.force_state(healthy());
            test.wait("checked again", |t| {
                t.observe().target == Target::Endpoints { ready: 1, total: 1 }
            })
            .await;
            assert_eq!(test.send("b").await, "");
            assert!(is_forbidden(&test));
        }

        forbidden.store(false, Ordering::Relaxed);
        test.context.force_state(unreachable());
        tokio::time::sleep(Duration::from_millis(100)).await;
        test.context.force_state(healthy());
        test.wait("checked again", |t| !is_forbidden(t)).await;
        assert_eq!(test.send("c").await, "db-0#1:c");
    }

    /// The warm stream that an outage took is opened again after it, which
    /// checks the port-forward at once: a 403 shows without a connection.
    #[tokio::test]
    async fn the_warm_stream_after_an_outage_finds_a_forbidden_port_forward() {
        let forbidden = Arc::new(AtomicBool::new(false));
        let reply = {
            let forbidden = forbidden.clone();
            // Only port-forwards: the watches that a recovery restarts
            // would show a 403 of their own.
            move |request: &fake::Request| {
                if forbidden.load(Ordering::Relaxed) && request.port_forward().is_some() {
                    Reply::Json(403, status(403, "pods \"web-0\" is forbidden"))
                } else {
                    Reply::Kubelet(Kubelet::Echo)
                }
            }
        };
        let test = Test::start("frontend", 80, &[("web-0", true)], reply).await;
        assert_eq!(test.send("a").await, "web-0#1:a");
        test.wait("a warm stream", |t| {
            t.warm_ready() && t.open_streams() == [2]
        })
        .await;

        test.context.force_state(unreachable());
        test.wait("no warm stream", |t| t.open_streams().is_empty())
            .await;
        forbidden.store(true, Ordering::Relaxed);
        test.context.force_state(healthy());
        test.wait("forbidden", |t| {
            matches!(t.observe().target, Target::Forbidden(_))
        })
        .await;
        assert!(test.handle.shared.warm.lock().unwrap().is_none());
        assert_eq!(test.port_forwards(), 3);
        assert_eq!(test.handle.last_error(), None);
    }

    /// A forward that had no warm stream when the outage began opens none
    /// after it, as it would have none without the outage.
    #[tokio::test]
    async fn no_warm_stream_after_an_outage_without_one() {
        let test = Test::start("frontend", 80, &[("web-0", true)], echo).await;
        test.context.force_state(unreachable());
        tokio::time::sleep(Duration::from_millis(100)).await;
        test.context.force_state(healthy());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(test.port_forwards(), 0, "{:?}", test.api.requests());
    }

    #[tokio::test]
    async fn a_warm_stream_opened_during_an_outage_is_not_kept() {
        let api = FakeApi::start(echo).await;
        let context = Context::fake(api.client(), unreachable());
        let shared = Arc::new(Shared::new(EventLog::default()));
        refill(
            context,
            &spec("frontend", 80, 18080),
            endpoint("web-0"),
            shared.clone(),
        );
        let started = Instant::now();
        while shared.refilling.load(Ordering::Acquire) || api.streams().iter().any(|s| s.open) {
            assert!(started.elapsed() < Duration::from_secs(5), "stream kept");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(api.streams().len(), 1);
        assert!(shared.warm.lock().unwrap().is_none());
    }

    /// A client that resets its connection, as one that is killed or closes
    /// with data unread does, ends it without an error of the forward.
    #[tokio::test]
    async fn a_client_that_resets_its_connection_is_no_error() {
        let test = Test::start("db", 5432, &[("db-0", true)], echo).await;
        let mut socket = TcpStream::connect(("127.0.0.1", test.port)).await.unwrap();
        socket.write_all(b"a").await.unwrap();
        assert_eq!(read_answer(&mut socket, "a").await, "db-0#1:a");
        socket.set_zero_linger().unwrap();
        drop(socket);
        test.wait("no connection", |t| {
            t.observe().active_conns == 0 && t.open_streams().is_empty()
        })
        .await;
        assert_eq!(test.handle.last_error(), None);
        assert!(
            !test
                .events()
                .iter()
                .any(|event| event.starts_with("connection failed")),
            "{:?}",
            test.events()
        );
    }

    #[tokio::test]
    async fn the_error_channel_becomes_the_last_error() {
        let text = "error forwarding port 8080 to pod db-0: connection refused";
        let test = Test::start("db", 5432, &[("db-0", true)], move |_| {
            Reply::Kubelet(Kubelet::Error(text.into()))
        })
        .await;
        assert_eq!(test.send("a").await, "");
        test.wait("an error", |t| t.handle.last_error().is_some())
            .await;
        assert_eq!(test.handle.last_error().unwrap().message, text);
        assert!(
            test.events()
                .contains(&format!("connection failed: {text}")),
            "{:?}",
            test.events()
        );
    }

    #[tokio::test]
    async fn a_warm_stream_serves_the_next_connection_to_its_pod() {
        let test = Test::start("frontend", 80, &[("web-0", true), ("web-1", true)], echo).await;
        assert_eq!(test.send("a").await, "web-0#1:a");
        test.wait("a warm stream", |t| {
            t.warm_ready() && t.open_streams() == [2]
        })
        .await;
        assert_eq!(test.send("b").await, "web-0#2:b");
        test.wait("the next warm stream", |t| {
            t.warm_ready() && t.open_streams() == [3]
        })
        .await;

        // The pod goes: its warm stream is dropped, not used.
        test.cache
            .set_slices(pods("frontend", &[("web-0", false), ("web-1", true)]));
        assert_eq!(test.send("c").await, "web-1#4:c");
        test.wait("a warm stream to the other pod", |t| {
            t.warm_ready() && t.open_streams() == [5]
        })
        .await;
        assert_eq!(test.api.streams()[4].pod, "web-1");

        // An outage drops it too, and the recovery opens it again.
        test.context.force_state(unreachable());
        test.wait("no warm stream", |t| t.open_streams().is_empty())
            .await;
        test.context.force_state(healthy());
        test.wait("a warm stream after the outage", |t| {
            t.warm_ready() && t.open_streams() == [6]
        })
        .await;
        assert_eq!(test.api.streams()[5].pod, "web-1");
        assert_eq!(test.send("d").await, "web-1#6:d");

        // So does stopping the forward.
        test.wait("a warm stream", |t| {
            t.warm_ready() && t.open_streams() == [7]
        })
        .await;
        test.handle.stop();
        test.wait("no stream", |t| t.open_streams().is_empty())
            .await;
        assert_eq!(test.events().last().map(String::as_str), Some("stopped"));
    }

    #[tokio::test]
    async fn a_warm_stream_that_receives_data_is_dropped() {
        let opened = Arc::new(AtomicUsize::new(0));
        let reply = {
            let opened = opened.clone();
            move |_: &fake::Request| {
                // The second stream is the first warm one; it talks first.
                if opened.fetch_add(1, Ordering::Relaxed) == 1 {
                    Reply::Kubelet(Kubelet::Greeting("hello".into()))
                } else {
                    Reply::Kubelet(Kubelet::Echo)
                }
            }
        };
        let test = Test::start("frontend", 80, &[("web-0", true)], reply).await;
        assert_eq!(test.send("a").await, "web-0#1:a");
        test.wait("the warm stream to end", |t| {
            t.api.streams().len() == 2 && t.open_streams().is_empty()
        })
        .await;
        assert_eq!(test.send("b").await, "web-0#3:b");
    }

    #[tokio::test]
    async fn a_stopped_forward_stores_no_warm_stream() {
        let api = FakeApi::start(echo).await;
        let shared = Shared::new(EventLog::default());
        shared.stopped.store(true, Ordering::Relaxed);
        let stream = portforward::open(&api.client(), "shop", "web-0", CONTAINER_PORT)
            .await
            .unwrap();
        shared.store_warm(endpoint("web-0"), stream);
        assert!(shared.warm.lock().unwrap().is_none());
        let started = Instant::now();
        while api.streams()[0].open {
            assert!(started.elapsed() < Duration::from_secs(5), "stream kept");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Only a port in use is busy; another error, such as permission denied
    /// for a privileged port on Linux, is shown as what it is.
    #[tokio::test]
    async fn a_bind_error_other_than_a_busy_port_is_no_busy_port() {
        for errno in [Errno::ACCESS, Errno::ADDRNOTAVAIL] {
            let err = std::io::Error::from_raw_os_error(errno.raw_os_error());
            let (bind, event) = failed_bind(80, &err).await;
            assert_eq!(
                bind,
                Bind::Failed {
                    error: err.to_string()
                }
            );
            assert_eq!(event, format!("cannot listen on port 80: {err}"));
        }
    }

    #[tokio::test]
    async fn a_busy_port_is_reported_and_taken_once_free() {
        // Not one of port 0: the forward is to take it once the holder is
        // gone, before any other socket can.
        let holder = std::net::TcpListener::bind(("127.0.0.1", closed_port())).unwrap();
        busy_until_free(holder).await;
    }

    /// `localhost` may resolve to ::1 first, so a port another process
    /// holds there is busy, although 127.0.0.1 is free.
    #[tokio::test]
    async fn a_port_busy_on_ipv6_only_is_busy() {
        let Ok(holder) = std::net::TcpListener::bind(("::1", closed_port())) else {
            return; // no IPv6 here
        };
        busy_until_free(holder).await;
    }

    /// A forward on `holder`'s port reports it busy and takes it once
    /// `holder` is gone. With ::1 taken, it leaves 127.0.0.1 free meanwhile.
    async fn busy_until_free(holder: std::net::TcpListener) {
        let port = holder.local_addr().unwrap().port();
        let api = FakeApi::start(echo).await;
        let context = Context::fake(api.client(), healthy());
        let events = EventLog::default();
        let handle = ForwardHandle::start(spec("db", 5432, port), context.clone(), events.clone());
        let observe = || handle.observe(&context).bind;
        let started = Instant::now();
        assert_ne!(first_bind(&handle, &context).await, Bind::Bound);
        let Bind::Busy { holder: who } = observe() else {
            panic!("{:?}", observe());
        };
        if holder.local_addr().unwrap().is_ipv6() {
            let v4_free = |port| std::net::TcpListener::bind(("127.0.0.1", port)).is_ok();
            assert!(fake::freed(port, v4_free), "127.0.0.1 held while busy");
        }
        // lsof names this process, where it is installed.
        let busy = match &who {
            Some(who) => {
                assert!(
                    who.ends_with(&format!("(pid {})", std::process::id())),
                    "{who}"
                );
                format!("port {port} busy: {who}")
            }
            None => format!("port {port} busy"),
        };
        assert_eq!(messages(&events), ["started".to_owned(), busy]);

        // The next attempt, 5 s later, takes it.
        drop(holder);
        while observe() != Bind::Bound {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{:?}",
                observe()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            messages(&events).last(),
            Some(&format!("listening on port {port}"))
        );
    }
}
