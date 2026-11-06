//! Noticing kubeconfig changes per context.

use std::collections::HashMap;
use std::ffi::OsString;
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
    files_of(std::env::var_os("KUBECONFIG"), std::env::var_os("HOME"))
}

/// The kubeconfig files for these values of `$KUBECONFIG` and `$HOME`.
fn files_of(kubeconfig: Option<OsString>, home: Option<OsString>) -> Vec<PathBuf> {
    match kubeconfig.filter(|value| !value.is_empty()) {
        Some(value) => std::env::split_paths(&value).collect(),
        None => home
            .map(|home| vec![PathBuf::from(home).join(".kube").join("config")])
            .unwrap_or_default(),
    }
}

/// A hash per context over its context, cluster and user entries, to tell
/// which contexts a change of the kubeconfig touched. Only the hash is kept,
/// never the settings, which include credentials.
pub fn fingerprints() -> Result<HashMap<String, u64>, String> {
    fingerprints_of(&Kubeconfig::read().map_err(|err| describe(&err))?)
}

fn fingerprints_of(config: &Kubeconfig) -> Result<HashMap<String, u64>, String> {
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

    /// Contexts `a` (cluster one, user alice), `b` (cluster two, user alice)
    /// and `c` (cluster two, user bob); `{one}`, `{alice}` and `{current}`
    /// are filled in.
    const TEMPLATE: &str = r#"
apiVersion: v1
kind: Config
current-context: {current}
clusters:
- name: one
  cluster:
    server: {one}
- name: two
  cluster:
    server: https://two.example:6443
    certificate-authority-data: Y2VydGlmaWNhdGU=
users:
- name: alice
  user:
    token: {alice}
- name: bob
  user:
    client-certificate-data: Y2VydA==
    client-key-data: a2V5
contexts:
- name: a
  context:
    cluster: one
    user: alice
- name: b
  context:
    cluster: two
    user: alice
- name: c
  context:
    cluster: two
    user: bob
"#;

    fn prints(one: &str, alice: &str, current: &str) -> HashMap<String, u64> {
        let yaml = TEMPLATE
            .replace("{one}", one)
            .replace("{alice}", alice)
            .replace("{current}", current);
        fingerprints_of(&Kubeconfig::from_yaml(&yaml).unwrap()).unwrap()
    }

    /// The contexts whose print differs.
    fn changed(before: &HashMap<String, u64>, after: &HashMap<String, u64>) -> Vec<String> {
        let mut names: Vec<String> = before
            .keys()
            .chain(after.keys())
            .filter(|name| before.get(*name) != after.get(*name))
            .cloned()
            .collect();
        names.sort();
        names.dedup();
        names
    }

    const ONE: &str = "https://one.example:6443";

    #[test]
    fn the_same_kubeconfig_gives_the_same_prints() {
        let before = prints(ONE, TOKEN, "a");
        assert_eq!(before.len(), 3);
        assert!(changed(&before, &prints(ONE, TOKEN, "a")).is_empty());

        // Read from a file twice, as the daemon does after a change.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config");
        let yaml = TEMPLATE
            .replace("{one}", ONE)
            .replace("{alice}", TOKEN)
            .replace("{current}", "a");
        std::fs::write(&file, yaml).unwrap();
        let read = || fingerprints_of(&Kubeconfig::read_from(&file).unwrap()).unwrap();
        assert_eq!(read(), read());
        assert_eq!(read(), before);
    }

    #[test]
    fn a_change_touches_only_the_contexts_that_use_it() {
        let before = prints(ONE, TOKEN, "a");
        // A moved cluster.
        assert_eq!(
            changed(&before, &prints("https://moved.example:6443", TOKEN, "a")),
            ["a"]
        );
        // A renewed token of a user two contexts share.
        assert_eq!(
            changed(&before, &prints(ONE, "tok-renewed", "a")),
            ["a", "b"]
        );
        // `kubectl config use-context` touches none.
        assert!(changed(&before, &prints(ONE, TOKEN, "c")).is_empty());
    }

    #[test]
    fn added_and_removed_contexts_show_as_changed() {
        let before = prints(ONE, TOKEN, "a");
        let mut yaml = TEMPLATE
            .replace("{one}", ONE)
            .replace("{alice}", TOKEN)
            .replace("{current}", "a");
        yaml.push_str("- name: d\n  context:\n    cluster: one\n    user: bob\n");
        let after = fingerprints_of(&Kubeconfig::from_yaml(&yaml).unwrap()).unwrap();
        assert_eq!(changed(&before, &after), ["d"]);
        assert_eq!(changed(&after, &before), ["d"]);
    }

    #[test]
    fn kubeconfig_files_come_from_the_environment() {
        assert_eq!(
            files_of(Some("/k/one:/k/two".into()), Some("/home/dev".into())),
            [PathBuf::from("/k/one"), PathBuf::from("/k/two")]
        );
        for unset in [None, Some(OsString::new())] {
            assert_eq!(
                files_of(unset, Some("/home/dev".into())),
                [PathBuf::from("/home/dev/.kube/config")]
            );
        }
        assert!(files_of(None, None).is_empty());
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
