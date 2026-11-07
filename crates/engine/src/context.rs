//! One kubeconfig context: its client, the health of its API server, its
//! login, and the watch caches of the namespaces its forwards use.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

use futures::FutureExt;
use futures::future::BoxFuture;
use http_body_util::BodyExt;
use hyper::{Request, Response};
use hyper_timeout::TimeoutConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use kprtfwd_core::state::{ContextState, NetCause};
use kube::client::retry::RetryPolicy;
use kube::client::{ClientBuilder, ConfigExt, DynBody};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Client, Config};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tower::retry::RetryLayer;
use tower::util::BoxService;
use tower::{BoxError, ServiceBuilder, ServiceExt};
use tracing::{info, warn};

use crate::auth::{self, Login};
use crate::cache::NamespaceCache;
use crate::errors::{Failure, classify};
use crate::events::EventLog;
use crate::kubeconfig;

pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_INTERVAL_HEALTHY: Duration = Duration::from_secs(5);
const PROBE_INTERVAL_UNHEALTHY: Duration = Duration::from_secs(2);
/// A healthy context counts as unreachable only after this many failed
/// probes in a row that may pass by themselves: timeouts, resets and failures
/// without a known cause. One slow answer or one reset over VPN is not an
/// outage. Definite failures (DNS, refused, TLS) count at once.
const FAILURES_UNTIL_UNREACHABLE: u32 = 2;

/// How long a probe may take and how often it runs; shorter in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Timing {
    pub probe_timeout: Duration,
    pub healthy_interval: Duration,
    pub unhealthy_interval: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            probe_timeout: PROBE_TIMEOUT,
            healthy_interval: PROBE_INTERVAL_HEALTHY,
            unhealthy_interval: PROBE_INTERVAL_UNHEALTHY,
        }
    }
}

pub struct Context {
    name: String,
    client: Option<Client>,
    /// Set if the context logs in through an exec plugin. Only the health
    /// probe runs it; see [`crate::auth`].
    login: Option<Login>,
    state: watch::Sender<ContextState>,
    namespaces: Mutex<HashMap<String, Arc<NamespaceCache>>>,
    health: Mutex<Option<JoinHandle<()>>>,
    events: EventLog,
    timing: Timing,
}

/// The settings for a context from `kubeconfig`, or from the kubeconfig
/// files if that is `None`.
async fn load(context: &str, kubeconfig: Option<&Kubeconfig>) -> Result<Config, String> {
    let options = KubeConfigOptions {
        context: Some(context.to_owned()),
        ..Default::default()
    };
    let kubeconfig = match kubeconfig {
        Some(kubeconfig) => kubeconfig.clone(),
        None => kubeconfig::read()?,
    };
    Config::from_custom_kubeconfig(kubeconfig, &options)
        .await
        .map_err(|err| kubeconfig::describe(&err))
}

/// A client for a kubeconfig context, and its login if it logs in through an
/// exec plugin. The client never runs the plugin: it sends the token of the
/// last [`Login::run`], or no credentials.
pub async fn client_for(context: &str) -> Result<(Client, Option<Login>), String> {
    build(load(context, None).await?)
}

fn build(mut config: Config) -> Result<(Client, Option<Login>), String> {
    let login = Login::take(&mut config.auth_info);
    let builder = client_builder(config).map_err(|err| err.to_string())?;
    let client = match &login {
        Some(login) => builder.with_layer(&login.layer()).build(),
        None => builder.build(),
    };
    Ok((client, login))
}

/// TCP keepalive on the connections to the API server. A connection whose
/// path died silently, e.g. as the VPN came back with another address before
/// the probe noticed the outage, fails about 30 s after its last packet
/// instead of hanging; that is no quicker than the probe declares an outage.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(15);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
const KEEPALIVE_PROBES: u32 = 3;

/// The stack `kube::client::ClientBuilder` builds by default.
type Stack = BoxService<Request<kube::client::Body>, Response<Box<DynBody>>, BoxError>;

/// The client stack kube builds with [`ClientBuilder::try_from`], with a
/// TCP connector of its own: kube's has no keepalive and no hook to set one.
/// Its sockets also send at once, as Go's do, so a forwarded client's small
/// writes are not held back until the API server acknowledged the last one.
fn client_builder(config: Config) -> Result<ClientBuilder<Stack>, kube::Error> {
    // kube's builder handles proxies; with the features built, it rejects
    // them.
    if config.proxy_url.is_some() {
        return ClientBuilder::try_from(config);
    }
    let mut tcp = HttpConnector::new();
    tcp.enforce_http(false);
    tcp.set_nodelay(true);
    tcp.set_keepalive(Some(KEEPALIVE_IDLE));
    tcp.set_keepalive_interval(Some(KEEPALIVE_INTERVAL));
    tcp.set_keepalive_retries(Some(KEEPALIVE_PROBES));
    let mut connector = TimeoutConnector::new(config.rustls_https_connector_with_connector(tcp)?);
    connector.set_connect_timeout(config.connect_timeout);
    connector.set_read_timeout(config.read_timeout);
    connector.set_write_timeout(config.write_timeout);
    let http = hyper_util::client::legacy::Builder::new(TokioExecutor::new()).build(connector);
    let retry = config
        .default_retry
        .then(|| RetryLayer::new(RetryPolicy::server_retry()));
    let service = ServiceBuilder::new()
        .layer(config.base_uri_layer())
        .option_layer(retry)
        .option_layer(config.auth_layer()?)
        .layer(config.extra_headers_layer()?)
        .map_err(BoxError::from)
        .service(http)
        .map_response(|response| {
            response.map(|body| Box::new(body.map_err(BoxError::from)) as Box<DynBody>)
        });
    Ok(ClientBuilder::new(
        BoxService::new(service),
        config.default_namespace,
    ))
}

