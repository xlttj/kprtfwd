//! Finding services to forward: the kubeconfig's contexts and a context's
//! services.

use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::core::v1::{Namespace, Service};
use kprtfwd_core::filter::wildcard;
use kprtfwd_core::state::NetCause;
use kube::api::ListParams;
use kube::{Api, Client, ResourceExt};

use crate::context::{LoginError, PROBE_TIMEOUT, answered, client_for, log_in, probe};
use crate::errors::{Failure, classify};
use crate::kubeconfig;

/// How long one page of a list may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Objects per page: a large cluster is listed in several answers, each with
/// its own time limit, instead of one that may run into it.
const PAGE: u32 = 500;
/// Namespaces the fallback lists at the same time.
const PARALLEL: usize = 8;

/// What listing a context found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    /// Sorted by namespace and name.
    pub services: Vec<ServiceInfo>,
    /// Namespaces that match the filter but whose services the user may not
    /// list, sorted. What they hold is unknown: prune must not take their
    /// forwards for gone.
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    pub namespace: String,
    pub name: String,
    /// TCP ports only; port-forwarding cannot carry UDP.
    pub ports: Vec<PortInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortInfo {
    pub port: u16,
    pub name: Option<String>,
}

/// The context names in the kubeconfig, sorted, and the current context.
pub fn contexts() -> Result<(Vec<String>, Option<String>), String> {
    let config = kubeconfig::read()?;
    let mut names: Vec<String> = config.contexts.iter().map(|c| c.name.clone()).collect();
    names.sort();
    Ok((names, config.current_context))
}

/// The services in `context` whose namespace matches `namespace_filter` (`*`
/// wildcards, empty for all), and the namespaces skipped because the user may
/// not list their services.
pub async fn services(context: &str, namespace_filter: &str) -> Result<Listing, String> {
    let (client, login) = client_for(context).await?;
    if let Some(login) = login {
        // Log in only if the API server answers: a login may open the browser.
        let answer = probe(&client, PROBE_TIMEOUT).await;
        if let Err(failure) = &answer
            && !answered(&answer)
        {
            return Err(describe(failure, context));
        }
        log_in(&login, &client, PROBE_TIMEOUT)
            .await
            .map_err(|err| match err {
                LoginError::Unanswered(failure) => describe(&failure, context),
                LoginError::Failed(message) => format!("login failed: {message}"),
            })?;
    }
    list_services(&client, context, namespace_filter).await
}

/// [`services`] with a client that is ready to use.
async fn list_services(
    client: &Client,
    context: &str,
    namespace_filter: &str,
) -> Result<Listing, String> {
    let filter = namespace_filter.trim();
    let all = filter.is_empty() || filter == "*";

    let mut skipped = Vec::new();
    let services = if exact(filter) {
        // One namespace is listed alone, not picked out of the whole cluster.
        list(Api::<Service>::namespaced(client.clone(), filter))
            .await
            .map_err(|failure| describe(&failure, context))?
    } else {
        match list(Api::<Service>::all(client.clone())).await {
            Ok(services) => services,
            // Many developers may only list services per namespace.
            Err(Failure::Forbidden(_)) => {
                let (services, forbidden) = per_namespace(client, context, filter).await?;
                skipped = forbidden;
                services
            }
            Err(failure) => return Err(describe(&failure, context)),
        }
    };

    let mut found: Vec<ServiceInfo> = services
        .into_iter()
        .filter_map(|service| {
            let namespace = service.namespace()?;
            if !all && !wildcard(filter, &namespace) {
                return None;
            }
            let ports = service
                .spec
                .as_ref()?
                .ports
                .iter()
                .flatten()
                .filter(|p| p.protocol.as_deref().is_none_or(|proto| proto == "TCP"))
                .filter_map(|p| {
                    Some(PortInfo {
                        port: u16::try_from(p.port).ok()?,
                        name: p.name.clone(),
                    })
                })
                .collect();
            Some(ServiceInfo {
                namespace,
                name: service.name_any(),
                ports,
            })
        })
        .collect();
    found.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok(Listing {
        services: found,
        skipped,
    })
}

/// Whether `filter` names one namespace, without wildcards.
fn exact(filter: &str) -> bool {
    !filter.is_empty() && !filter.contains('*')
}

