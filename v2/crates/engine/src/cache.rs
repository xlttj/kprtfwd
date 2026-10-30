//! Services and EndpointSlices of one namespace, kept current by watches.
//!
//! The cache object lives as long as forwards need the namespace. Its watches
//! can be restarted (after an outage) and refill the same cache, so forwards
//! never hold on to a stale copy.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use futures::StreamExt;
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kprtfwd_core::state::Target;
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client, Resource, ResourceExt};
use serde::de::DeserializeOwned;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::errors::{Failure, classify};

/// The label that links an EndpointSlice to its Service.
const SERVICE_NAME_LABEL: &str = "kubernetes.io/service-name";

/// The objects of one kind in one namespace.
struct Objects<K> {
    /// The first full listing has arrived.
    ready: bool,
    items: HashMap<String, Arc<K>>,
    /// The last watch error, cleared by the next successful listing.
    error: Option<Failure>,
}

impl<K> Default for Objects<K> {
    fn default() -> Self {
        Self {
            ready: false,
            items: HashMap::new(),
            error: None,
        }
    }
}

type Shared<K> = Arc<RwLock<Objects<K>>>;

pub struct NamespaceCache {
    namespace: String,
    services: Shared<Service>,
    slices: Shared<EndpointSlice>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

/// A ready pod and the container port to connect to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Endpoint {
    pub pod: String,
    pub port: u16,
}

impl NamespaceCache {
    pub fn start(client: &Client, namespace: &str) -> Arc<Self> {
        let cache = Arc::new(Self {
            namespace: namespace.to_owned(),
            services: Shared::default(),
            slices: Shared::default(),
            tasks: Mutex::new(Vec::new()),
        });
        cache.restart(client);
        cache
    }

    /// Starts the watches again. The cached objects stay until the new
    /// listing replaces them.
    pub fn restart(&self, client: &Client) {
        let mut tasks = self.tasks.lock().unwrap();
        for task in tasks.drain(..) {
            task.abort();
        }
        tasks.push(tokio::spawn(watch(
            Api::<Service>::namespaced(client.clone(), &self.namespace),
            self.services.clone(),
        )));
        tasks.push(tokio::spawn(watch(
            Api::<EndpointSlice>::namespaced(client.clone(), &self.namespace),
            self.slices.clone(),
        )));
    }

    pub fn stop(&self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }

    /// Ready endpoints for the service port `remote_port`, sorted. If there
    /// are none, the error says why, as a [`Target`].
    pub fn endpoints(&self, service: &str, remote_port: u16) -> Result<Vec<Endpoint>, Target> {
        let (ready, total) = self.lookup(service, remote_port)?;
        if ready.is_empty() {
            return Err(Target::Endpoints { ready: 0, total });
        }
        Ok(ready)
    }

    /// The target as shown in the forward's state.
    pub fn target(&self, service: &str, remote_port: u16) -> Target {
        match self.lookup(service, remote_port) {
            Ok((ready, total)) => Target::Endpoints {
                ready: count(ready.len()),
                total,
            },
            Err(target) => target,
        }
    }

    /// Ready endpoints and the number of pods behind the service port.
    fn lookup(&self, service: &str, remote_port: u16) -> Result<(Vec<Endpoint>, u16), Target> {
        let port_name = {
            let services = self.services.read().unwrap();
            check(&services)?;
            let service = services.items.get(service).ok_or(Target::ServiceNotFound)?;
            let port = service
                .spec
                .as_ref()
                .and_then(|spec| spec.ports.as_ref())
                .and_then(|ports| ports.iter().find(|p| p.port == i32::from(remote_port)))
                .ok_or(Target::PortNotOnService)?;
            port.name.clone().unwrap_or_default()
        };

        let slices = self.slices.read().unwrap();
        check(&slices)?;
        let mut ready = Vec::new();
        let mut pods = Vec::new();
        for slice in slices.items.values() {
            if slice.labels().get(SERVICE_NAME_LABEL).map(String::as_str) != Some(service) {
                continue;
            }
            // The slice lists ports by the service port's name, already
            // resolved to the container port; that covers named target ports.
            let Some(port) = slice
                .ports
                .iter()
                .flatten()
                .find(|p| p.name.as_deref().unwrap_or_default() == port_name)
                .and_then(|p| p.port)
                .and_then(|p| u16::try_from(p).ok())
            else {
                continue;
            };
            for endpoint in slice.endpoints.iter().flatten() {
                let Some(pod) = endpoint
                    .target_ref
                    .as_ref()
                    .filter(|r| r.kind.as_deref() == Some("Pod"))
                    .and_then(|r| r.name.clone())
                else {
                    continue;
                };
                // An unset "ready" means unknown, which counts as ready.
                let is_ready = endpoint
                    .conditions
                    .as_ref()
                    .and_then(|c| c.ready)
                    .unwrap_or(true);
                if is_ready {
                    ready.push(Endpoint {
                        pod: pod.clone(),
                        port,
                    });
                }
                pods.push(pod);
            }
        }
        // Dual-stack clusters list each pod in an IPv4 and an IPv6 slice.
        ready.sort();
        ready.dedup();
        pods.sort();
        pods.dedup();
        Ok((ready, count(pods.len())))
    }
}

fn count(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// Fails with the reason the objects cannot be used yet.
fn check<K>(objects: &Objects<K>) -> Result<(), Target> {
    if objects.ready {
        return Ok(());
    }
    match &objects.error {
        Some(Failure::Forbidden(message)) => Err(Target::Forbidden(message.clone())),
        _ => Err(Target::Syncing),
    }
}

/// Feeds watch events of one kind into `objects` until aborted. The watcher
/// retries with backoff on its own.
async fn watch<K>(api: Api<K>, objects: Shared<K>)
where
    K: Resource<DynamicType = ()>
        + Clone
        + DeserializeOwned
        + std::fmt::Debug
        + Send
        + Sync
        + 'static,
{
    let kind = K::kind(&()).into_owned();
    let mut events = watcher(api, watcher::Config::default())
        .default_backoff()
        .boxed();
    let mut listing: Option<HashMap<String, Arc<K>>> = None;
    while let Some(event) = events.next().await {
        match event {
            Ok(watcher::Event::Init) => listing = Some(HashMap::new()),
            Ok(watcher::Event::InitApply(object)) => {
                if let Some(listing) = listing.as_mut() {
                    listing.insert(object.name_any(), Arc::new(object));
                }
            }
            Ok(watcher::Event::InitDone) => {
                let mut objects = objects.write().unwrap();
                objects.items = listing.take().unwrap_or_default();
                objects.ready = true;
                if objects.error.take().is_some() {
                    debug!(%kind, "watch recovered");
                }
            }
            Ok(watcher::Event::Apply(object)) => {
                let name = object.name_any();
                objects
                    .write()
                    .unwrap()
                    .items
                    .insert(name, Arc::new(object));
            }
            Ok(watcher::Event::Delete(object)) => {
                objects.write().unwrap().items.remove(&object.name_any());
            }
            Err(err) => {
                let failure = classify(&err);
                let mut objects = objects.write().unwrap();
                if objects.error.as_ref() != Some(&failure) {
                    warn!(%kind, error = %err, "watch failed");
                }
                objects.error = Some(failure);
            }
        }
    }
}