impl Context {
    /// Builds the client from `kubeconfig`, or from the kubeconfig files if
    /// that is `None`, and starts the health probe.
    pub async fn start(name: &str, kubeconfig: Option<&Kubeconfig>, events: EventLog) -> Arc<Self> {
        Self::from_config(
            name,
            load(name, kubeconfig).await,
            events,
            Timing::default(),
        )
    }

    fn from_config(
        name: &str,
        config: Result<Config, String>,
        events: EventLog,
        timing: Timing,
    ) -> Arc<Self> {
        let (state, client, login) = match config.and_then(build) {
            Ok((client, login)) => (ContextState::Connecting, Some(client), login),
            Err(message) => {
                warn!(context = name, %message, "cannot load context");
                events.push(format!("kubeconfig: {message}"));
                (ContextState::ConfigError { message }, None, None)
            }
        };
        let context = Arc::new(Self {
            name: name.to_owned(),
            client,
            login,
            state: watch::channel(state).0,
            namespaces: Mutex::new(HashMap::new()),
            health: Mutex::new(None),
            events,
            timing,
        });
        if context.client.is_some() {
            let task = tokio::spawn(probe_health(Arc::downgrade(&context)));
            *context.health.lock().unwrap() = Some(task);
        }
        context
    }

    pub fn client(&self) -> Option<&Client> {
        self.client.as_ref()
    }

    pub fn state(&self) -> ContextState {
        self.state.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<ContextState> {
        self.state.subscribe()
    }

    /// The cache of a namespace that forwards use, if it is being watched.
    pub fn namespace(&self, namespace: &str) -> Option<Arc<NamespaceCache>> {
        self.namespaces.lock().unwrap().get(namespace).cloned()
    }

    /// Watches exactly the given namespaces.
    pub fn watch_namespaces(&self, wanted: &HashSet<String>) {
        let Some(client) = &self.client else { return };
        let mut namespaces = self.namespaces.lock().unwrap();
        namespaces.retain(|name, cache| {
            let keep = wanted.contains(name);
            if !keep {
                cache.stop();
            }
            keep
        });
        for name in wanted {
            namespaces.entry(name.clone()).or_insert_with(|| {
                let cache = NamespaceCache::new(name);
                // Without a login yet the watches would only fail; they
                // start once the probe has logged in.
                if self.login.as_ref().is_none_or(Login::has_token) {
                    cache.restart(client);
                }
                cache
            });
        }
    }

    pub fn stop(&self) {
        if let Some(task) = self.health.lock().unwrap().take() {
            task.abort();
        }
        for (_, cache) in self.namespaces.lock().unwrap().drain() {
            cache.stop();
        }
    }

    fn restart_watches(&self) {
        let Some(client) = &self.client else { return };
        for cache in self.namespaces.lock().unwrap().values() {
            cache.restart(client);
        }
    }

    /// Starts a login through the context's plugin; see [`probe_health`].
    fn start_login(&self, client: &Client) -> Option<BoxFuture<'static, Result<(), LoginError>>> {
        let login = self.login.clone()?;
        let plugin = login.name();
        info!(context = %self.name, %plugin, "logging in");
        self.events.push(format!("logging in with {plugin}"));
        let client = client.clone();
        let probe_timeout = self.timing.probe_timeout;
        Some(async move { log_in(&login, &client, probe_timeout).await }.boxed())
    }

    /// Logs a change of state, keeps it in the context's events and
    /// publishes it.
    fn set_state(&self, next: ContextState) {
        let previous = self.state();
        if next == previous {
            return;
        }
        if restarts_watches(self.login.is_some(), &previous, &next) {
            self.restart_watches();
        }
        let event = match &next {
            ContextState::Healthy { server_version } => {
                info!(context = %self.name, %server_version, "API server reachable");
                format!("API server reachable ({server_version})")
            }
            ContextState::LoggingIn => {
                let plugin = self.login.as_ref().map(Login::name).unwrap_or_default();
                info!(context = %self.name, %plugin, "waiting for the login");
                // The same text as the start of the login, so the two make
                // one event when they come together.
                format!("logging in with {plugin}")
            }
            ContextState::AuthFailed { message, .. } => {
                warn!(context = %self.name, %message, "login failed");
                format!("login needed: {message}")
            }
            other => {
                let state = other.summary();
                match other.detail() {
                    Some(cause) => {
                        warn!(context = %self.name, %state, %cause, "API server not usable");
                        format!("API server {state}: {cause}")
                    }
                    None => {
                        warn!(context = %self.name, %state, "API server not usable");
                        format!("API server {state}")
                    }
                }
            }
        };
        self.events.push(event);
        self.state.send_replace(next);
    }
}