/// Lists the services of the namespaces that match `filter` (wildcards or
/// empty), a few namespaces at a time, for users who may not list them
/// across the cluster. Returns them with the namespaces whose services the
/// user may not list either, sorted; any other failure fails the listing.
async fn per_namespace(
    client: &Client,
    context: &str,
    filter: &str,
) -> Result<(Vec<Service>, Vec<String>), String> {
    let namespaces: Vec<String> = match list(Api::<Namespace>::all(client.clone())).await {
        Ok(namespaces) => namespaces
            .into_iter()
            .map(|ns| ns.name_any())
            .filter(|ns| filter.is_empty() || wildcard(filter, ns))
            .collect(),
        Err(Failure::Forbidden(_)) => {
            return Err(
                "you may not list services or namespaces across the cluster; enter an exact namespace name"
                    .into(),
            );
        }
        Err(failure) => return Err(describe(&failure, context)),
    };
    let mut lists = futures::stream::iter(namespaces)
        .map(|namespace| {
            let api = Api::<Service>::namespaced(client.clone(), &namespace);
            async move { (namespace, list(api).await) }
        })
        .buffer_unordered(PARALLEL);
    let mut services = Vec::new();
    let mut skipped = Vec::new();
    while let Some((namespace, listed)) = lists.next().await {
        match listed {
            Ok(listed) => services.extend(listed),
            // Rights are often granted per namespace: the others are named.
            Err(Failure::Forbidden(_)) => skipped.push(namespace),
            Err(failure) => {
                return Err(format!("{namespace}: {}", describe(&failure, context)));
            }
        }
    }
    skipped.sort();
    Ok((services, skipped))
}

/// Lists every object, a page at a time; the time limit applies per page.
async fn list<K>(api: Api<K>) -> Result<Vec<K>, Failure>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let mut items = Vec::new();
    let mut params = ListParams::default().limit(PAGE);
    loop {
        let page = match tokio::time::timeout(LIST_TIMEOUT, api.list(&params)).await {
            Ok(Ok(page)) => page,
            Ok(Err(err)) => return Err(classify(&err)),
            Err(_) => return Err(Failure::Net(NetCause::Timeout)),
        };
        items.extend(page.items);
        match page.metadata.continue_ {
            Some(token) if !token.is_empty() => params = params.continue_token(&token),
            _ => return Ok(items),
        }
    }
}

