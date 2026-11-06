//! One kubeconfig context: its client, the health of its API server, its
//! login, and the watch caches of the namespaces its forwards use.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

use kprtfwd_core::state::{ContextState, NetCause};
use kube::client::ClientBuilder;
use kube::config::KubeConfigOptions;
use kube::{Client, Config};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::auth::Login;
use crate::cache::NamespaceCache;
use crate::errors::{Failure, classify};
use crate::events::EventLog;
use crate::kubeconfig;

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_INTERVAL_HEALTHY: Duration = Duration::from_secs(5);
const PROBE_INTERVAL_UNHEALTHY: Duration = Duration::from_secs(2);
/// A healthy context counts as unreachable only after this many probe
/// timeouts in a row: one slow answer is not an outage. Definite failures
/// (DNS, refused) count at once.
const TIMEOUTS_UNTIL_UNREACHABLE: u32 = 2;

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
}

/// The kubeconfig's settings for a context.
async fn load(context: &str) -> Result<Config, String> {
    let options = KubeConfigOptions {
        context: Some(context.to_owned()),
        ..Default::default()
    };
    Config::from_kubeconfig(&options)
        .await
        .map_err(|err| kubeconfig::describe(&err))
}

/// A client for a kubeconfig context, and its login if it logs in through an
/// exec plugin. The client never runs the plugin: it sends the token of the
/// last [`Login::run`], or no credentials.
pub async fn client_for(context: &str) -> Result<(Client, Option<Login>), String> {
    build(load(context).await?)
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
    /// Builds the client from the kubeconfig and starts the health probe.
    pub async fn start(name: &str, events: EventLog) -> Arc<Self> {
        Self::from_config(name, load(name).await, events)
    }

    fn from_config(name: &str, config: Result<Config, String>, events: EventLog) -> Arc<Self> {
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
        let answer = probe(client).await;
        let Some(login) = &self.login else {
            return next_state(&previous, answer, timeouts);
        };
        let rejected = matches!(answer, Err(Failure::Auth(_)));
        if !answered(&answer) || !(rejected || login.needed()) {
            return next_state(&previous, answer, timeouts);
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
            return failed(&previous, Failure::Auth(message));
        }
        let answer = probe(client).await;
        if let Err(Failure::Auth(message)) = &answer {
            login.forget();
            let message = format!("the API server rejected the login: {message}");
            return failed(&previous, Failure::Auth(message));
        }
        next_state(&previous, answer, timeouts)
    }

    /// Logs a change of state, keeps it in the context's events and
    /// publishes it.
    fn set_state(&self, next: ContextState) {
        let previous = self.state();
        if next == previous {
            return;
        }
        let event = match &next {
            ContextState::Healthy { server_version } => {
                info!(context = %self.name, %server_version, "API server reachable");
                // Watches of a context with a login start only now. After an
                // outage a watch may be silently dead; start them fresh
                // instead of waiting for their idle timeout.
                if self.login.is_some() || !matches!(previous, ContextState::Connecting) {
                    self.restart_watches();
                }
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
            PROBE_INTERVAL_HEALTHY
        } else {
            PROBE_INTERVAL_UNHEALTHY
        };
        drop(context);
        tokio::time::sleep(interval).await;
    }
}

/// `GET /version`: the API server's version, or why there is none.
pub(crate) async fn probe(client: &Client) -> Result<String, Failure> {
    match tokio::time::timeout(PROBE_TIMEOUT, client.apiserver_version()).await {
        Ok(Ok(info)) => Ok(info.git_version),
        Ok(Err(err)) => Err(classify(&err)),
        Err(_) => Err(Failure::Net(NetCause::Timeout)),
    }
}

/// Whether the API server answered a probe, even if only to refuse it.
pub(crate) fn answered(answer: &Result<String, Failure>) -> bool {
    !matches!(answer, Err(Failure::Net(_) | Failure::Other(_)))
}

/// The state after a probe.
fn next_state(
    previous: &ContextState,
    answer: Result<String, Failure>,
    timeouts: &mut u32,
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
                failed(previous, Failure::Net(NetCause::Timeout))
            }
        }
        Err(failure) => {
            *timeouts = 0;
            failed(previous, failure)
        }
    }
}

/// The state after a failed probe. Keeps the time the outage started.
fn failed(previous: &ContextState, failure: Failure) -> ContextState {
    let since = match previous {
        ContextState::Unreachable { since, .. } | ContextState::AuthFailed { since, .. } => *since,
        _ => SystemTime::now(),
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::auth::fake;

    const VERSION: &str = r#"{"major":"1","minor":"33","gitVersion":"v1.33.0","gitCommit":"","gitTreeState":"","buildDate":"","goVersion":"","compiler":"","platform":""}"#;
    const UNAUTHORIZED: &str = r#"{"kind":"Status","apiVersion":"v1","metadata":{},"status":"Failure","message":"Unauthorized","reason":"Unauthorized","code":401}"#;

    /// An API server that answers every request like `/version`: 200 for the
    /// token "good", 401 otherwise.
    async fn api_server(listener: TcpListener) {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&request).to_lowercase();
                let (status, body) = if head.contains("authorization: bearer good\r\n") {
                    ("200 OK", VERSION)
                } else {
                    ("401 Unauthorized", UNAUTHORIZED)
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A context whose exec plugin is [`fake::plugin`] in `mode`.
    async fn context(dir: &Path, port: u16, mode: &str) -> Arc<Context> {
        let plugin = fake::plugin(dir);
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
        value: {mode}
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
        Context::from_config("test", config, EventLog::default())
    }

    async fn wait_for(context: &Context, what: &str, done: impl Fn(&ContextState) -> bool) {
        let started = Instant::now();
        while !done(&context.state()) {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "not {what}: {:?}",
                context.state()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn logs_in_only_while_the_api_server_answers() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let port = free_port();
        let context = context(dir.path(), port, "ok").await;

        wait_for(&context, "unreachable", |state| {
            matches!(state, ContextState::Unreachable { .. })
        })
        .await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(fake::runs(&runs).is_empty(), "logged in while unreachable");

        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        tokio::spawn(api_server(listener));
        wait_for(&context, "healthy", ContextState::is_healthy).await;
        assert_eq!(fake::runs(&runs).len(), 1);

        // The token lasts, so the next probes need no login.
        tokio::time::sleep(Duration::from_secs(6)).await;
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
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(api_server(listener));
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

            tokio::time::sleep(Duration::from_secs(5)).await;
            assert_eq!(fake::runs(&runs).len(), 1, "{mode}: logged in again");
            context.stop();
        }
    }
}
