//! Noticing kubeconfig changes per context.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

use kube::config::Kubeconfig;

/// The kubeconfig files in use: the entries of `$KUBECONFIG`, or
/// `~/.kube/config`.
pub fn files() -> Vec<PathBuf> {
    match std::env::var_os("KUBECONFIG").filter(|value| !value.is_empty()) {
        Some(value) => std::env::split_paths(&value).collect(),
        None => std::env::var_os("HOME")
            .map(|home| vec![PathBuf::from(home).join(".kube").join("config")])
            .unwrap_or_default(),
    }
}

/// A hash per context over its context, cluster and user entries, to tell
/// which contexts a change of the kubeconfig touched. Only the hash is kept,
/// never the settings, which include credentials.
pub fn fingerprints() -> Result<HashMap<String, u64>, String> {
    let config = Kubeconfig::read().map_err(|err| format!("cannot read kubeconfig: {err}"))?;
    let mut prints = HashMap::new();
    for named in &config.contexts {
        let context = named.context.as_ref();
        let cluster = context.and_then(|c| config.clusters.iter().find(|cl| cl.name == c.cluster));
        let user = context
            .and_then(|c| c.user.as_ref())
            .and_then(|user| config.auth_infos.iter().find(|a| &a.name == user));
        let settings = serde_json::to_string(&(context, cluster, user))
            .map_err(|err| format!("cannot read kubeconfig: {err}"))?;
        let mut hasher = DefaultHasher::new();
        settings.hash(&mut hasher);
        prints.insert(named.name.clone(), hasher.finish());
    }
    Ok(prints)
}
