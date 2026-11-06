//! Finding services to forward: the kubeconfig's contexts and a context's
//! services.

use std::time::Duration;

use k8s_openapi::api::core::v1::{Namespace, Service};
use kprtfwd_core::filter::wildcard;
use kprtfwd_core::state::NetCause;
use kube::api::ListParams;
use kube::config::Kubeconfig;
use kube::{Api, Client, ResourceExt};

use crate::context::{answered, client_for, probe};
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
        let answer = probe(&client).await;
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
    let filter = namespace_filter.trim();
    let all = filter.is_empty() || filter == "*";

    let listed = list(Api::<Service>::all(client.clone())).await;
    let services = match listed {
        Ok(services) => services,
        // Many developers may only list services per namespace.
        Err(Failure::Forbidden(_)) => per_namespace(&client, context, filter).await?,
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
