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
use tracing::{info, warn};

use crate::errors::{Failure, classify};

/// The label that links an EndpointSlice to its Service.
const SERVICE_NAME_LABEL: &str = "kubernetes.io/service-name";

/// The objects of one kind in one namespace.
struct Objects<K> {
    /// The first full listing has arrived.
    ready: bool,
    items: HashMap<String, Arc<K>>,
    /// The last watch error, cleared by the next event that arrives.
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
    /// An empty cache; [`Self::restart`] starts its watches.
    pub fn new(namespace: &str) -> Arc<Self> {
        Arc::new(Self {
            namespace: namespace.to_owned(),
            services: Shared::default(),
            slices: Shared::default(),
            tasks: Mutex::new(Vec::new()),
        })
    }

    /// Starts the watches, or starts them again. The cached objects stay
    /// until the new listing replaces them.
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

#[cfg(test)]
impl NamespaceCache {
    /// A cache whose listing is these objects, without watches.
    pub(crate) fn listed(
        namespace: &str,
        services: Vec<Service>,
        slices: Vec<EndpointSlice>,
    ) -> Arc<Self> {
        let cache = Self::new(namespace);
        cache.set_services(services);
        cache.set_slices(slices);
        cache
    }

    pub(crate) fn set_services(&self, services: Vec<Service>) {
        fill(&self.services, services);
    }

