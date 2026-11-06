//! One kubeconfig context: its client, the health of its API server, its
//! login, and the watch caches of the namespaces its forwards use.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

use kprtfwd_core::state::{ContextState, NetCause};
use kube::client::ClientBuilder;
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Client, Config};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::auth::Login;
use crate::cache::NamespaceCache;
use crate::errors::{Failure, classify};
use crate::events::EventLog;
use crate::kubeconfig;

pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_INTERVAL_HEALTHY: Duration = Duration::from_secs(5);
const PROBE_INTERVAL_UNHEALTHY: Duration = Duration::from_secs(2);
/// A healthy context counts as unreachable only after this many probe
/// timeouts in a row: one slow answer is not an outage. Definite failures
/// (DNS, refused) count at once.
const TIMEOUTS_UNTIL_UNREACHABLE: u32 = 2;

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
    let builder = ClientBuilder::try_from(config).map_err(|err| err.to_string())?;
    let client = match &login {
        Some(login) => builder.with_layer(&login.layer()).build(),
        None => builder.build(),
    };
    Ok((client, login))
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

    /// Probes the API server once, logs in if the context needs to, and
    /// returns the new state.
    ///
    /// A login may open the browser, so it only happens here, one at a time,
    /// and only while the API server answers.
    async fn check(&self, client: &Client, timeouts: &mut u32) -> ContextState {
        let previous = self.state();
        let answer = probe(client, self.timing.probe_timeout).await;
        let Some(login) = &self.login else {
            return next_state(&previous, answer, timeouts, SystemTime::now());
        };
        let rejected = matches!(answer, Err(Failure::Auth(_)));
        if !answered(&answer) || !(rejected || login.needed()) {
            return next_state(&previous, answer, timeouts, SystemTime::now());
        }

        if rejected {
            login.forget();
        }
        if previous.is_healthy() {
            debug!(context = %self.name, "refreshing the login");
        } else {
            self.set_state(ContextState::Connecting);
        }
        if let Err(message) = login.run().await {
            return failed(&previous, Failure::Auth(message), SystemTime::now());
        }
        let answer = probe(client, self.timing.probe_timeout).await;
        if let Err(Failure::Auth(message)) = &answer {
            login.forget();
            let message = format!("the API server rejected the login: {message}");
            return failed(&previous, Failure::Auth(message), SystemTime::now());
        }
        next_state(&previous, answer, timeouts, SystemTime::now())
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
            ContextState::Connecting => {
                let plugin = self.login.as_ref().map(Login::name).unwrap_or_default();
                info!(context = %self.name, %plugin, "logging in");
                format!("logging in with {plugin}")
            }
            ContextState::AuthFailed { message, .. } => {
                warn!(context = %self.name, %message, "login failed");
                format!("login needed: {message}")
            }
            other => {
                warn!(context = %self.name, state = %other.summary(), "API server not usable");
                format!("API server {}", other.summary())
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

/// Probes the API server until the context is dropped or stopped.
async fn probe_health(context: Weak<Context>) {
    let mut timeouts = 0;
    loop {
        let Some(context) = context.upgrade() else {
            return;
        };
        let Some(client) = context.client.clone() else {
            return;
        };
        let next = context.check(&client, &mut timeouts).await;
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
        tokio::time::sleep(interval).await;
    }
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

/// The state after a probe at `now`.
fn next_state(
    previous: &ContextState,
    answer: Result<String, Failure>,
    timeouts: &mut u32,
    now: SystemTime,
) -> ContextState {
    match answer {
        Ok(server_version) => {
            *timeouts = 0;
            ContextState::Healthy { server_version }
        }
        Err(Failure::Net(NetCause::Timeout)) => {
            *timeouts += 1;
            if previous.is_healthy() && *timeouts < TIMEOUTS_UNTIL_UNREACHABLE {
                previous.clone()
            } else {
                failed(previous, Failure::Net(NetCause::Timeout), now)
            }
        }
        Err(failure) => {
            *timeouts = 0;
            failed(previous, failure, now)
        }
    }
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
    use std::time::Instant;

    use kube::config::Kubeconfig;

    use super::*;
    use crate::auth::fake;
    use crate::fake::{FakeApi, Reply, VERSION, closed_port, status};

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
        let plugin = fake::plugin(dir);
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

    /// A plugin that hands out its cached token until that expires does not
    /// run on every probe of the token's last minute.
    #[tokio::test]
    async fn a_token_that_keeps_its_expiry_is_not_renewed_on_every_probe() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let api = FakeApi::start(version_for_good_tokens).await;
        let port = api.url.rsplit(':').next().unwrap().parse().unwrap();
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
            let port = api.url.rsplit(':').next().unwrap().parse().unwrap();
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
        let mut timeouts = 0;
        let state = next_state(&healthy(), timeout(), &mut timeouts, at(10));
        assert_eq!(state, healthy(), "one slow answer is no outage");
        let state = next_state(&state, timeout(), &mut timeouts, at(15));
        assert_eq!(state, unreachable(15, NetCause::Timeout));
        assert_eq!(timeouts, 2);

        // An answer in between starts the count again.
        let mut timeouts = 0;
        let state = next_state(&healthy(), timeout(), &mut timeouts, at(10));
        let state = next_state(&state, Ok("v1.33.0".into()), &mut timeouts, at(15));
        assert_eq!(timeouts, 0);
        let state = next_state(&state, timeout(), &mut timeouts, at(20));
        assert_eq!(state, healthy());
        // So does a definite failure.
        let state = next_state(
            &state,
            Err(Failure::Net(NetCause::Dns)),
            &mut timeouts,
            at(25),
        );
        assert_eq!(timeouts, 0);
        assert_eq!(state, unreachable(25, NetCause::Dns));
    }

    #[test]
    fn definite_failures_and_timeouts_of_an_unhealthy_context_count_at_once() {
        for previous in [ContextState::Connecting, unreachable(1, NetCause::Dns)] {
            let mut timeouts = 0;
            let state = next_state(&previous, timeout(), &mut timeouts, at(10));
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
        }
        for cause in [
            NetCause::Dns,
            NetCause::Refused,
            NetCause::Tls("bad certificate".into()),
        ] {
            let mut timeouts = 0;
            let state = next_state(
                &healthy(),
                Err(Failure::Net(cause.clone())),
                &mut timeouts,
                at(10),
            );
            assert_eq!(state, unreachable(10, cause));
        }
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
        let mut timeouts = 0;
        let previous = unreachable(5, NetCause::Timeout);
        let state = next_state(
            &previous,
            Err(Failure::Net(NetCause::Refused)),
            &mut timeouts,
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
        let state = next_state(&state, Ok("v1.33.0".into()), &mut timeouts, at(50));
        let state = next_state(
            &state,
            Err(Failure::Net(NetCause::Dns)),
            &mut timeouts,
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
