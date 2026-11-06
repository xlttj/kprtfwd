//! Tests against a real cluster.
//!
//! They only run when `KPRTFWD_TEST_CONTEXT` names a kubeconfig context of a
//! throwaway cluster (kind in CI); otherwise they pass without doing anything.
//! Each test creates and deletes its own namespace with a two-replica
//! `traefik/whoami` deployment, which answers with its pod name.
//!
//! The outage test also needs `KPRTFWD_TEST_PAUSE` and `KPRTFWD_TEST_RESUME`:
//! shell commands that make the API server unreachable and reachable again,
//! for kind `docker pause <cluster>-control-plane` and `docker unpause ...`.
//! Run with `--test-threads=1`, as pausing the cluster would disturb the
//! other test.

use std::time::{Duration, Instant};

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{Namespace, Pod, Service};
use kprtfwd_core::state::derive;
use kprtfwd_core::{Forward, ForwardId, ForwardState};
use kprtfwd_engine::Engine;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::config::KubeConfigOptions;
use kube::{Api, Client, Config};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

struct TestCluster {
    client: Client,
    context: String,
    namespace: String,
}

impl TestCluster {
    /// Creates the namespace with the test service; `None` without a cluster.
    async fn new(namespace: &str) -> Option<Self> {
        let context = env("KPRTFWD_TEST_CONTEXT")?;
        let options = KubeConfigOptions {
            context: Some(context.clone()),
            ..Default::default()
        };
        let config = Config::from_kubeconfig(&options).await.expect("kubeconfig");
        let cluster = Self {
            client: Client::try_from(config).expect("client"),
            context,
            namespace: namespace.to_owned(),
        };
        cluster.delete_namespace().await; // left over from an aborted run
        cluster.create().await;
        Some(cluster)
    }

    async fn create(&self) {
        let ns = &self.namespace;
        let post = PostParams::default();
        Api::<Namespace>::all(self.client.clone())
            .create(
                &post,
                &serde_json::from_value(json!({"metadata": {"name": ns}})).unwrap(),
            )
            .await
            .expect("create namespace");
        let deployment = json!({
            "metadata": {"name": "whoami"},
            "spec": {
                "replicas": 2,
                "selector": {"matchLabels": {"app": "whoami"}},
                "template": {
                    "metadata": {"labels": {"app": "whoami"}},
                    "spec": {"containers": [{
                        "name": "whoami",
                        "image": "traefik/whoami:v1.10",
                        "ports": [{"name": "http", "containerPort": 80}],
                        "readinessProbe": {"httpGet": {"path": "/", "port": "http"}, "periodSeconds": 1},
                    }]},
                },
            },
        });
        self.deployments()
            .create(&post, &serde_json::from_value(deployment).unwrap())
            .await
            .expect("create deployment");
        // A named target port, so the test covers EndpointSlice port names.
        let service = json!({
            "metadata": {"name": "whoami"},
            "spec": {
                "selector": {"app": "whoami"},
                "ports": [{"name": "web", "port": 80, "targetPort": "http"}],
            },
        });
        Api::<Service>::namespaced(self.client.clone(), ns)
            .create(&post, &serde_json::from_value(service).unwrap())
            .await
            .expect("create service");
        self.wait_for_ready_replicas(2).await;
    }