#[cfg(test)]
impl Context {
    /// A context in `state` without a health probe; the test sets its state
    /// and caches.
    pub(crate) fn fake(client: Client, state: ContextState) -> Arc<Self> {
        Arc::new(Self {
            name: "test".into(),
            client: Some(client),
            login: None,
            state: watch::channel(state).0,
            namespaces: Mutex::new(HashMap::new()),
            health: Mutex::new(None),
            events: EventLog::default(),
            timing: Timing::default(),
        })
    }

    pub(crate) fn put_namespace(&self, namespace: &str, cache: Arc<NamespaceCache>) {
        self.namespaces
            .lock()
            .unwrap()
            .insert(namespace.to_owned(), cache);
    }

    pub(crate) fn force_state(&self, state: ContextState) {
        self.set_state(state);
    }
}

/// Probes the API server until the context is dropped or stopped, and logs
/// in when the context needs to.
///
/// A login may open the browser, so it starts only here and only while the
/// API server answers. It runs next to the probe, not inside it: a browser
/// login can wait minutes for the user, and an outage in the meantime must
/// still show.
async fn probe_health(weak: Weak<Context>) {
    let mut probed = Probed::default();
    // The login the probe started, while it runs.
    let mut login = None;
    loop {
        let Some(context) = weak.upgrade() else {
            return;
        };
        let Some(client) = context.client.clone() else {
            return;
        };
        let answer = probe(&client, context.timing.probe_timeout).await;
        let (next, log_in) = after_probe(
            &context.state(),
            answer,
            context.login.as_ref(),
            login.is_some(),
            &mut probed,
            SystemTime::now(),
        );
        if log_in {
            login = context.start_login(&client);
        }
        let login_failed =
            context.login.is_some() && matches!(next, ContextState::AuthFailed { .. });
        context.set_state(next);
        if login_failed {
            // Another attempt may open yet another browser tab while nobody
            // is there. Reconnecting the context tries again.
            return;
        }

        let interval = if context.state().is_healthy() {
            context.timing.healthy_interval
        } else {
            context.timing.unhealthy_interval
        };
        drop(context);
        let Some(running) = &mut login else {
            tokio::time::sleep(interval).await;
            continue;
        };
        let ended = tokio::select! {
            ended = running => ended,
            () = tokio::time::sleep(interval) => continue,
        };
        // The login ended; probe again at once.
        login = None;
        match ended {
            // Whether the API server takes the new token.
            Ok(()) => probed.unverified = true,
            // The API server stopped answering while the login waited for
            // its turn; the probe will show it.
            Err(LoginError::Unanswered(_)) => {}
            Err(LoginError::Failed(message)) => {
                if let Some(context) = weak.upgrade() {
                    let now = SystemTime::now();
                    context.set_state(failed(&context.state(), Failure::Auth(message), now));
                }
                return;
            }
        }
    }
}

/// What the probe keeps from one probe to the next.
#[derive(Debug, Default)]
struct Probed {
    /// Failures in a row that may pass; see [`next_state`].
    failures: u32,
    /// Set when a login succeeded, until the API server answers. If it
    /// rejects the new token, another login would only get the same.
    unverified: bool,
}

/// The state after a probe at `now`, and whether to start a login, for a
/// context that logs in through `login`, if it has one, while `logging_in`
/// says whether a login runs.
///
/// A context logs in when the API server answers and the token is missing,
/// due or rejected. While it has no token the server accepts, the login
/// decides whether it is usable, so its state is [`ContextState::LoggingIn`];
/// while the old token still works, it stays healthy. Probes that get no
/// answer change the state as without a login; a login that runs goes on, as
/// the user may be in the middle of it.
fn after_probe(
    previous: &ContextState,
    answer: Result<String, Failure>,
    login: Option<&Login>,
    logging_in: bool,
    probed: &mut Probed,
    now: SystemTime,
) -> (ContextState, bool) {
    let Some(login) = login.filter(|_| answered(&answer)) else {
        return (
            next_state(previous, answer, &mut probed.failures, now),
            false,
        );
    };
    let unverified = std::mem::take(&mut probed.unverified);
    if let Err(Failure::Auth(message)) = &answer {
        login.forget();
        if unverified {
            let message = format!("the API server rejected the login: {message}");
            return (failed(previous, Failure::Auth(message), now), false);
        }
    }
    let log_in = !logging_in && login.needed();
    let next = next_state(previous, answer, &mut probed.failures, now);
    if (logging_in || log_in) && !login.has_token() {
        (ContextState::LoggingIn, log_in)
    } else {
        (next, log_in)
    }
}

/// Why [`log_in`] did not log in.
pub(crate) enum LoginError {
    /// The API server stopped answering while the login waited for its turn.
    Unanswered(Failure),
    /// The plugin failed; the message says why.
    Failed(String),
}

/// Logs in through `login` once no other plugin runs (see [`auth::turn`]).
/// A login may open the browser, so it runs only while the API server
/// answers: the caller has just probed it, and a login that had to wait for
/// its turn probes it again.
pub(crate) async fn log_in(
    login: &Login,
    client: &Client,
    probe_timeout: Duration,
) -> Result<(), LoginError> {
    let turn = auth::turn().await;
    if turn.waited {
        let answer = probe(client, probe_timeout).await;
        if let Err(failure) = &answer
            && !answered(&answer)
        {
            return Err(LoginError::Unanswered(failure.clone()));
        }
    }
    login.run(&turn).await.map_err(LoginError::Failed)
}