/// A failure as a sentence for the user.
pub fn describe(failure: &Failure, context: &str) -> String {
    match failure {
        Failure::Net(cause) => format!("API server unreachable ({})", cause.short()),
        Failure::Auth(_) => format!("login needed: kubectl --context {context} get ns"),
        Failure::Forbidden(message) => format!("forbidden: {message}"),
        Failure::NotFound(message) | Failure::Other(message) => message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};
    use tokio::sync::Barrier;

    use super::*;
    use crate::fake::{FakeApi, Reply, list as list_of, status};

    fn service(namespace: &str, name: &str, ports: Value) -> Value {
        json!({
            "metadata": {"name": name, "namespace": namespace},
            "spec": {"ports": ports},
        })
    }

    fn services_json(items: &[Value]) -> Reply {
        Reply::Json(200, list_of("ServiceList", items))
    }

    fn forbidden() -> Reply {
        Reply::Json(403, status(403, "services is forbidden"))
    }

    /// `namespace/name:port` of every port found.
    fn found(listing: &Listing) -> Vec<String> {
        listing
            .services
            .iter()
            .flat_map(|s| {
                s.ports
                    .iter()
                    .map(move |p| format!("{}/{}:{}", s.namespace, s.name, p.port))
            })
            .collect()
    }

    /// A cluster with services in shop-a, shop-b and web, where listing
    /// services across the cluster is forbidden.
    fn namespaced_only(namespaces_forbidden: bool) -> impl Fn(&crate::fake::Request) -> Reply {
        move |request| match request.path.as_str() {
            "/api/v1/services" => forbidden(),
            "/api/v1/namespaces" if namespaces_forbidden => forbidden(),
            "/api/v1/namespaces" => Reply::Json(
                200,
                list_of(
                    "NamespaceList",
                    &["shop-a", "shop-b", "web"].map(|name| json!({"metadata": {"name": name}})),
                ),
            ),
            path => match path
                .strip_prefix("/api/v1/namespaces/")
                .and_then(|rest| rest.strip_suffix("/services"))
            {
                Some(namespace) => services_json(&[service(
                    namespace,
                    "api",
                    json!([{"name": "http", "port": 80}]),
                )]),
                None => Reply::Json(404, status(404, "not found")),
            },
        }
    }

    #[tokio::test]
    async fn lists_across_the_cluster_and_filters_namespaces() {
        let api = FakeApi::start(|_| {
            services_json(&[
                service("web", "frontend", json!([{"port": 80}])),
                service(
                    "shop-b",
                    "db",
                    json!([
                        {"name": "mysql", "port": 3306, "protocol": "TCP"},
                        {"name": "dns", "port": 53, "protocol": "UDP"},
                    ]),
                ),
                service("shop-a", "api", json!([{"name": "http", "port": 80}])),
            ])
        })
        .await;
        let client = api.client();

        let all = list_services(&client, "staging", "*").await.unwrap();
        // Sorted, without UDP ports, a port without protocol counts as TCP.
        assert_eq!(
            found(&all),
            ["shop-a/api:80", "shop-b/db:3306", "web/frontend:80"]
        );
        assert_eq!(all.services[0].ports[0].name.as_deref(), Some("http"));
        assert!(all.skipped.is_empty());
        assert_eq!(
            found(&list_services(&client, "staging", "").await.unwrap()).len(),
            3
        );
        assert_eq!(
            found(&list_services(&client, "staging", " shop-* ").await.unwrap()),
            ["shop-a/api:80", "shop-b/db:3306"]
        );
        assert_eq!(api.requests(), ["GET /api/v1/services?&limit=500"; 3]);
    }

    #[tokio::test]
    async fn an_exact_namespace_is_listed_alone() {
        for namespaces_forbidden in [false, true] {
            let api = FakeApi::start(namespaced_only(namespaces_forbidden)).await;
            let services = list_services(&api.client(), "staging", " shop-a ")
                .await
                .unwrap();
            assert_eq!(found(&services), ["shop-a/api:80"]);
            assert_eq!(
                api.requests(),
                ["GET /api/v1/namespaces/shop-a/services?&limit=500"]
            );
        }
        // Also when the user may list services across the cluster.
        let api = FakeApi::start(|request| match request.path.as_str() {
            "/api/v1/namespaces/web/services" => {
                services_json(&[service("web", "frontend", json!([{"port": 80}]))])
            }
            _ => services_json(&[]),
        })
        .await;
        let services = list_services(&api.client(), "staging", "web")
            .await
            .unwrap();
        assert_eq!(found(&services), ["web/frontend:80"]);
        assert_eq!(
            api.requests(),
            ["GET /api/v1/namespaces/web/services?&limit=500"]
        );
    }

    #[tokio::test]
    async fn large_lists_are_read_page_by_page() {
        let api = FakeApi::start(|request| {
            let (items, next) = if request.query.contains("continue=page-2") {
                (vec![service("web", "frontend", json!([{"port": 80}]))], "")
            } else {
                (
                    vec![service("shop", "api", json!([{"port": 80}]))],
                    "page-2",
                )
            };
            let page = json!({
                "kind": "ServiceList",
                "apiVersion": "v1",
                "metadata": {"resourceVersion": "1", "continue": next},
                "items": items,
            });
            Reply::Json(200, page.to_string())
        })
        .await;
        let services = list_services(&api.client(), "staging", "").await.unwrap();
        assert_eq!(found(&services), ["shop/api:80", "web/frontend:80"]);
        assert_eq!(
            api.requests(),
            [
                "GET /api/v1/services?&limit=500",
                "GET /api/v1/services?&limit=500&continue=page-2"
            ]
        );
    }

    #[tokio::test]
    async fn without_cluster_wide_rights_the_matching_namespaces_are_listed() {
        let api = FakeApi::start(namespaced_only(false)).await;
        let client = api.client();
        let services = list_services(&client, "staging", "shop-*").await.unwrap();
        assert_eq!(found(&services), ["shop-a/api:80", "shop-b/api:80"]);
        let services = list_services(&client, "staging", "*").await.unwrap();
        assert_eq!(
            found(&services),
            ["shop-a/api:80", "shop-b/api:80", "web/api:80"]
        );
    }

    #[tokio::test]
    async fn without_cluster_wide_rights_namespaces_are_listed_side_by_side() {
        // Each namespace answers only when all three wait, which never happens
        // if they are listed one after another.
        let gate = Arc::new(Barrier::new(3));
        let api = FakeApi::start(move |request| match request.path.as_str() {
            "/api/v1/services" | "/api/v1/namespaces" => namespaced_only(false)(request),
            _ => Reply::Gated(gate.clone(), Box::new(namespaced_only(false)(request))),
        })
        .await;
        let listed = tokio::time::timeout(
            Duration::from_secs(5),
            list_services(&api.client(), "staging", "*"),
        )
        .await
        .expect("the namespaces were listed one by one");
        assert_eq!(
            found(&listed.unwrap()),
            ["shop-a/api:80", "shop-b/api:80", "web/api:80"]
        );
    }

    #[tokio::test]
    async fn without_any_listing_rights_an_exact_namespace_is_needed() {
        let api = FakeApi::start(namespaced_only(true)).await;
        let err = list_services(&api.client(), "staging", "shop-*")
            .await
            .unwrap_err();
        assert_eq!(
            err,
            "you may not list services or namespaces across the cluster; enter an exact namespace name"
        );
    }

    /// Rights are often granted per namespace: a user who may list the
    /// namespaces but read services only in some of them gets those, and the
    /// others are named, so prune can tell them from gone.
    #[tokio::test]
    async fn namespaces_whose_services_are_forbidden_are_skipped_and_named() {
        let api = FakeApi::start(|request| match request.path.as_str() {
            "/api/v1/namespaces/shop-b/services" | "/api/v1/namespaces/web/services" => forbidden(),
            _ => namespaced_only(false)(request),
        })
        .await;
        let client = api.client();
        let listing = list_services(&client, "staging", "*").await.unwrap();
        assert_eq!(found(&listing), ["shop-a/api:80"]);
        assert_eq!(listing.skipped, ["shop-b", "web"]);
        let listing = list_services(&client, "staging", "shop-*").await.unwrap();
        assert_eq!(found(&listing), ["shop-a/api:80"]);
        assert_eq!(listing.skipped, ["shop-b"]);

        // Asked for by name, a forbidden namespace is an error.
        let err = list_services(&client, "staging", "web").await.unwrap_err();
        assert_eq!(err, "forbidden: services is forbidden");
    }

    /// Only a 403 is skipped: a listing that missed a namespace for any
    /// other reason must not look complete, or prune would take its
    /// forwards for gone.
    #[tokio::test]
    async fn any_other_failure_in_a_namespace_fails_the_whole_listing() {
        for (reply, expected) in [
            (
                Reply::Json(401, status(401, "Unauthorized")),
                "shop-b: login needed: kubectl --context staging get ns",
            ),
            (
                Reply::Json(500, status(500, "etcdserver: leader changed")),
                "shop-b: ApiError: etcdserver: leader changed",
            ),
        ] {
            let reply = Arc::new(std::sync::Mutex::new(Some(reply)));
            let api = FakeApi::start(move |request| match request.path.as_str() {
                "/api/v1/namespaces/shop-b/services" => reply.lock().unwrap().take().unwrap(),
                _ => namespaced_only(false)(request),
            })
            .await;
            let err = list_services(&api.client(), "staging", "shop-*")
                .await
                .unwrap_err();
            assert!(err.starts_with(expected), "{err}");
        }
    }

    #[tokio::test]
    async fn failures_name_what_to_do() {
        let api = FakeApi::start(|_| Reply::Json(401, status(401, "Unauthorized"))).await;
        let err = list_services(&api.client(), "staging", "*")
            .await
            .unwrap_err();
        assert_eq!(err, "login needed: kubectl --context staging get ns");

        let port = crate::fake::closed_port();
        let client = Client::try_from(kube::Config::new(
            format!("http://127.0.0.1:{port}").parse().unwrap(),
        ))
        .unwrap();
        let err = list_services(&client, "staging", "*").await.unwrap_err();
        assert_eq!(err, "API server unreachable (refused)");
    }

    #[test]
    fn describes_each_failure() {
        let cases = [
            (Failure::Net(NetCause::Dns), "API server unreachable (DNS)"),
            (
                Failure::Auth("token expired".into()),
                "login needed: kubectl --context staging get ns",
            ),
            (Failure::Forbidden("no".into()), "forbidden: no"),
            (Failure::NotFound("gone".into()), "gone"),
            (Failure::Other("odd".into()), "odd"),
        ];
        for (failure, expected) in cases {
            assert_eq!(describe(&failure, "staging"), expected);
        }
    }
}