    fn deployments(&self) -> Api<Deployment> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    async fn wait_for_ready_replicas(&self, want: i32) {
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let deployment = self
                .deployments()
                .get("whoami")
                .await
                .expect("get deployment");
            let ready = deployment
                .status
                .and_then(|s| s.ready_replicas)
                .unwrap_or(0);
            if ready == want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{ready} of {want} replicas ready"
            );
            sleep(Duration::from_millis(500)).await;
        }
    }

    async fn scale(&self, replicas: i32) {
        let patch = json!({"spec": {"replicas": replicas}});
        self.deployments()
            .patch("whoami", &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .expect("scale");
    }

    async fn delete_pod(&self, name: &str) {
        Api::<Pod>::namespaced(self.client.clone(), &self.namespace)
            .delete(name, &DeleteParams::default())
            .await
            .expect("delete pod");
    }

    async fn delete_namespace(&self) {
        let namespaces = Api::<Namespace>::all(self.client.clone());
        if namespaces
            .delete(&self.namespace, &DeleteParams::default())
            .await
            .is_err()
        {
            return; // not there
        }
        let deadline = Instant::now() + Duration::from_secs(120);
        while namespaces
            .get_opt(&self.namespace)
            .await
            .ok()
            .flatten()
            .is_some()
        {
            assert!(
                Instant::now() < deadline,
                "namespace {} not deleted",
                self.namespace
            );
            sleep(Duration::from_millis(500)).await;
        }
    }

    fn forward(&self, id: &str) -> Forward {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("free port")
            .port();
        Forward {
            id: ForwardId::new(id),
            context: self.context.clone(),
            namespace: self.namespace.clone(),
            service: "whoami".into(),
            remote_port: 80,
            local_port: port,
            enabled: true,
        }
    }
}

/// `GET /` through the forward; the pod name whoami answers with.
async fn hostname(port: u16) -> Result<String, String> {
    hostname_at("127.0.0.1", port).await
}

/// [`hostname`] through the forward's listener on `host`.
async fn hostname_at(host: &str, port: u16) -> Result<String, String> {
    let request = async {
        let mut stream = TcpStream::connect((host, port))
            .await
            .map_err(|e| e.to_string())?;
        stream
            .write_all(b"GET / HTTP/1.0\r\n\r\n")
            .await
            .map_err(|e| e.to_string())?;
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .map_err(|e| e.to_string())?;
        response
            .lines()
            .find_map(|line| line.strip_prefix("Hostname: "))
            .map(str::to_owned)
            .ok_or_else(|| format!("no hostname in {response:?}"))
    };
    timeout(Duration::from_secs(15), request)
        .await
        .map_err(|_| "no answer within 15 s".to_owned())?
}

async fn wait_for(
    engine: &Engine,
    forward: &Forward,
    what: &str,
    within: Duration,
    want: impl Fn(&ForwardState) -> bool,
) -> Duration {
    let started = Instant::now();
    loop {
        let state = derive(true, false, engine.observe(&forward.id).as_ref());
        if want(&state) {
            return started.elapsed();
        }
        assert!(
            started.elapsed() < within,
            "not {what} after {within:?}: {state:?}"
        );
        sleep(Duration::from_millis(100)).await;
    }
}

fn ready(state: &ForwardState) -> bool {
    matches!(state, ForwardState::Ready { .. })
}

/// The forward's events.
fn events(engine: &Engine, forward: &Forward) -> Vec<String> {
    engine
        .forward_events(&forward.id)
        .into_iter()
        .map(|event| event.message)
        .collect()
}