/// Whether a change of state restarts the watches: watches of a context with
/// a login start only once it is healthy, and after an outage a watch may be
/// silently dead, so they start fresh instead of waiting for their idle
/// timeout. Called only when the state changed.
fn restarts_watches(has_login: bool, previous: &ContextState, next: &ContextState) -> bool {
    next.is_healthy() && (has_login || !matches!(previous, ContextState::Connecting))
}

/// `GET /version`: the API server's version, or why there is none.
pub(crate) async fn probe(client: &Client, timeout: Duration) -> Result<String, Failure> {
    match tokio::time::timeout(timeout, client.apiserver_version()).await {
        Ok(Ok(info)) => Ok(info.git_version),
        Ok(Err(err)) => Err(classify(&err)),
        Err(_) => Err(Failure::Net(NetCause::Timeout)),
    }
}

/// Whether the API server answered a probe, even if only to refuse it.
pub(crate) fn answered(answer: &Result<String, Failure>) -> bool {
    !matches!(answer, Err(Failure::Net(_) | Failure::Other(_)))
}

/// The state after a probe at `now`. `failures` counts the failures in a
/// row that may pass by themselves (see [`FAILURES_UNTIL_UNREACHABLE`]).
fn next_state(
    previous: &ContextState,
    answer: Result<String, Failure>,
    failures: &mut u32,
    now: SystemTime,
) -> ContextState {
    match answer {
        Ok(server_version) => {
            *failures = 0;
            ContextState::Healthy { server_version }
        }
        Err(failure) if may_pass(&failure) => {
            *failures += 1;
            if previous.is_healthy() && *failures < FAILURES_UNTIL_UNREACHABLE {
                previous.clone()
            } else {
                failed(previous, failure, now)
            }
        }
        Err(failure) => {
            *failures = 0;
            failed(previous, failure, now)
        }
    }
}

/// Whether a failed probe may be a blip: a timeout, a reset or a failure
/// without a known cause, such as a keep-alive connection the server had
/// just closed.
fn may_pass(failure: &Failure) -> bool {
    matches!(
        failure,
        Failure::Net(NetCause::Timeout | NetCause::Other(_)) | Failure::Other(_)
    )
}

