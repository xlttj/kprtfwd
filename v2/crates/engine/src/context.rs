//! One kubeconfig context: its client, the health of its API server, and the
//! watch caches of the namespaces its forwards use.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

use kprtfwd_core::state::{ContextState, NetCause};
use kube::config::KubeConfigOptions;
use kube::{Client, Config};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::cache::NamespaceCache;
use crate::errors::{Failure, classify};

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
    state: watch::Sender<ContextState>,
    namespaces: Mutex<HashMap<String, Arc<NamespaceCache>>>,
    health: Mutex<Option<JoinHandle<()>>>,
}

impl Context {
    /// Builds the client from the kubeconfig and starts the health probe.
    pub async fn start(name: &str) -> Arc<Self> {
        let options = KubeConfigOptions {
            context: Some(name.to_owned()),
            ..Default::default()
        };
        let client = match Config::from_kubeconfig(&options).await {
            Ok(config) => Client::try_from(config).map_err(|err| err.to_string()),
            Err(err) => Err(err.to_string()),
        };
        let (state, client) = match client {
            Ok(client) => (ContextState::Connecting, Some(client)),
            Err(message) => {
                warn!(context = name, %message, "cannot load context");
                (ContextState::ConfigError { message }, None)
            }
        };
        let context = Arc::new(Self {
            name: name.to_owned(),
            client,
            state: watch::channel(state).0,
            namespaces: Mutex::new(HashMap::new()),
            health: Mutex::new(None),
        });
        if context.client.is_some() {
            let task = tokio::spawn(probe_health(Arc::downgrade(&context)));
            *context.health.lock().unwrap() = Some(task);
        }
        context
    }

    pub fn name(&self) -> &str {
        &self.name
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
            namespaces
                .entry(name.clone())
                .or_insert_with(|| NamespaceCache::start(client, name));
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
        let previous = context.state();
        let next = match tokio::time::timeout(PROBE_TIMEOUT, client.apiserver_version()).await {
            Ok(Ok(info)) => {
                timeouts = 0;
                ContextState::Healthy {
                    server_version: info.git_version,
                }
            }
            Ok(Err(err)) => {
                timeouts = 0;
                failed(&previous, classify(&err))
            }
            Err(_) => {
                timeouts += 1;
                if previous.is_healthy() && timeouts < TIMEOUTS_UNTIL_UNREACHABLE {
                    previous.clone()
                } else {
                    failed(&previous, Failure::Net(NetCause::Timeout))
                }
            }
        };

        if next != previous {
            match &next {
                ContextState::Healthy { server_version } => {
                    info!(context = context.name(), %server_version, "API server reachable");
                    // After an outage a watch may be silently dead; start
                    // them fresh instead of waiting for their idle timeout.
                    if !matches!(previous, ContextState::Connecting) {
                        context.restart_watches();
                    }
                }
                other => {
                    warn!(context = context.name(), state = %other.summary(), "API server not usable")
                }
            }
            context.state.send_replace(next.clone());
        }

        let interval = if next.is_healthy() {
            PROBE_INTERVAL_HEALTHY
        } else {
            PROBE_INTERVAL_UNHEALTHY
        };
        drop(context);
        tokio::time::sleep(interval).await;
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