fn shell(command: &str) {
    let status = std::process::Command::new("sh")
        .args(["-c", command])
        .status()
        .expect("run shell");
    assert!(status.success(), "{command} failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn follows_pod_restarts_and_scaling() {
    let Some(cluster) = TestCluster::new("kprtfwd-it-pods").await else {
        eprintln!("skipped: KPRTFWD_TEST_CONTEXT is not set");
        return;
    };
    let forward = cluster.forward("it-pods");
    let port = forward.local_port;
    let mut engine = Engine::new();
    engine.reconcile(std::slice::from_ref(&forward)).await;
    wait_for(&engine, &forward, "ready", Duration::from_secs(60), ready).await;

    // Connections stay on one pod, also those to ::1.
    let first = hostname(port).await.expect("first request");
    for _ in 0..5 {
        assert_eq!(hostname(port).await.expect("request"), first);
    }
    if std::net::TcpListener::bind("[::1]:0").is_ok() {
        let pod = hostname_at("::1", port).await.expect("request to ::1");
        assert_eq!(pod, first);
    }

    // When that pod goes, the forward moves to the other one by itself.
    cluster.delete_pod(&first).await;
    let mut failures = 0;
    let mut moved_to = None;
    for _ in 0..100 {
        match hostname(port).await {
            Ok(pod) if pod != first => {
                moved_to = Some(pod);
                break;
            }
            Ok(_) => {}
            Err(err) => {
                eprintln!("request while the pod went away: {err}");
                failures += 1;
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
    let moved_to = moved_to.unwrap_or_else(|| panic!("still on {first}"));
    // Requests in the moment the pod stops may fail; there must not be more.
    assert!(failures <= 3, "{failures} failed requests");
    let seen = events(&engine, &forward);
    assert!(
        seen.contains(&format!("listening on port {port}")),
        "{seen:?}"
    );
    assert!(seen.contains(&format!("pod {first}")), "{seen:?}");
    assert!(
        seen.contains(&format!("pod {first} gone, now {moved_to}")),
        "{seen:?}"
    );

    // Without pods, connections are closed at once and the state says why.
    cluster.scale(0).await;
    wait_for(
        &engine,
        &forward,
        "waiting for pods",
        Duration::from_secs(60),
        |s| matches!(s, ForwardState::WaitingForPods { ready: 0, .. }),
    )
    .await;
    let started = Instant::now();
    assert!(hostname(port).await.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "closing took {:?}",
        started.elapsed()
    );
    let seen = events(&engine, &forward);
    assert!(
        seen.iter()
            .any(|message| message.starts_with("connection closed: 0/")),
        "{seen:?}"
    );

    // Scaled up again, the same forward works without a restart.
    cluster.scale(1).await;
    wait_for(&engine, &forward, "ready", Duration::from_secs(120), ready).await;
    hostname(port).await.expect("request after scaling up");

    engine.shutdown();
    cluster.delete_namespace().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn recovers_from_an_api_server_outage() {
    let (Some(pause), Some(resume)) = (env("KPRTFWD_TEST_PAUSE"), env("KPRTFWD_TEST_RESUME"))
    else {
        eprintln!("skipped: KPRTFWD_TEST_PAUSE and KPRTFWD_TEST_RESUME are not set");
        return;
    };
    let Some(cluster) = TestCluster::new("kprtfwd-it-outage").await else {
        eprintln!("skipped: KPRTFWD_TEST_CONTEXT is not set");
        return;
    };
    let forward = cluster.forward("it-outage");
    let port = forward.local_port;
    let mut engine = Engine::new();
    engine.reconcile(std::slice::from_ref(&forward)).await;
    wait_for(&engine, &forward, "ready", Duration::from_secs(60), ready).await;
    hostname(port).await.expect("request before the outage");

    // Freeze the API server: no answers at all, like a VPN that went away.
    shell(&pause);
    let detected = wait_for(
        &engine,
        &forward,
        "waiting for the context",
        Duration::from_secs(40),
        |s| *s == ForwardState::WaitingForContext,
    )
    .await;
    eprintln!("outage detected after {detected:?}");

    // Meanwhile connections are closed at once instead of hanging. Resume
    // before checking, so a failure does not leave the cluster frozen.
    let started = Instant::now();
    let during = hostname(port).await;
    let took = started.elapsed();
    shell(&resume);
    assert!(during.is_err(), "a request got through during the outage");
    assert!(took < Duration::from_secs(2), "closing took {took:?}");

    let recovered = wait_for(&engine, &forward, "ready", Duration::from_secs(60), ready).await;
    eprintln!("recovered after {recovered:?}");
    let mut last = Err(String::new());
    for _ in 0..20 {
        last = hostname(port).await;
        if last.is_ok() {
            break;
        }
        sleep(Duration::from_millis(500)).await;
    }
    last.expect("request after the outage");

    engine.shutdown();
    cluster.delete_namespace().await;
}