/// The state after a probe failed at `now`. Keeps the time the outage
/// started.
fn failed(previous: &ContextState, failure: Failure, now: SystemTime) -> ContextState {
    let since = match previous {
        ContextState::Unreachable { since, .. } | ContextState::AuthFailed { since, .. } => *since,
        _ => now,
    };
    match failure {
        Failure::Net(cause) => ContextState::Unreachable { since, cause },
        Failure::Auth(message) => ContextState::AuthFailed { since, message },
        // The server answered, so it is reachable; /version is just not
        // readable for this user.
        Failure::Forbidden(_) | Failure::NotFound(_) => ContextState::Healthy {
            server_version: "unknown".into(),
        },
        Failure::Other(message) => ContextState::Unreachable {
            since,
            cause: NetCause::Other(message),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    use kube::config::Kubeconfig;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::auth::fake;
    use crate::fake::{FakeApi, Kubelet, Reply, VERSION, closed_port, status};

    /// Probes ten times faster than in use.
    const FAST: Timing = Timing {
        probe_timeout: Duration::from_secs(1),
        healthy_interval: Duration::from_millis(200),
        unhealthy_interval: Duration::from_millis(100),
    };

    /// Answers every request like `/version`: 200 for the token "good", 401
    /// otherwise.
    fn version_for_good_tokens(request: &crate::fake::Request) -> Reply {
        if request.head.contains("authorization: bearer good\r\n") {
            Reply::Json(200, VERSION.into())
        } else {
            Reply::Json(401, status(401, "Unauthorized"))
        }
    }

    /// A context whose exec plugin is [`fake::plugin`] in `mode`.
    async fn context(dir: &Path, port: u16, mode: &str) -> Arc<Context> {
        context_with(dir, port, mode, &[]).await
    }

    /// [`context`] with more variables for the plugin.
    async fn context_with(dir: &Path, port: u16, mode: &str, env: &[(&str, &str)]) -> Arc<Context> {
        // Contexts of one test share the plugin; rewriting it could break a
        // run of it.
        let plugin = dir.join("plugin");
        if !plugin.exists() {
            fake::plugin(dir);
        }
        let env: String = env
            .iter()
            .map(|(name, value)| format!("\n      - name: {name}\n        value: {value}"))
            .collect();
        let yaml = format!(
            r#"
apiVersion: v1
kind: Config
clusters:
- name: test
  cluster:
    server: http://127.0.0.1:{port}
users:
- name: test
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1
      command: {plugin}
      args: [get-token, --client-secret=s3cr3t]
      env:
      - name: RUNS
        value: {runs}
      - name: MODE
        value: {mode}{env}
contexts:
- name: test
  context:
    cluster: test
    user: test
current-context: test
"#,
            plugin = plugin.display(),
            runs = dir.join("runs").display(),
        );
        let kubeconfig = Kubeconfig::from_yaml(&yaml).unwrap();
        let config = Config::from_custom_kubeconfig(kubeconfig, &KubeConfigOptions::default())
            .await
            .map_err(|err| err.to_string());
        Context::from_config("test", config, EventLog::default(), FAST)
    }

    async fn wait_for(context: &Context, what: &str, done: impl Fn(&ContextState) -> bool) {
        let started = Instant::now();
        while !done(&context.state()) {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "not {what}: {:?}",
                context.state()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_until(what: &str, done: impl Fn() -> bool) {
        let started = Instant::now();
        while !done() {
            assert!(started.elapsed() < Duration::from_secs(10), "not {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The client's own stack sends what kube's would: the credentials and
    /// the impersonation of the kubeconfig, on requests and port-forwards.
    #[tokio::test]
    async fn the_client_sends_the_credentials_of_the_kubeconfig() {
        let heads = Arc::new(Mutex::new(Vec::new()));
        let api = FakeApi::start({
            let heads = heads.clone();
            move |request| {
                heads.lock().unwrap().push(request.head.clone());
                if request.port_forward().is_some() {
                    Reply::Kubelet(Kubelet::Echo)
                } else {
                    Reply::Json(200, VERSION.into())
                }
            }
        })
        .await;
        let yaml = format!(
            r#"
apiVersion: v1
kind: Config
clusters:
- name: test
  cluster:
    server: {}
users:
- name: test
  user:
    token: t0ken
    as: dev
contexts:
- name: test
  context:
    cluster: test
    user: test
"#,
            api.url
        );
        let kubeconfig = Kubeconfig::from_yaml(&yaml).unwrap();
        let (client, login) = build(load("test", Some(&kubeconfig)).await.unwrap()).unwrap();
        assert!(login.is_none());

        assert_eq!(probe(&client, PROBE_TIMEOUT).await, Ok("v1.33.0".into()));
        let stream = crate::portforward::open(&client, "shop", "web-0", 8080)
            .await
            .unwrap();
        let (mut app, local) = tokio::io::duplex(1024);
        let pump = tokio::spawn(stream.pump(local));
        app.write_all(b"a").await.unwrap();
        let mut answer = [0; 9];
        app.read_exact(&mut answer).await.unwrap();
        assert_eq!(&answer, b"web-0#1:a");
        drop(app);
        pump.await.unwrap().unwrap();

        let heads = heads.lock().unwrap();
        assert_eq!(heads.len(), 2);
        for head in heads.iter() {
            assert!(head.contains("authorization: bearer t0ken\r\n"), "{head}");
            assert!(head.contains("impersonate-user: dev\r\n"), "{head}");
        }
    }

    /// A proxy is left to kube's own builder, which rejects it in this build.
    #[test]
    fn a_proxy_is_left_to_kube() {
        let mut config = Config::new("https://127.0.0.1:6443".parse().unwrap());
        config.proxy_url = Some("socks5://127.0.0.1:1080".parse().unwrap());
        let Err(message) = build(config) else {
            panic!("a client with a proxy");
        };
        assert!(message.contains("kube/socks5"), "{message}");
    }

    #[tokio::test]
    async fn logs_in_only_while_the_api_server_answers() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let port = closed_port();
        let context = context(dir.path(), port, "ok").await;

        wait_for(&context, "unreachable", |state| {
            matches!(state, ContextState::Unreachable { .. })
        })
        .await;
        // Five probes.
        tokio::time::sleep(FAST.unhealthy_interval * 5).await;
        assert!(fake::runs(&runs).is_empty(), "logged in while unreachable");

        let _api = FakeApi::start_at(port, version_for_good_tokens).await;
        wait_for(&context, "healthy", ContextState::is_healthy).await;
        assert_eq!(fake::runs(&runs).len(), 1);

        // The token lasts, so the next five probes need no login.
        tokio::time::sleep(FAST.healthy_interval * 5).await;
        assert_eq!(fake::runs(&runs).len(), 1);
        context.stop();
    }

    /// A login that waits for the user does not stop the probe: an outage in
    /// the meantime shows, and while the API server answers, the context is
    /// logging in.
    #[tokio::test]
    async fn a_login_runs_next_to_the_probe() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let gate = dir.path().join("gate");
        let port = closed_port();
        let api = FakeApi::start_at(port, version_for_good_tokens).await;
        let wait = [("GATE", gate.to_str().unwrap())];
        let context = context_with(dir.path(), port, "wait", &wait).await;
        let logging_in = |state: &ContextState| *state == ContextState::LoggingIn;
        wait_for(&context, "logging in", logging_in).await;

        drop(api);
        wait_for(&context, "unreachable", |state| {
            matches!(state, ContextState::Unreachable { .. })
        })
        .await;
        let _api = FakeApi::start_at(port, version_for_good_tokens).await;
        wait_for(&context, "logging in again", logging_in).await;

        std::fs::write(&gate, "").unwrap();
        wait_for(&context, "healthy", ContextState::is_healthy).await;
        assert_eq!(fake::runs(&runs).len(), 1);
        let events: Vec<String> = context
            .events
            .entries()
            .into_iter()
            .map(|event| event.message)
            .collect();
        let login = "logging in with plugin get-token";
        assert_eq!(
            events.first().map(String::as_str),
            Some(login),
            "{events:?}"
        );
        assert_eq!(
            events.iter().filter(|event| *event == login).count(),
            2,
            "{events:?}"
        );
        assert_eq!(
            events.last().map(String::as_str),
            Some("API server reachable (v1.33.0)")
        );
        context.stop();
    }

    /// While a login renews a token that still works, the context stays
    /// healthy and is probed; once the API server rejects the token, it is
    /// logging in.
    #[tokio::test]
    async fn a_renewal_keeps_the_context_healthy_while_the_token_works() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let gate = dir.path().join("gate");
        let expired = Arc::new(AtomicBool::new(false));
        let api = FakeApi::start({
            let expired = expired.clone();
            move |request| {
                if expired.load(Ordering::Relaxed) {
                    Reply::Json(401, status(401, "Unauthorized"))
                } else {
                    version_for_good_tokens(request)
                }
            }
        })
        .await;
        // Only the second login, the renewal, waits.
        let env = [("GATE", gate.to_str().unwrap()), ("WAIT_FROM", "2")];
        let context = context_with(dir.path(), api.port(), "wait", &env).await;
        wait_for(&context, "healthy", ContextState::is_healthy).await;
        // The token is due, but still works.
        let login = context.login.as_ref().unwrap();
        login.set_token(Some(SystemTime::now()));
        wait_until("renewing", || fake::runs(&runs).len() == 2).await;

        let probes = api.requests().len();
        tokio::time::sleep(FAST.healthy_interval * 3).await;
        assert!(context.state().is_healthy(), "{:?}", context.state());
        assert!(api.requests().len() > probes, "not probed while renewing");

        expired.store(true, Ordering::Relaxed);
        wait_for(&context, "logging in", |state| {
            *state == ContextState::LoggingIn
        })
        .await;
        expired.store(false, Ordering::Relaxed);
        std::fs::write(&gate, "").unwrap();
        wait_for(&context, "healthy", ContextState::is_healthy).await;
        assert_eq!(fake::runs(&runs).len(), 2);
        context.stop();
    }

    #[test]
    fn a_context_is_logging_in_while_it_has_no_token_the_server_takes() {
        let login = Login::fake();
        let mut probed = Probed::default();
        let ok = || Ok("v1.33.0".to_owned());
        let rejected = || Err(Failure::Auth("Unauthorized".into()));
        let mut step = |previous: &ContextState, answer, logging_in| {
            after_probe(
                previous,
                answer,
                Some(&login),
                logging_in,
                &mut probed,
                at(10),
            )
        };
        // Without a token it logs in, and waits for that.
        assert_eq!(
            step(&ContextState::Connecting, ok(), false),
            (ContextState::LoggingIn, true)
        );
        // While the login runs, an outage shows; nothing else starts.
        assert_eq!(
            step(&ContextState::LoggingIn, timeout(), true),
            (unreachable(10, NetCause::Timeout), false)
        );
        assert_eq!(
            step(&unreachable(10, NetCause::Timeout), rejected(), true),
            (ContextState::LoggingIn, false)
        );

        // A token that works and is due is renewed while the context stays
        // healthy, until the API server rejects it.
        login.set_token(Some(at(0)));
        assert_eq!(step(&healthy(), ok(), false), (healthy(), true));
        assert_eq!(step(&healthy(), ok(), true), (healthy(), false));
        assert_eq!(
            step(&healthy(), rejected(), true),
            (ContextState::LoggingIn, false)
        );
        assert!(!login.has_token());
    }

    #[test]
    fn a_new_token_the_server_rejects_ends_the_logins() {
        let login = Login::fake();
        login.set_token(None);
        let mut probed = Probed {
            unverified: true,
            ..Probed::default()
        };
        let (state, log_in) = after_probe(
            &ContextState::LoggingIn,
            Err(Failure::Auth("Unauthorized".into())),
            Some(&login),
            false,
            &mut probed,
            at(10),
        );
        assert_eq!(
            state,
            ContextState::AuthFailed {
                since: at(10),
                message: "the API server rejected the login: Unauthorized".into(),
            }
        );
        assert!(!log_in);

        // An answer that takes the token ends the check.
        login.set_token(None);
        probed.unverified = true;
        let step = after_probe(
            &ContextState::LoggingIn,
            Ok("v1.33.0".into()),
            Some(&login),
            false,
            &mut probed,
            at(10),
        );
        assert_eq!(step, (healthy(), false));
        assert!(!probed.unverified);
    }

    /// Two contexts that both need a login, for example with the same
    /// identity provider, log in one after the other.
    #[tokio::test]
    async fn contexts_log_in_one_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let gate = dir.path().join("gate");
        let wait = [("GATE", gate.to_str().unwrap())];
        let api = FakeApi::start(version_for_good_tokens).await;
        let staging = context_with(dir.path(), api.port(), "wait", &wait).await;
        let production = context_with(dir.path(), api.port(), "wait", &wait).await;

        wait_until("logging in", || !fake::runs(&runs).is_empty()).await;
        tokio::time::sleep(FAST.unhealthy_interval * 5).await;
        assert_eq!(fake::runs(&runs).len(), 1, "two logins at once");

        std::fs::write(&gate, "").unwrap();
        for context in [&staging, &production] {
            wait_for(context, "healthy", ContextState::is_healthy).await;
            context.stop();
        }
        assert_eq!(fake::runs(&runs).len(), 2);
    }

    /// A login that waited for another one runs only if the API server still
    /// answers then.
    #[tokio::test]
    async fn a_login_that_waited_for_its_turn_asks_the_api_server_again() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let gate = dir.path().join("gate");
        let wait = [("GATE", gate.to_str().unwrap())];
        let api = FakeApi::start(version_for_good_tokens).await;
        let staging = context_with(dir.path(), api.port(), "wait", &wait).await;
        wait_until("logging in", || fake::runs(&runs).len() == 1).await;

        // The API server answers the first probe of production, whose login
        // then waits, and is gone when its turn comes.
        let port = closed_port();
        let production_api = FakeApi::start_at(port, version_for_good_tokens).await;
        let production = context_with(dir.path(), port, "wait", &wait).await;
        wait_until("probed", || !production_api.requests().is_empty()).await;
        drop(production_api);
        wait_until("gone", || {
            std::net::TcpStream::connect(("127.0.0.1", port)).is_err()
        })
        .await;
        std::fs::write(&gate, "").unwrap();
        wait_for(&staging, "healthy", ContextState::is_healthy).await;
        wait_for(&production, "unreachable", |state| {
            matches!(state, ContextState::Unreachable { .. })
        })
        .await;
        tokio::time::sleep(FAST.unhealthy_interval * 5).await;
        assert_eq!(fake::runs(&runs).len(), 1, "logged in while unreachable");

        let _api = FakeApi::start_at(port, version_for_good_tokens).await;
        wait_for(&production, "healthy", ContextState::is_healthy).await;
        assert_eq!(fake::runs(&runs).len(), 2);
        staging.stop();
        production.stop();
    }

    /// A plugin that hands out its cached token until that expires does not
    /// run on every probe of the token's last minute.
    #[tokio::test]
    async fn a_token_that_keeps_its_expiry_is_not_renewed_on_every_probe() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let api = FakeApi::start(version_for_good_tokens).await;
        let port = api.port();
        let expires = jiff::Timestamp::now() + jiff::SignedDuration::from_secs(30);
        let expires = format!("'{expires}'");
        let context = context_with(dir.path(), port, "ok", &[("EXPIRES", &expires)]).await;

        wait_for(&context, "healthy", ContextState::is_healthy).await;
        tokio::time::sleep(FAST.healthy_interval * 5).await;
        assert_eq!(fake::runs(&runs).len(), 1);
        context.stop();
    }

    #[tokio::test]
    async fn a_failed_login_is_not_retried() {
        for (mode, expected) in [
            ("fail", "login timed out"),
            ("bad", "the API server rejected the login"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let runs = dir.path().join("runs");
            let api = FakeApi::start(version_for_good_tokens).await;
            let port = api.port();
            let context = context(dir.path(), port, mode).await;

            wait_for(&context, "login needed", |state| {
                matches!(state, ContextState::AuthFailed { .. })
            })
            .await;
            let ContextState::AuthFailed { message, .. } = context.state() else {
                unreachable!()
            };
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("s3cr3t"), "{message}");

            tokio::time::sleep(FAST.unhealthy_interval * 5).await;
            assert_eq!(fake::runs(&runs).len(), 1, "{mode}: logged in again");
            context.stop();
        }
    }

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn healthy() -> ContextState {
        ContextState::Healthy {
            server_version: "v1.33.0".into(),
        }
    }

    fn unreachable(since: u64, cause: NetCause) -> ContextState {
        ContextState::Unreachable {
            since: at(since),
            cause,
        }
    }

    fn timeout() -> Result<String, Failure> {
        Err(Failure::Net(NetCause::Timeout))
    }

    #[test]
    fn a_healthy_context_turns_unreachable_after_two_timeouts_in_a_row() {
        let mut failures = 0;
        let state = next_state(&healthy(), timeout(), &mut failures, at(10));
        assert_eq!(state, healthy(), "one slow answer is no outage");
        let state = next_state(&state, timeout(), &mut failures, at(15));
        assert_eq!(state, unreachable(15, NetCause::Timeout));
        assert_eq!(failures, 2);

        // An answer in between starts the count again.
        let mut failures = 0;
        let state = next_state(&healthy(), timeout(), &mut failures, at(10));
        let state = next_state(&state, Ok("v1.33.0".into()), &mut failures, at(15));
        assert_eq!(failures, 0);
        let state = next_state(&state, timeout(), &mut failures, at(20));
        assert_eq!(state, healthy());
        // So does a definite failure.
        let state = next_state(
            &state,
            Err(Failure::Net(NetCause::Dns)),
            &mut failures,
            at(25),
        );
        assert_eq!(failures, 0);
        assert_eq!(state, unreachable(25, NetCause::Dns));
    }

    /// A reset or an error without a known cause may pass like a timeout:
    /// a healthy context stays healthy after one of them. The second in a
    /// row, of any of these kinds, makes it unreachable with its cause.
    #[test]
    fn resets_and_unknown_errors_count_like_timeouts() {
        let reset = || Err(Failure::Net(NetCause::Other("connection reset".into())));
        let odd = || Err(Failure::Other("connection closed".into()));
        for (first, second, cause) in [
            (reset(), reset(), NetCause::Other("connection reset".into())),
            (odd(), odd(), NetCause::Other("connection closed".into())),
            (
                timeout(),
                odd(),
                NetCause::Other("connection closed".into()),
            ),
            (reset(), timeout(), NetCause::Timeout),
        ] {
            let mut failures = 0;
            let state = next_state(&healthy(), first, &mut failures, at(10));
            assert_eq!(state, healthy());
            let state = next_state(&state, second, &mut failures, at(15));
            assert_eq!(state, unreachable(15, cause));
        }
    }

    #[test]
    fn definite_failures_and_timeouts_of_an_unhealthy_context_count_at_once() {
        for previous in [ContextState::Connecting, unreachable(1, NetCause::Dns)] {
            let mut failures = 0;
            let state = next_state(&previous, timeout(), &mut failures, at(10));
            assert!(
                matches!(
                    state,
                    ContextState::Unreachable {
                        cause: NetCause::Timeout,
                        ..
                    }
                ),
                "{previous:?}: {state:?}"
            );
            let odd = Err(Failure::Other("odd".into()));
            let state = next_state(&previous, odd, &mut failures, at(10));
            assert!(
                matches!(
                    state,
                    ContextState::Unreachable {
                        cause: NetCause::Other(_),
                        ..
                    }
                ),
                "{previous:?}: {state:?}"
            );
        }
        for cause in [
            NetCause::Dns,
            NetCause::Refused,
            NetCause::Tls("bad certificate".into()),
        ] {
            let mut failures = 0;
            let state = next_state(
                &healthy(),
                Err(Failure::Net(cause.clone())),
                &mut failures,
                at(10),
            );
            assert_eq!(state, unreachable(10, cause));
        }
    }

    /// The event of an outage carries the error text of a TLS or unknown
    /// failure, which the summary leaves out.
    #[tokio::test]
    async fn the_event_of_an_outage_says_why() {
        let config = Config::new("http://127.0.0.1:1".parse().unwrap());
        let context = Context::fake(Client::try_from(config).unwrap(), healthy());
        let expired = "invalid peer certificate: Expired";
        context.force_state(unreachable(10, NetCause::Tls(expired.into())));
        context.force_state(unreachable(10, NetCause::Dns));
        let events: Vec<String> = context
            .events
            .entries()
            .into_iter()
            .map(|event| event.message)
            .collect();
        assert_eq!(
            events,
            [
                format!("API server unreachable (TLS): {expired}"),
                "API server unreachable (DNS)".to_owned(),
            ]
        );
    }

    #[test]
    fn failures_map_to_states() {
        let cases = [
            (
                Failure::Auth("token expired".into()),
                ContextState::AuthFailed {
                    since: at(10),
                    message: "token expired".into(),
                },
            ),
            (
                Failure::Other("odd".into()),
                unreachable(10, NetCause::Other("odd".into())),
            ),
            // The server answered, so it is reachable; /version is just not
            // readable for this user.
            (
                Failure::Forbidden("no".into()),
                ContextState::Healthy {
                    server_version: "unknown".into(),
                },
            ),
            (
                Failure::NotFound("no".into()),
                ContextState::Healthy {
                    server_version: "unknown".into(),
                },
            ),
        ];
        for (failure, expected) in cases {
            assert_eq!(failed(&ContextState::Connecting, failure, at(10)), expected);
        }
    }

    #[test]
    fn an_outage_keeps_the_time_it_started() {
        let mut failures = 0;
        let previous = unreachable(5, NetCause::Timeout);
        let state = next_state(
            &previous,
            Err(Failure::Net(NetCause::Refused)),
            &mut failures,
            at(20),
        );
        assert_eq!(state, unreachable(5, NetCause::Refused));

        let state = failed(&state, Failure::Auth("expired".into()), at(30));
        assert_eq!(
            state,
            ContextState::AuthFailed {
                since: at(5),
                message: "expired".into(),
            }
        );
        let state = failed(&state, Failure::Net(NetCause::Dns), at(40));
        assert_eq!(state, unreachable(5, NetCause::Dns));

        // A recovery ends it; the next outage has its own start.
        let state = next_state(&state, Ok("v1.33.0".into()), &mut failures, at(50));
        let state = next_state(
            &state,
            Err(Failure::Net(NetCause::Dns)),
            &mut failures,
            at(60),
        );
        assert_eq!(state, unreachable(60, NetCause::Dns));
    }

    #[test]
    fn answered_means_the_server_said_something() {
        assert!(answered(&Ok("v1".into())));
        for failure in [
            Failure::Auth("expired".into()),
            Failure::Forbidden("no".into()),
            Failure::NotFound("no".into()),
        ] {
            assert!(answered(&Err(failure.clone())), "{failure:?}");
        }
        for failure in [
            Failure::Net(NetCause::Timeout),
            Failure::Other("odd".into()),
        ] {
            assert!(!answered(&Err(failure.clone())), "{failure:?}");
        }
    }

    #[test]
    fn watches_restart_on_recovery_and_after_the_first_login() {
        let down = unreachable(1, NetCause::Timeout);
        let login_needed = ContextState::AuthFailed {
            since: at(1),
            message: "expired".into(),
        };
        // Without a login, the watches started with the context.
        assert!(!restarts_watches(
            false,
            &ContextState::Connecting,
            &healthy()
        ));
        assert!(restarts_watches(false, &down, &healthy()));
        assert!(restarts_watches(false, &login_needed, &healthy()));
        // With a login, they wait for it.
        assert!(restarts_watches(
            true,
            &ContextState::Connecting,
            &healthy()
        ));
        assert!(restarts_watches(true, &down, &healthy()));
        // Nothing to start while the context is not usable.
        for next in [ContextState::Connecting, down.clone(), login_needed.clone()] {
            assert!(!restarts_watches(true, &healthy(), &next), "{next:?}");
            assert!(!restarts_watches(false, &healthy(), &next), "{next:?}");
        }
    }
}
