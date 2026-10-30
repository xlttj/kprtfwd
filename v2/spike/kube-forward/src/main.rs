//! Spike: native Kubernetes service port-forward without kubectl.
//!
//! Each local connection is resolved to a ready pod through the service's
//! EndpointSlices and gets its own port-forward stream. A pod restart therefore
//! only affects the connections open at that moment; the local listener stays up
//! and the next connection goes to a ready pod. A background probe tracks whether
//! the API server is reachable (VPN), and connections are refused fast while it
//! is not, instead of the forward dying and burning retries.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use k8s_openapi::api::{
    core::v1::{Pod, Service},
    discovery::v1::EndpointSlice,
};
use futures::StreamExt;
use kube::{
    Api, Client, Config,
    api::Portforwarder,
    config::KubeConfigOptions,
    runtime::{
        WatchStreamExt,
        reflector::{self, Store},
        watcher,
    },
};
use tokio::{
    io::copy_bidirectional,
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tracing::{info, warn};

#[derive(Parser, Debug)]
struct Args {
    /// Kubeconfig context to use
    #[arg(long)]
    context: String,
    #[arg(long, short)]
    namespace: String,
    #[arg(long, short)]
    service: String,
    /// Service port, as listed in the Service spec
    #[arg(long)]
    remote_port: i32,
    /// Local port to listen on (127.0.0.1)
    #[arg(long, default_value_t = 0)]
    local_port: u16,
    /// Seconds between API server health probes
    #[arg(long, default_value_t = 5)]
    health_interval: u64,
    /// Resolve the service to a pod and port, print it and exit
    #[arg(long)]
    resolve_only: bool,
    /// Keep one port-forward open in advance, so a new connection skips the
    /// TCP/TLS/WebSocket handshake to the API server
    #[arg(long)]
    warm: bool,
}

struct Shared {
    client: Client,
    args: Args,
    port_name: String,
    slices: Store<EndpointSlice>,
    healthy: AtomicBool,
    next: AtomicUsize,
    last_pod: Mutex<Option<String>>,
    warm: Mutex<Option<Warm>>,
}

/// A port-forward opened ahead of time, handed to the next local connection.
struct Warm {
    pod: String,
    port: u16,
    pf: Portforwarder,
    opened: Instant,
}

/// A warm forward older than this is replaced instead of used, in case the
/// API server or a proxy dropped the idle WebSocket.
const WARM_MAX_AGE: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct Target {
    pod: String,
    port: u16,
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Looks up the name of the service port. EndpointSlices list ports by that
/// name, which is how named target ports get resolved to container ports.
async fn service_port_name(client: &Client, a: &Args) -> Result<String> {
    let services: Api<Service> = Api::namespaced(client.clone(), &a.namespace);
    let svc = services
        .get(&a.service)
        .await
        .with_context(|| format!("get service {}/{}", a.namespace, a.service))?;
    let svc_port = svc
        .spec
        .as_ref()
        .and_then(|s| s.ports.as_ref())
        .and_then(|ports| ports.iter().find(|p| p.port == a.remote_port))
        .ok_or_else(|| anyhow!("service {} has no port {}", a.service, a.remote_port))?;
    Ok(svc_port.name.clone().unwrap_or_default())
}

/// Picks a ready pod and its container port from the watched EndpointSlices.
/// No API call per connection: the reflector keeps the slices current.
fn resolve(shared: &Shared) -> Result<Target> {
    let a = &shared.args;
    let mut candidates = ready_targets(shared);
    if candidates.is_empty() {
        bail!("no ready pod behind {}/{} port {}", a.namespace, a.service, a.remote_port);
    }
    let i = shared.next.fetch_add(1, Ordering::Relaxed) % candidates.len();
    let (pod, port) = candidates.swap_remove(i);
    Ok(Target { pod, port: u16::try_from(port)? })
}

/// All ready (pod, container port) pairs behind the service port.
fn ready_targets(shared: &Shared) -> Vec<(String, i32)> {
    let mut candidates: Vec<(String, i32)> = Vec::new();
    for slice in shared.slices.state() {
        let Some(port) = slice
            .ports
            .iter()
            .flatten()
            .find(|p| p.name.clone().unwrap_or_default() == shared.port_name)
            .and_then(|p| p.port)
        else {
            continue;
        };
        for ep in slice.endpoints.iter().flatten() {
            // A nil "ready" means unknown, which consumers should treat as ready.
            let ready = ep.conditions.as_ref().and_then(|c| c.ready).unwrap_or(true);
            let pod = ep
                .target_ref
                .as_ref()
                .filter(|r| r.kind.as_deref() == Some("Pod"))
                .and_then(|r| r.name.clone());
            if let (true, Some(pod)) = (ready, pod) {
                candidates.push((pod, port));
            }
        }
    }
    // Dual-stack clusters list the same pod in an IPv4 and an IPv6 slice.
    candidates.sort();
    candidates.dedup();
    candidates
}

async fn open(shared: &Shared, pod: &str, port: u16) -> Result<Portforwarder> {
    let pods: Api<Pod> = Api::namespaced(shared.client.clone(), &shared.args.namespace);
    timeout(CONNECT_TIMEOUT, pods.portforward(pod, &[port]))
        .await
        .map_err(|_| anyhow!("port-forward to {pod} timed out"))?
        .with_context(|| format!("port-forward to pod {pod}"))
}

/// Opens the next warm forward in the background.
fn refill(shared: Arc<Shared>) {
    if !shared.args.warm {
        return;
    }
    tokio::spawn(async move {
        let Ok(target) = resolve(&shared) else { return };
        match open(&shared, &target.pod, target.port).await {
            Ok(pf) => {
                let warm = Warm { pod: target.pod, port: target.port, pf, opened: Instant::now() };
                if let Some(old) = shared.warm.lock().unwrap().replace(warm) {
                    old.pf.abort();
                }
            }
            Err(e) => warn!("warm forward: {e:#}"),
        }
    });
}

/// Takes the warm forward if its pod is still ready and it is not too old.
fn take_warm(shared: &Shared) -> Option<Warm> {
    let warm = shared.warm.lock().unwrap().take()?;
    let pod_ready = ready_targets(shared).iter().any(|(pod, _)| *pod == warm.pod);
    if pod_ready && warm.opened.elapsed() < WARM_MAX_AGE {
        return Some(warm);
    }
    warm.pf.abort();
    None
}

async fn handle(shared: Arc<Shared>, mut local: TcpStream, id: usize) -> Result<()> {
    if !shared.healthy.load(Ordering::Relaxed) {
        bail!("API server unreachable, refusing connection");
    }
    let t0 = Instant::now();
    let (pod, port, mut pf, was_warm) = match take_warm(&shared) {
        Some(w) => (w.pod, w.port, w.pf, true),
        None => {
            let target = resolve(&shared)?;
            let pf = open(&shared, &target.pod, target.port).await?;
            (target.pod, target.port, pf, false)
        }
    };
    refill(shared.clone());

    {
        let mut last = shared.last_pod.lock().unwrap();
        if last.as_deref() != Some(pod.as_str()) {
            info!(conn = id, %pod, previous = ?last.as_deref(), "target pod changed");
            *last = Some(pod.clone());
        }
    }

    let mut upstream = pf
        .take_stream(port)
        .ok_or_else(|| anyhow!("no stream for port {port}"))?;
    let pod_error = pf.take_error(port);
    info!(
        conn = id,
        %pod,
        port,
        warm = was_warm,
        setup_ms = t0.elapsed().as_millis(),
        "connected"
    );

    let copied = copy_bidirectional(&mut local, &mut upstream).await;
    drop(upstream);

    if let Some(err) = pod_error {
        if let Ok(Some(msg)) = timeout(Duration::from_millis(200), err).await {
            warn!(conn = id, "pod reported: {msg}");
        }
    }
    // Both directions are finished, so stopping the forwarder loses no data. A
    // loop that already ended keeps its own result, which join returns.
    tokio::time::sleep(Duration::from_millis(50)).await;
    pf.abort();
    if let Err(e) = pf.join().await {
        if !e.to_string().contains("cancelled") {
            warn!(conn = id, "forwarder: {e}");
        }
    }

    match copied {
        Ok((sent, received)) => info!(
            conn = id,
            sent,
            received,
            secs = format!("{:.1}", t0.elapsed().as_secs_f32()),
            "closed"
        ),
        Err(e) => warn!(conn = id, "stream error: {e}"),
    }
    Ok(())
}

/// Tracks API server reachability and logs every change, the way the daemon
/// would track it per context.
async fn health_loop(shared: Arc<Shared>) {
    let mut last = true;
    loop {
        tokio::time::sleep(Duration::from_secs(shared.args.health_interval)).await;
        let t0 = Instant::now();
        let probe = timeout(PROBE_TIMEOUT, shared.client.apiserver_version()).await;
        let (healthy, detail) = match probe {
            Ok(Ok(_)) => (true, String::new()),
            Ok(Err(e)) => (false, format!("{e:#}")),
            Err(_) => (false, format!("no answer within {PROBE_TIMEOUT:?}")),
        };
        shared.healthy.store(healthy, Ordering::Relaxed);
        if healthy != last {
            if healthy {
                info!(ms = t0.elapsed().as_millis(), "API server reachable again");
                refill(shared.clone());
            } else {
                warn!(%detail, "API server unreachable");
                // A forward opened before the outage is most likely dead.
                if let Some(warm) = shared.warm.lock().unwrap().take() {
                    warm.pf.abort();
                }
            }
            last = healthy;
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).with_ansi(false).init();
    let args = Args::parse();

    let options = KubeConfigOptions { context: Some(args.context.clone()), ..Default::default() };
    let config = Config::from_kubeconfig(&options)
        .await
        .with_context(|| format!("load kubeconfig context {}", args.context))?;
    let client = Client::try_from(config)?;

    let t0 = Instant::now();
    let version = client.apiserver_version().await.context("API server unreachable")?;
    info!(context = %args.context, version = %version.git_version, ms = t0.elapsed().as_millis(), "API server reachable");

    let port_name = service_port_name(&client, &args).await?;

    // Watch the service's EndpointSlices so connections resolve from memory.
    // The watcher retries with backoff on its own, e.g. after a VPN drop.
    let t0 = Instant::now();
    let (slices, writer) = reflector::store();
    let selector = format!("kubernetes.io/service-name={}", args.service);
    let api: Api<EndpointSlice> = Api::namespaced(client.clone(), &args.namespace);
    let events = watcher(api, watcher::Config::default().labels(&selector))
        .default_backoff()
        .reflect(writer);
    tokio::spawn(async move {
        let mut events = std::pin::pin!(events);
        let mut failing = false;
        while let Some(event) = events.next().await {
            match event {
                Err(e) => {
                    if !failing {
                        warn!("endpointslice watch failing: {e}");
                    }
                    failing = true;
                }
                Ok(_) if failing => {
                    info!("endpointslice watch resumed");
                    failing = false;
                }
                Ok(_) => {}
            }
        }
    });
    slices.wait_until_ready().await?;
    info!(ms = t0.elapsed().as_millis(), "endpointslice cache ready");

    let shared = Arc::new(Shared {
        client,
        args,
        port_name,
        slices,
        healthy: AtomicBool::new(true),
        next: AtomicUsize::new(0),
        last_pod: Mutex::new(None),
        warm: Mutex::new(None),
    });

    if shared.args.resolve_only {
        let target = resolve(&shared)?;
        info!(?target, "resolved");
        return Ok(());
    }

    let listener = TcpListener::bind(("127.0.0.1", shared.args.local_port)).await?;
    info!(
        local = %listener.local_addr()?,
        target = format!("{}/{}:{}", shared.args.namespace, shared.args.service, shared.args.remote_port),
        "listening"
    );
    tokio::spawn(health_loop(shared.clone()));
    refill(shared.clone());

    let mut id = 0;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, _) = accepted?;
                id += 1;
                let shared = shared.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(shared, socket, id).await {
                        warn!(conn = id, "{e:#}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                return Ok(());
            }
        }
    }
}
