//! Reading the kubeconfig files, and noticing changes per context.

use std::collections::HashMap;
use std::ffi::OsString;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::PathBuf;

use kube::config::{Kubeconfig, KubeconfigError};
use serde_json::Value;

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
/// Empty entries of `$KUBECONFIG` are ignored, as kubectl and kube-rs do.
fn files_of(kubeconfig: Option<OsString>, home: Option<OsString>) -> Vec<PathBuf> {
    let listed: Vec<PathBuf> = kubeconfig
        .map(|value| {
            std::env::split_paths(&value)
                .filter(|path| !path.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default();
    if !listed.is_empty() {
        return listed;
    }
    home.map(|home| vec![PathBuf::from(home).join(".kube").join("config")])
        .unwrap_or_default()
}

/// The kubeconfig from [`files`], merged as kubectl merges them: the first
/// file to set a value wins, and a file that does not exist is skipped, so a
/// stale entry in `$KUBECONFIG` breaks nothing. A file that cannot be read or
/// parsed is an error, and so is finding no file at all.
pub fn read() -> Result<Kubeconfig, String> {
    read_files(&files())
}

fn read_files(files: &[PathBuf]) -> Result<Kubeconfig, String> {
    let mut merged = Kubeconfig::default();
    let mut found = false;
    for file in files {
        let config = match Kubeconfig::read_from(file) {
            Ok(config) => config,
            Err(KubeconfigError::ReadConfig(err, _)) if err.kind() == io::ErrorKind::NotFound => {
                continue;
            }
            Err(err) => return Err(describe(&err)),
        };
        merged = merged.merge(config).map_err(|err| describe(&err))?;
        found = true;
    }
    if found {
        return Ok(merged);
    }
    if files.is_empty() {
        return Err(describe(&KubeconfigError::FindPath));
    }
    let names: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
    Err(format!("no kubeconfig file found: {}", names.join(", ")))
}

/// A hash per context over what its client is built from, to tell which
/// contexts a change of the kubeconfig touched: the names of its cluster and
/// user and their entries. The context's default namespace and extensions do
/// not count; forwards carry their own namespace, so `kubens` must not
/// reconnect them. Only the hash is kept, never the settings, which include
/// credentials.
pub fn fingerprints() -> Result<HashMap<String, u64>, String> {
    fingerprints_of(&read()?)
}

fn fingerprints_of(config: &Kubeconfig) -> Result<HashMap<String, u64>, String> {
    let mut prints = HashMap::new();
    for named in &config.contexts {
        let context = named.context.as_ref();
        let cluster_name = context.map(|c| &c.cluster);
        let user_name = context.and_then(|c| c.user.as_ref());
        let cluster = cluster_name
            .and_then(|name| config.clusters.iter().find(|c| &c.name == name))
            .and_then(|named| named.cluster.as_ref());
        let user = user_name
            .and_then(|name| config.auth_infos.iter().find(|a| &a.name == name))
            .and_then(|named| named.auth_info.as_ref());
        let settings = serde_json::to_value((cluster_name, user_name, cluster, user))
            .map_err(|err| format!("cannot read kubeconfig: {err}"))?;
        let mut hasher = DefaultHasher::new();
        hash_canonical(&settings, &mut hasher);
        prints.insert(named.name.clone(), hasher.finish());
    }
    Ok(prints)
}

/// Feeds `value` to `hasher` with the keys of every object in sorted order.
/// kube-rs keeps some settings in `HashMap`s, which every read fills in
/// another order (exec env entries, auth-provider config); the print must
/// not depend on that.
fn hash_canonical(value: &Value, hasher: &mut impl Hasher) {
    match value {
        Value::Null => 0u8.hash(hasher),
        Value::Bool(flag) => (1u8, flag).hash(hasher),
        Value::Number(number) => (2u8, number.to_string()).hash(hasher),
        Value::String(text) => (3u8, text).hash(hasher),
        Value::Array(items) => {
            (4u8, items.len()).hash(hasher);
            for item in items {
                hash_canonical(item, hasher);
            }
        }
        Value::Object(map) => {
            (5u8, map.len()).hash(hasher);
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            for (key, value) in entries {
                key.hash(hasher);
                hash_canonical(value, hasher);
            }
        }
    }
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

    /// A user whose settings kube-rs keeps in `HashMap`s: exec env entries,
    /// auth-provider config and impersonation extras.
    const MAPS: &str = r#"
apiVersion: v1
kind: Config
clusters:
- name: one
  cluster:
    server: https://one.example:6443
users:
- name: eks
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      command: aws
      args: [eks, get-token, --cluster-name, shop]
      env:
      - name: AWS_PROFILE
        value: shop
      - name: AWS_REGION
        value: eu-central-1
      - name: AWS_STS_REGIONAL_ENDPOINTS
        value: regional
- name: legacy
  user:
    auth-provider:
      name: oidc
      config:
        client-id: shop
        idp-issuer-url: https://issuer.example
        extra-scopes: groups
    as: admin
    as-user-extra:
      team: [shop]
      reason: [support]
contexts:
- name: eks
  context:
    cluster: one
    user: eks
- name: legacy
  context:
    cluster: one
    user: legacy
"#;

    #[test]
    fn maps_in_the_settings_do_not_change_the_print() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config");
        std::fs::write(&file, MAPS).unwrap();
        // Every read builds new maps, each with its own iteration order.
        let read = || fingerprints_of(&Kubeconfig::read_from(&file).unwrap()).unwrap();
        let first = read();
        assert_eq!(first.len(), 2);
        for _ in 0..20 {
            assert_eq!(read(), first);
        }

        // A value in a map still counts.
        std::fs::write(&file, MAPS.replace("value: shop", "value: other")).unwrap();
        assert_eq!(changed(&first, &read()), ["eks"]);
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
    fn the_default_namespace_and_extensions_do_not_count() {
        let yaml = TEMPLATE
            .replace("{one}", ONE)
            .replace("{alice}", TOKEN)
            .replace("{current}", "a");
        let prints_of =
            |yaml: &str| fingerprints_of(&Kubeconfig::from_yaml(yaml).unwrap()).unwrap();
        let before = prints_of(&yaml);
        // `kubens shop` on context a, and a tool that stamps the context.
        let context_a = "    cluster: one\n    user: alice\n";
        let after = yaml.replacen(
            context_a,
            &format!(
                "{context_a}    namespace: shop\n    extensions:\n    - name: tool\n      extension:\n        last-update: 2026-11-06\n"
            ),
            1,
        );
        assert_ne!(after, yaml);
        assert_eq!(prints_of(&after).len(), 3);
        assert!(changed(&before, &prints_of(&after)).is_empty());

        // The cluster or user a context names does count.
        let other_user = yaml.replacen(context_a, "    cluster: one\n    user: bob\n", 1);
        assert_eq!(changed(&before, &prints_of(&other_user)), ["a"]);
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
        assert_eq!(
            files_of(Some(":/k/one::".into()), Some("/home/dev".into())),
            [PathBuf::from("/k/one")]
        );
        for unset in [None, Some(OsString::new()), Some(":".into())] {
            assert_eq!(
                files_of(unset, Some("/home/dev".into())),
                [PathBuf::from("/home/dev/.kube/config")]
            );
        }
        assert!(files_of(None, None).is_empty());
    }

    /// Context `d` on cluster one, current.
    const SECOND: &str = r#"
apiVersion: v1
kind: Config
current-context: d
clusters:
- name: one
  cluster:
    server: https://elsewhere.example:6443
users:
- name: dave
  user:
    token: tok-dave
contexts:
- name: d
  context:
    cluster: one
    user: dave
"#;

    #[test]
    fn files_that_do_not_exist_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let gone = dir.path().join("gone");
        let yaml = TEMPLATE
            .replace("{one}", ONE)
            .replace("{alice}", TOKEN)
            .replace("{current}", "a");
        std::fs::write(&first, &yaml).unwrap();
        std::fs::write(&second, SECOND).unwrap();

        let merged = read_files(&[gone.clone(), first.clone(), gone.clone(), second]).unwrap();
        let names: Vec<&str> = merged.contexts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "d"]);
        // The first file to set a value wins, as in kubectl.
        assert_eq!(merged.current_context.as_deref(), Some("a"));
        let one = merged.clusters.iter().find(|c| c.name == "one").unwrap();
        assert_eq!(one.cluster.as_ref().unwrap().server.as_deref(), Some(ONE));

        // Skipping changes nothing for the files that exist.
        assert_eq!(
            fingerprints_of(&read_files(&[first.clone(), gone.clone()]).unwrap()).unwrap(),
            fingerprints_of(&Kubeconfig::from_yaml(&yaml).unwrap()).unwrap()
        );

        // No file at all is an error that names the files.
        let message = read_files(std::slice::from_ref(&gone)).unwrap_err();
        assert_eq!(
            message,
            format!("no kubeconfig file found: {}", gone.display())
        );
        assert!(read_files(&[]).is_err());

        // A file that exists but cannot be parsed is still an error.
        std::fs::write(&first, "users: [\n").unwrap();
        let message = read_files(&[gone, first]).unwrap_err();
        assert!(
            message.starts_with("cannot parse the kubeconfig"),
            "{message:?}"
        );
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
