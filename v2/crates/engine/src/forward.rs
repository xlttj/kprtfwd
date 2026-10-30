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
