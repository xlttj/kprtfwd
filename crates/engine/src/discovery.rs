//! Finding services to forward: the kubeconfig's contexts and a context's
//! services.

use std::time::Duration;

use k8s_openapi::api::core::v1::{Namespace, Service};
use kprtfwd_core::filter::wildcard;
use kprtfwd_core::state::NetCause;
use kube::api::ListParams;
use kube::config::Kubeconfig;
use kube::{Api, Client, ResourceExt};

use crate::context::{PROBE_TIMEOUT, answered, client_for, probe};
use crate::errors::{Failure, classify};
use crate::kubeconfig;

const LIST_TIMEOUT: Duration = Duration::from_secs(30);

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
    let config = Kubeconfig::read().map_err(|err| kubeconfig::describe(&err))?;
    let mut names: Vec<String> = config.contexts.iter().map(|c| c.name.clone()).collect();
    names.sort();
    Ok((names, config.current_context))
}

/// The services in `context` whose namespace matches `namespace_filter` (`*`
/// wildcards, empty for all), sorted by namespace and name.
pub async fn services(context: &str, namespace_filter: &str) -> Result<Vec<ServiceInfo>, String> {
    let (client, login) = client_for(context).await?;
    if let Some(login) = login {
        // Log in only if the API server answers: a login may open the browser.
        let answer = probe(&client, PROBE_TIMEOUT).await;
        if let Err(failure) = &answer
            && !answered(&answer)
        {
            return Err(describe(failure, context));
        }
        login
            .run()
            .await
            .map_err(|message| format!("login failed: {message}"))?;
    }
    list_services(&client, context, namespace_filter).await
}

/// [`services`] with a client that is ready to use.
async fn list_services(
    client: &Client,
    context: &str,
    namespace_filter: &str,
) -> Result<Vec<ServiceInfo>, String> {
    let filter = namespace_filter.trim();
    let all = filter.is_empty() || filter == "*";

    let listed = list(Api::<Service>::all(client.clone())).await;
    let services = match listed {
        Ok(services) => services,
        // Many developers may only list services per namespace.
        Err(Failure::Forbidden(_)) => per_namespace(client, context, filter).await?,
        Err(failure) => return Err(describe(&failure, context)),
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
    Ok(found)
}

/// Lists services namespace by namespace, for users who may not list them
/// across the cluster.
async fn per_namespace(
    client: &Client,
    context: &str,
    filter: &str,
) -> Result<Vec<Service>, String> {
    let namespaces: Vec<String> = if !filter.is_empty() && !filter.contains('*') {
        vec![filter.to_owned()]
    } else {
        match list(Api::<Namespace>::all(client.clone())).await {
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
        }
    };
    let mut services = Vec::new();
    for namespace in namespaces {
        let api = Api::<Service>::namespaced(client.clone(), &namespace);
        services.extend(
            list(api)
                .await
                .map_err(|failure| format!("{namespace}: {}", describe(&failure, context)))?,
        );
    }
    Ok(services)
}

async fn list<K>(api: Api<K>) -> Result<Vec<K>, Failure>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    match tokio::time::timeout(LIST_TIMEOUT, api.list(&ListParams::default())).await {
        Ok(Ok(list)) => Ok(list.items),
        Ok(Err(err)) => Err(classify(&err)),
        Err(_) => Err(Failure::Net(NetCause::Timeout)),
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
    use serde_json::{Value, json};

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
    fn found(services: &[ServiceInfo]) -> Vec<String> {
        services
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
        assert_eq!(all[0].ports[0].name.as_deref(), Some("http"));
        assert_eq!(
            found(&list_services(&client, "staging", "").await.unwrap()).len(),
            3
        );
        assert_eq!(
            found(&list_services(&client, "staging", " shop-* ").await.unwrap()),
            ["shop-a/api:80", "shop-b/db:3306"]
        );
        assert_eq!(api.requests(), ["GET /api/v1/services?"; 3]);
    }

    #[tokio::test]
    async fn without_cluster_wide_rights_an_exact_namespace_is_listed_alone() {
        let api = FakeApi::start(namespaced_only(true)).await;
        let services = list_services(&api.client(), "staging", "shop-a")
            .await
            .unwrap();
        assert_eq!(found(&services), ["shop-a/api:80"]);
        assert_eq!(
            api.requests(),
            [
                "GET /api/v1/services?",
                "GET /api/v1/namespaces/shop-a/services?"
            ]
        );
    }

    #[tokio::test]
    async fn without_cluster_wide_rights_matching_namespaces_are_listed_one_by_one() {
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

    /// Prune deletes what is not listed, so a listing that misses a
    /// namespace must not look complete.
    #[tokio::test]
    async fn a_namespace_that_cannot_be_listed_fails_the_whole_listing() {
        let api = FakeApi::start(|request| match request.path.as_str() {
            "/api/v1/namespaces/shop-b/services" => forbidden(),
            _ => namespaced_only(false)(request),
        })
        .await;
        let err = list_services(&api.client(), "staging", "shop-*")
            .await
            .unwrap_err();
        assert_eq!(err, "shop-b: forbidden: services is forbidden");
    }

    #[tokio::test]
    async fn failures_name_what_to_do() {
        let api = FakeApi::start(|_| Reply::Json(401, status(401, "Unauthorized"))).await;
        let err = list_services(&api.client(), "staging", "*")
            .await
            .unwrap_err();
        assert_eq!(err, "login needed: kubectl --context staging get ns");

        let port = crate::fake::free_port();
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
