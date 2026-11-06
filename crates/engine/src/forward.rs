//! One enabled forward: listens on the local port and forwards every
//! connection to a ready pod behind the service.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use kprtfwd_core::Forward;
use kprtfwd_core::state::{Bind, Observed, Target};
use kprtfwd_core::svctype::{ServiceType, classify as service_type};
use kube::Client;
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

    /// Closes the listeners and all connections.
    pub fn stop(&self) {
        if !self.shared.stopped.swap(true, Ordering::Relaxed) {
            self.shared.events.push("stopped");
        }
        self.shared.warm.lock().unwrap().take();
        self.task.abort();
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
    /// listeners; best effort, as IPv6 may be off.
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
    let warm = service_type(&spec.service, spec.remote_port) == ServiceType::Http;
    let mut context_state = context.subscribe();
    // Owned here, so stopping the forward also ends its connections.
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listeners.accept() => match accepted {
                Ok((socket, _)) => {
                    connections.spawn(connection(
                        socket,
                        spec.clone(),
                        context.clone(),
                        shared.clone(),
                        warm,
                    ));
                }
                Err(err) => {
                    warn!(id = %spec.id, %err, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Ok(()) = context_state.changed() => {
                if !context_state.borrow().is_healthy() {
                    // Opened before the outage, so most likely dead.
                    shared.warm.lock().unwrap().take();
                }
            }
            Some(_) = connections.join_next() => {}
        }
    }
}

/// Binds the local port, retrying while it is busy.
async fn bind(spec: &Forward, shared: &Shared) -> Listeners {
    let port = spec.local_port;
    loop {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            Ok(v4) => {
                let v6 = TcpListener::bind((Ipv6Addr::LOCALHOST, port))
                    .await
                    .inspect_err(|err| debug!(id = %spec.id, %err, "no IPv6 listener"))
                    .ok();
                *shared.bind.lock().unwrap() = Bind::Bound;
                info!(id = %spec.id, port, "listening");
                shared.events.push(format!("listening on port {port}"));
                return Listeners { v4, v6 };
            }
            Err(err) => {
                let holder = if err.kind() == std::io::ErrorKind::AddrInUse {
                    port_holder(port).await
                } else {
                    Some(err.to_string())
                };
                let busy = Bind::Busy { holder };
                let mut bind = shared.bind.lock().unwrap();
                if *bind != busy {
                    warn!(id = %spec.id, port, ?busy, "cannot listen");
                    if let Bind::Busy { holder } = &busy {
                        shared.events.push(match holder {
                            Some(holder) => format!("port {port} busy: {holder}"),
                            None => format!("port {port} busy"),
                        });
                    }
                    *bind = busy;
                }
            }
        }
        tokio::time::sleep(BIND_RETRY).await;
    }
}

/// The process listening on `port`, as "name (pid N)", if `lsof` can tell.
async fn port_holder(port: u16) -> Option<String> {
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
        refill(
            client,
            spec.namespace.clone(),
            target.clone(),
            shared.clone(),
        );
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
    if let PortForwardError::Connect(kube_err) = err
        && let Failure::Forbidden(message) = classify(kube_err)
    {
        warn!(id = %spec.id, %message, "port-forward forbidden");
        shared
            .events
            .push(format!("port-forward forbidden: {message}"));
        *shared.forbidden.lock().unwrap() = Some(message);
        return;
    }
    shared.record_error(spec, err.to_string());
}

/// Opens the next warm stream in the background, unless one is already being
/// opened (a browser opens several connections at once).
fn refill(client: Client, namespace: String, target: Endpoint, shared: Arc<Shared>) {
    if shared.refilling.swap(true, Ordering::AcqRel) {
        return;
    }
    tokio::spawn(async move {
        match portforward::open(&client, &namespace, &target.pod, target.port).await {
            Ok(stream) => shared.store_warm(target, stream),
            Err(err) => debug!(%err, "cannot open warm stream"),
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
    use kprtfwd_core::state::{ContextState, NetCause};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::cache::NamespaceCache;
    use crate::fake::{self, FakeApi, Kubelet, Reply, free_port, slice, status};

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
            let port = free_port();
            let events = EventLog::default();
            let handle = ForwardHandle::start(
                spec(service, remote_port, port),
                context.clone(),
                events.clone(),
            );
            let test = Self {
                api,
                context,
                cache,
                events,
                handle,
                port,
            };
            test.wait("listening", |t| t.observe().bind == Bind::Bound)
                .await;
            test
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
        let message =
            "failed to upgrade to a WebSocket connection: failed to switch protocol: 403 Forbidden";
        assert_eq!(test.observe().target, Target::Forbidden(message.into()));
        assert!(
            test.events()
                .contains(&format!("port-forward forbidden: {message}")),
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

        // An outage drops it too.
        test.context.force_state(unreachable());
        test.wait("no warm stream", |t| t.open_streams().is_empty())
            .await;
        test.context.force_state(healthy());
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

    #[tokio::test]
    async fn a_busy_port_is_reported_and_taken_once_free() {
        let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = holder.local_addr().unwrap().port();
        let api = FakeApi::start(echo).await;
        let context = Context::fake(api.client(), healthy());
        let events = EventLog::default();
        let handle = ForwardHandle::start(spec("db", 5432, port), context.clone(), events.clone());
        let observe = || handle.observe(&context).bind;
        let started = Instant::now();
        while observe() == Bind::Pending {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "no bind attempt"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let Bind::Busy { holder: who } = observe() else {
            panic!("{:?}", observe());
        };
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