    pub(crate) fn set_slices(&self, slices: Vec<EndpointSlice>) {
        fill(&self.slices, slices);
    }
}

/// Replaces the objects, as a full listing does.
#[cfg(test)]
fn fill<K: Resource>(objects: &Shared<K>, items: Vec<K>) {
    let mut objects = objects.write().unwrap();
    objects.items = items
        .into_iter()
        .map(|object| (object.name_any(), Arc::new(object)))
        .collect();
    objects.ready = true;
    objects.error = None;
}

fn count(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// Fails with the reason the objects cannot be used.
///
/// A 403 counts after the first listing too: it does not pass by itself, and
/// the context's probe does not see it, so the objects would go stale while
/// the forward looks ready. Other failures of a listed watch keep what it
/// has; the probe shows an outage, and the watcher retries the rest.
fn check<K>(objects: &Objects<K>) -> Result<(), Target> {
    match &objects.error {
        Some(Failure::Forbidden(message)) => Err(Target::Forbidden(message.clone())),
        _ if objects.ready => Ok(()),
        _ => Err(Target::Syncing),
    }
}

/// Clears the error of `objects` once an event arrived.
fn recovered<K>(objects: &mut Objects<K>, kind: &str) {
    if objects.error.take().is_some() {
        info!(%kind, "watch recovered");
    }
}

/// Feeds watch events of one kind into `objects` until aborted. The watcher
/// retries with backoff on its own: a failed listing with a new listing, a
/// failed watch with a new watch from where it was, which shows only with
/// the next event.
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
                recovered(&mut objects, &kind);
            }
            Ok(watcher::Event::Apply(object)) => {
                let mut objects = objects.write().unwrap();
                objects.items.insert(object.name_any(), Arc::new(object));
                recovered(&mut objects, &kind);
            }
            Ok(watcher::Event::Delete(object)) => {
                let mut objects = objects.write().unwrap();
                objects.items.remove(&object.name_any());
                recovered(&mut objects, &kind);
            }
            Err(err) => {
                let failure = classify(&err);
                let mut objects = objects.write().unwrap();
                // Once per failure, until an event ends it.
                if objects.error.as_ref() != Some(&failure) {
                    warn!(%kind, error = %err, "watch failed");
                }
                objects.error = Some(failure);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use kprtfwd_core::state::NetCause;
    use serde_json::json;
    use tokio::sync::Barrier;

    use super::*;
    use crate::fake::{FakeApi, Reply, list, service, slice, status};

    fn endpoint(pod: &str, port: u16) -> Endpoint {
        Endpoint {
            pod: pod.into(),
            port,
        }
    }

    fn ready_pods(ready: u16, total: u16) -> Target {
        Target::Endpoints { ready, total }
    }

    #[test]
    fn finds_the_ready_pods_of_a_service_port_across_slices() {
        let cache = NamespaceCache::listed(
            "shop",
            vec![service("api", &[("http", 80), ("metrics", 9090)])],
            vec![
                slice(
                    "api-1",
                    "api",
                    &[("http", 8080), ("metrics", 9100)],
                    &[("api-1", Some(true)), ("api-0", Some(true))],
                ),
                slice(
                    "api-2",
                    "api",
                    &[("http", 8080), ("metrics", 9100)],
                    &[("api-2", Some(true))],
                ),
            ],
        );
        assert_eq!(
            cache.endpoints("api", 80),
            Ok(vec![
                endpoint("api-0", 8080),
                endpoint("api-1", 8080),
                endpoint("api-2", 8080)
            ])
        );
        // Another service port, another container port.
        assert_eq!(
            cache.endpoints("api", 9090).unwrap()[0],
            endpoint("api-0", 9100)
        );
        assert_eq!(cache.target("api", 80), ready_pods(3, 3));
    }

    #[test]
    fn dual_stack_slices_list_each_pod_once() {
        let mut v6 = slice("api-v6", "api", &[("http", 8080)], &[("api-0", Some(true))]);
        v6.address_type = "IPv6".into();
        let cache = NamespaceCache::listed(
            "shop",
            vec![service("api", &[("http", 80)])],
            vec![
                slice("api-v4", "api", &[("http", 8080)], &[("api-0", Some(true))]),
                v6,
            ],
        );
        assert_eq!(
            cache.endpoints("api", 80),
            Ok(vec![endpoint("api-0", 8080)])
        );
        assert_eq!(cache.target("api", 80), ready_pods(1, 1));
    }

    #[test]
    fn unknown_readiness_counts_as_ready_and_unready_pods_count_only_in_total() {
        let cache = NamespaceCache::listed(
            "shop",
            vec![service("api", &[("http", 80)])],
            vec![slice(
                "api",
                "api",
                &[("http", 8080)],
                &[("api-0", None), ("api-1", Some(false))],
            )],
        );
        assert_eq!(
            cache.endpoints("api", 80),
            Ok(vec![endpoint("api-0", 8080)])
        );
        assert_eq!(cache.target("api", 80), ready_pods(1, 2));

        // Only terminating pods left.
        cache.set_slices(vec![slice(
            "api",
            "api",
            &[("http", 8080)],
            &[("api-0", Some(false)), ("api-1", Some(false))],
        )]);
        assert_eq!(cache.endpoints("api", 80), Err(ready_pods(0, 2)));
        assert_eq!(cache.target("api", 80), ready_pods(0, 2));
        // No pods at all.
        cache.set_slices(vec![]);
        assert_eq!(cache.endpoints("api", 80), Err(ready_pods(0, 0)));
    }

    /// A named target port can mean another container port per pod, as in a
    /// rollout that changes it; each slice says which.
    #[test]
    fn named_target_ports_resolve_per_slice() {
        let cache = NamespaceCache::listed(
            "shop",
            vec![service("api", &[("http", 80)])],
            vec![
                slice(
                    "api-old",
                    "api",
                    &[("http", 8080)],
                    &[("api-old", Some(true))],
                ),
                slice(
                    "api-new",
                    "api",
                    &[("http", 8081)],
                    &[("api-new", Some(true))],
                ),
            ],
        );
        assert_eq!(
            cache.endpoints("api", 80),
            Ok(vec![endpoint("api-new", 8081), endpoint("api-old", 8080)])
        );
    }

    #[test]
    fn an_unnamed_port_matches_the_unnamed_slice_port() {
        let cache = NamespaceCache::listed(
            "shop",
            vec![service("api", &[("", 80)])],
            vec![slice("api", "api", &[("", 8080)], &[("api-0", Some(true))])],
        );
        assert_eq!(
            cache.endpoints("api", 80),
            Ok(vec![endpoint("api-0", 8080)])
        );
    }

    #[test]
    fn ignores_other_services_ports_and_targets() {
        let mut node = slice(
            "api-node",
            "api",
            &[("http", 8080)],
            &[("node-0", Some(true))],
        );
        node.endpoints.as_mut().unwrap()[0]
            .target_ref
            .as_mut()
            .unwrap()
            .kind = Some("Node".into());
        let no_target: EndpointSlice = serde_json::from_value(json!({
            "metadata": {
                "name": "api-ip",
                "labels": {"kubernetes.io/service-name": "api"},
            },
            "addressType": "IPv4",
            "ports": [{"name": "http", "port": 8080}],
            "endpoints": [{"addresses": ["10.0.0.9"], "conditions": {"ready": true}}],
        }))
        .unwrap();
        let cache = NamespaceCache::listed(
            "shop",
            vec![
                service("api", &[("http", 80)]),
                service("web", &[("http", 80)]),
            ],
            vec![
                slice("api", "api", &[("http", 8080)], &[("api-0", Some(true))]),
                slice("web", "web", &[("http", 8080)], &[("web-0", Some(true))]),
                slice(
                    "api-grpc",
                    "api",
                    &[("grpc", 9000)],
                    &[("grpc-0", Some(true))],
                ),
                node,
                no_target,
            ],
        );
        assert_eq!(
            cache.endpoints("api", 80),
            Ok(vec![endpoint("api-0", 8080)])
        );
        assert_eq!(cache.target("api", 80), ready_pods(1, 1));
    }

    #[test]
    fn says_what_is_missing() {
        let cache = NamespaceCache::listed("shop", vec![service("api", &[("http", 80)])], vec![]);
        assert_eq!(cache.target("web", 80), Target::ServiceNotFound);
        assert_eq!(cache.endpoints("web", 80), Err(Target::ServiceNotFound));
        assert_eq!(cache.target("api", 8080), Target::PortNotOnService);
    }

    #[test]
    fn waits_for_both_listings_unless_they_are_forbidden() {
        let cache = NamespaceCache::new("shop");
        assert_eq!(cache.target("api", 80), Target::Syncing);

        cache.set_services(vec![service("api", &[("http", 80)])]);
        assert_eq!(
            cache.target("api", 80),
            Target::Syncing,
            "slices not listed yet"
        );

        cache.slices.write().unwrap().error = Some(Failure::Net(NetCause::Timeout));
        assert_eq!(cache.target("api", 80), Target::Syncing);
        cache.slices.write().unwrap().error = Some(Failure::Forbidden("no slices".into()));
        assert_eq!(
            cache.target("api", 80),
            Target::Forbidden("no slices".into())
        );
        assert_eq!(
            cache.endpoints("api", 80),
            Err(Target::Forbidden("no slices".into()))
        );

        // After the first listing, a watch that fails for a while keeps
        // what was listed; the context's probe shows an outage.
        cache.set_slices(vec![slice(
            "api",
            "api",
            &[("http", 8080)],
            &[("api-0", Some(true))],
        )]);
        for failure in [
            Failure::Net(NetCause::Timeout),
            Failure::Other("500 Internal Server Error".into()),
        ] {
            cache.slices.write().unwrap().error = Some(failure);
            assert_eq!(cache.target("api", 80), ready_pods(1, 1));
        }
        // A 403 does not pass, so the listing would only go stale.
        cache.slices.write().unwrap().error = Some(Failure::Forbidden("no slices".into()));
        assert_eq!(
            cache.target("api", 80),
            Target::Forbidden("no slices".into())
        );
        cache.services.write().unwrap().error = Some(Failure::Forbidden("no services".into()));
        assert_eq!(
            cache.endpoints("api", 80),
            Err(Target::Forbidden("no services".into()))
        );
    }

    /// Waits until `cache` shows `want` for the service port `api:80`.
    async fn wait_for_target(cache: &NamespaceCache, want: &Target) {
        let started = Instant::now();
        while cache.target("api", 80) != *want {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "not {want:?}: {:?}",
                cache.target("api", 80)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A request the fake API server never answers, like a watch without
    /// events.
    fn quiet() -> Reply {
        Reply::Gated(
            Arc::new(Barrier::new(2)),
            Box::new(Reply::Json(200, String::new())),
        )
    }

    /// The watch of EndpointSlices is forbidden after the first listing,
    /// e.g. by an RBAC change, and allowed again later. The forward shows
    /// the 403 until the next event, which a watch that resumes where it was
    /// sends only once something changed; it is no longer ready on what may
    /// be gone.
    #[tokio::test]
    async fn a_watch_forbidden_after_the_listing_shows_until_the_next_event() {
        let forbidden = Arc::new(AtomicBool::new(true));
        let events = Arc::new(AtomicUsize::new(0));
        let api = FakeApi::start({
            let forbidden = forbidden.clone();
            let events = events.clone();
            move |request| {
                let watching = request.query.contains("watch=true");
                let slices = request.path.ends_with("/endpointslices");
                match (slices, watching) {
                    (false, false) => {
                        let api = serde_json::to_value(service("api", &[("http", 80)])).unwrap();
                        Reply::Json(200, list("ServiceList", &[api]))
                    }
                    (true, false) => {
                        let api = slice("api", "api", &[("http", 8080)], &[("api-0", Some(true))]);
                        let api = serde_json::to_value(api).unwrap();
                        Reply::Json(200, list("EndpointSliceList", &[api]))
                    }
                    (true, true) if forbidden.load(Ordering::SeqCst) => Reply::Json(
                        403,
                        status(403, "endpointslices is forbidden: cannot watch"),
                    ),
                    // One change since the listing, then nothing.
                    (true, true) if events.fetch_add(1, Ordering::SeqCst) == 0 => {
                        let mut api = serde_json::to_value(slice(
                            "api",
                            "api",
                            &[("http", 8080)],
                            &[("api-1", Some(true)), ("api-2", Some(true))],
                        ))
                        .unwrap();
                        api["metadata"]["resourceVersion"] = json!("2");
                        let event = json!({"type": "MODIFIED", "object": api});
                        Reply::Json(200, format!("{event}\n"))
                    }
                    _ => quiet(),
                }
            }
        })
        .await;
        let cache = NamespaceCache::new("shop");
        cache.restart(&api.client());

        let denied = Target::Forbidden("endpointslices is forbidden: cannot watch".into());
        wait_for_target(&cache, &denied).await;
        assert!(cache.slices.read().unwrap().ready, "listed before");
        assert_eq!(cache.endpoints("api", 80), Err(denied));

        forbidden.store(false, Ordering::SeqCst);
        wait_for_target(&cache, &ready_pods(2, 2)).await;
        assert_eq!(cache.slices.read().unwrap().error, None);
        cache.stop();
    }
}
