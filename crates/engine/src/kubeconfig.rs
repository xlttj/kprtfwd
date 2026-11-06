//! Noticing kubeconfig changes per context.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

use kube::config::{Kubeconfig, KubeconfigError};

/// A kubeconfig error as text for messages, events and the log. A parse
/// error keeps only its line and column: the parser would quote the lines
/// around the error, and serde's message can quote the value itself, which
/// may be a token or a key.
pub fn describe(err: &KubeconfigError) -> String {
    match err {
        KubeconfigError::Parse(parse) => match parse.location() {
            Some(at) => format!(
                "cannot parse the kubeconfig at line {}, column {}",
                at.line(),
                at.column()
            ),
            None => "cannot parse the kubeconfig".to_owned(),
        },
        other => other.to_string(),
    }
}

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
    let config = Kubeconfig::read().map_err(|err| describe(&err))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "tok-0123456789abcdef";
    const KEY: &str = "c2VjcmV0LWtleS1kYXRh";

    fn parse_error(yaml: &str) -> KubeconfigError {
        Kubeconfig::from_yaml(yaml).expect_err("a parse error")
    }

    fn assert_clean(message: &str) {
        for secret in [TOKEN, KEY, "client-key-data"] {
            assert!(!message.contains(secret), "{message:?} contains {secret:?}");
        }
    }

    #[test]
    fn parse_errors_name_the_place_but_no_content() {
        let broken = format!(
            "apiVersion: v1\nkind: Config\nusers:\n- name: shop\n  user:\n    token: {TOKEN}\n   client-key-data: {KEY}\n"
        );
        let message = describe(&parse_error(&broken));
        assert!(
            message.starts_with("cannot parse the kubeconfig at line "),
            "{message:?}"
        );
        assert_clean(&message);

        // serde's own messages quote a value of the wrong type.
        let wrong_type = format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: shop\n  cluster:\n    server: https://shop.example\n    insecure-skip-tls-verify: {TOKEN}\n"
        );
        let message = describe(&parse_error(&wrong_type));
        assert!(
            message.starts_with("cannot parse the kubeconfig"),
            "{message:?}"
        );
        assert_clean(&message);
    }

    #[test]
    fn read_errors_of_a_file_keep_no_content_either() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config");
        std::fs::write(
            &file,
            format!(
                "users:\n- name: shop\n  user:\n    token: {TOKEN}\n   client-key-data: {KEY}\n"
            ),
        )
        .unwrap();
        let message = describe(&Kubeconfig::read_from(&file).unwrap_err());
        assert!(
            message.starts_with("cannot parse the kubeconfig"),
            "{message:?}"
        );
        assert_clean(&message);

        let missing = describe(&Kubeconfig::read_from(dir.path().join("missing")).unwrap_err());
        assert!(missing.contains("missing"), "{missing:?}");
    }
}
