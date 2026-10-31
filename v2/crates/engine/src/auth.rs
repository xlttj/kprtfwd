//! Logins through kubeconfig exec plugins, such as kubelogin.
//!
//! kube-rs runs the plugin inside its requests: once when it builds the client,
//! and again in every request while the token is expiring. A plugin that logs
//! in through the browser then opens a tab for every attempt, even while the
//! API server is unreachable, and a request that times out leaves its plugin
//! running while the next request starts another one.
//!
//! So the daemon takes the plugin out of the client's settings and runs it
//! itself, from one place only: the context's health probe. The client sends
//! the token of the last successful run, or no credentials at all.

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use hyper::Request;
use hyper::header::{AUTHORIZATION, HeaderValue};
use kube::client::Body;
use kube::config::{AuthInfo, ExecAuthCluster, ExecConfig};
use serde::{Deserialize, Serialize};
use tower::util::MapRequestLayer;

/// Log in again this long before the token expires, as kube-rs does.
const REFRESH_BEFORE: Duration = Duration::from_secs(60);
/// The longest a login may take. A browser login waits for the user;
/// kubelogin gives up after 3 minutes.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Flags and variables whose name contains one of these words have their
/// values kept out of messages.
const SECRET_WORDS: [&str; 4] = ["secret", "password", "token", "key"];

/// A context's exec plugin and the token it returned last.
#[derive(Clone)]
pub struct Login {
    exec: Arc<ExecConfig>,
    token: Arc<RwLock<Option<Token>>>,
}

struct Token {
    header: HeaderValue,
    expires: Option<SystemTime>,
}

impl Login {
    /// Takes the exec plugin out of `auth`, if that is how the context logs
    /// in. kube-rs prefers other credentials over the plugin, so with those
    /// everything stays as it is.
    pub fn take(auth: &mut AuthInfo) -> Option<Self> {
        let other_credentials = auth.auth_provider.is_some()
            || auth.username.is_some()
            || auth.token.is_some()
            || auth.token_file.is_some();
        if other_credentials {
            return None;
        }
        Some(Self {
            exec: Arc::new(auth.exec.take()?),
            token: Arc::default(),
        })
    }

    /// The plugin for messages, like "kubectl oidc-login": the command and its
    /// first argument. The other arguments may hold secrets.
    pub fn name(&self) -> String {
        let command = self.exec.command.as_deref().unwrap_or("exec plugin");
        let command = Path::new(command)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(command);
        match self.exec.args.as_deref().and_then(<[String]>::first) {
            Some(first) if !first.starts_with('-') => format!("{command} {first}"),
            _ => command.to_owned(),
        }
    }

    /// Whether there is a token, even one that expires soon.
    pub fn has_token(&self) -> bool {
        self.token.read().unwrap().is_some()
    }

    /// Whether there is no token, or it expires soon.
    pub fn needed(&self) -> bool {
        match &*self.token.read().unwrap() {
            None => true,
            Some(token) => token
                .expires
                .is_some_and(|expires| SystemTime::now() + REFRESH_BEFORE >= expires),
        }
    }

    /// Forgets the token, after the API server rejected it.
    pub fn forget(&self) {
        *self.token.write().unwrap() = None;
    }

    /// Adds the token, if there is one, to every request of a client.
    pub fn layer(
        &self,
    ) -> MapRequestLayer<impl FnMut(Request<Body>) -> Request<Body> + Clone + Send + Sync + 'static>
    {
        let token = self.token.clone();
        MapRequestLayer::new(move |mut request: Request<Body>| {
            if let Some(token) = &*token.read().unwrap() {
                request
                    .headers_mut()
                    .insert(AUTHORIZATION, token.header.clone());
            }
            request
        })
    }

    /// Runs the plugin and keeps the token it returns. Dropping the future
    /// ends the plugin. The error never contains the plugin's arguments.
    pub async fn run(&self) -> Result<(), String> {
        let name = self.name();
        let command = self
            .exec
            .command
            .as_deref()
            .ok_or("the kubeconfig's exec section has no command")?;
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(self.exec.args.iter().flatten())
            .envs(variables(&self.exec))
            .env("KUBERNETES_EXEC_INFO", exec_info(&self.exec)?)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = match tokio::time::timeout(LOGIN_TIMEOUT, cmd.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                let hint = self
                    .exec
                    .install_hint
                    .as_deref()
                    .and_then(|hint| hint.lines().map(str::trim).find(|line| !line.is_empty()))
                    .map(|hint| format!(" ({hint})"))
                    .unwrap_or_default();
                return Err(format!("{command} not found{hint}"));
            }
            Ok(Err(err)) => return Err(format!("cannot run {name}: {err}")),
            Err(_) => {
                return Err(format!(
                    "{name} did not finish within {} minutes",
                    LOGIN_TIMEOUT.as_secs() / 60
                ));
            }
        };
        if !output.status.success() {
            let mut message = format!("{name} failed ({})", output.status);
            if let Some(line) = last_line(&output.stderr) {
                message = format!("{message}: {line}");
            }
            return Err(self.redact(message));
        }
        let token = parse(&output.stdout).map_err(|problem| format!("{name} {problem}"))?;
        *self.token.write().unwrap() = Some(token);
        Ok(())
    }

    /// Replaces the values of secret-looking flags and variables in `text`.
    fn redact(&self, text: String) -> String {
        secrets(&self.exec)
            .iter()
            .fold(text, |text, secret| text.replace(secret.as_str(), "***"))
    }
}

/// `KUBERNETES_EXEC_INFO`, as kubectl sets it.
#[derive(Serialize)]
struct ExecInfo<'a> {
    #[serde(rename = "apiVersion", skip_serializing_if = "Option::is_none")]
    api_version: Option<&'a str>,
    kind: &'static str,
    spec: ExecInfoSpec<'a>,
}

#[derive(Serialize)]
struct ExecInfoSpec<'a> {
    /// The daemon has no terminal to prompt on. kubelogin ignores this and
    /// opens the browser anyway.
    interactive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    cluster: Option<&'a ExecAuthCluster>,
}

fn exec_info(exec: &ExecConfig) -> Result<String, String> {
    let cluster = if exec.provide_cluster_info {
        Some(
            exec.cluster
                .as_ref()
                .ok_or("the exec plugin wants the cluster's settings, but there are none")?,
        )
    } else {
        None
    };
    serde_json::to_string(&ExecInfo {
        api_version: exec.api_version.as_deref(),
        kind: "ExecCredential",
        spec: ExecInfoSpec {
            interactive: false,
            cluster,
        },
    })
    .map_err(|err| err.to_string())
}

fn variables(exec: &ExecConfig) -> impl Iterator<Item = (&String, &String)> {
    exec.env
        .iter()
        .flatten()
        .filter_map(|env| Some((env.get("name")?, env.get("value")?)))
}

fn secrets(exec: &ExecConfig) -> Vec<String> {
    let secret = |name: &str| {
        let name = name.to_lowercase();
        SECRET_WORDS.iter().any(|word| name.contains(word))
    };
    let mut found = Vec::new();
    let args = exec.args.as_deref().unwrap_or_default();
    for (i, arg) in args.iter().enumerate() {
        let (flag, value) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (arg.as_str(), None),
        };
        if !flag.starts_with('-') || !secret(flag) {
            continue;
        }
        if let Some(value) = value.or(args.get(i + 1).map(String::as_str)) {
            found.push(value.to_owned());
        }
    }
    for (name, value) in variables(exec) {
        if secret(name) {
            found.push(value.clone());
        }
    }
    // Replacing very short values would garble the message.
    found.retain(|value| value.len() >= 4);
    found
}

/// The plugin's last words on stderr, which usually say what went wrong.
fn last_line(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().map(str::trim).rfind(|line| !line.is_empty())?;
    Some(line.chars().take(300).collect())
}

#[derive(Deserialize)]
struct Credential {
    status: Option<Status>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    token: Option<String>,
    expiration_timestamp: Option<String>,
    client_certificate_data: Option<String>,
}

/// Reads the plugin's `ExecCredential`. The error completes "<plugin> …".
fn parse(stdout: &[u8]) -> Result<Token, String> {
    let credential: Credential = serde_json::from_slice(stdout)
        .map_err(|err| format!("printed no valid ExecCredential: {err}"))?;
    let status = credential.status.ok_or("returned no credentials")?;
    let Some(token) = status.token else {
        return Err(if status.client_certificate_data.is_some() {
            "returned a client certificate; kprtfwd supports only tokens from exec plugins".into()
        } else {
            "returned no token".into()
        });
    };
    let expires = status
        .expiration_timestamp
        .map(|timestamp| {
            timestamp
                .parse::<jiff::Timestamp>()
                .map(SystemTime::from)
                .map_err(|err| format!("returned a bad expirationTimestamp: {err}"))
        })
        .transpose()?;
    let mut header = HeaderValue::try_from(format!("Bearer {token}"))
        .map_err(|_| "returned a token that cannot be sent".to_owned())?;
    header.set_sensitive(true);
    Ok(Token { header, expires })
}

/// Fake exec plugins for tests.
#[cfg(test)]
pub(crate) mod fake {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// A plugin that appends its `KUBERNETES_EXEC_INFO` to `$RUNS` and then,
    /// by `$MODE`: prints the token "good" (`ok`), the token "bad" (`bad`),
    /// or fails with its arguments on stderr (`fail`).
    pub fn plugin(dir: &Path) -> PathBuf {
        let path = dir.join("plugin");
        std::fs::write(
            &path,
            r#"#!/bin/sh
echo "$KUBERNETES_EXEC_INFO" >> "$RUNS"
case "$MODE" in
  fail)
    echo "opening the browser" >&2
    echo "error: login timed out, args: $*" >&2
    exit 1 ;;
  bad)
    echo '{"apiVersion":"client.authentication.k8s.io/v1","kind":"ExecCredential","status":{"token":"bad"}}' ;;
  *)
    echo '{"apiVersion":"client.authentication.k8s.io/v1","kind":"ExecCredential","status":{"token":"good","expirationTimestamp":"2999-01-01T00:00:00Z"}}' ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// The `KUBERNETES_EXEC_INFO` of every run so far.
    pub fn runs(file: &Path) -> Vec<String> {
        std::fs::read_to_string(file)
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn exec(command: &str, args: &[&str], env: &[(&str, &str)]) -> ExecConfig {
        ExecConfig {
            api_version: Some("client.authentication.k8s.io/v1".into()),
            command: Some(command.into()),
            args: Some(args.iter().map(|arg| (*arg).to_owned()).collect()),
            env: Some(
                env.iter()
                    .map(|(name, value)| {
                        HashMap::from([
                            ("name".to_owned(), (*name).to_owned()),
                            ("value".to_owned(), (*value).to_owned()),
                        ])
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    fn login(exec: ExecConfig) -> Login {
        let mut auth = AuthInfo {
            exec: Some(exec),
            ..Default::default()
        };
        Login::take(&mut auth).unwrap()
    }

    #[test]
    fn takes_the_plugin_only_if_it_is_the_login() {
        let mut auth = AuthInfo {
            exec: Some(exec("kubectl", &[], &[])),
            ..Default::default()
        };
        assert!(Login::take(&mut auth).is_some());
        assert!(auth.exec.is_none());

        let mut auth = AuthInfo {
            exec: Some(exec("kubectl", &[], &[])),
            token_file: Some("/token".into()),
            ..Default::default()
        };
        assert!(Login::take(&mut auth).is_none());
        assert!(auth.exec.is_some());
    }

    #[test]
    fn names_the_plugin_without_its_arguments() {
        let name = |command: &str, args: &[&str]| login(exec(command, args, &[])).name();
        assert_eq!(
            name("kubectl", &["oidc-login", "get-token", "--x-secret=s3cr3t"]),
            "kubectl oidc-login"
        );
        assert_eq!(
            name("/usr/local/bin/aws", &["--region", "eu", "eks"]),
            "aws"
        );
    }

    #[test]
    fn keeps_secrets_out_of_messages() {
        let login = login(exec(
            "kubectl",
            &[
                "oidc-login",
                "--oidc-client-secret=s3cr3t",
                "--password",
                "hunter22",
                "--oidc-client-id=kube",
            ],
            &[("API_KEY", "k3y-value"), ("REGION", "europe")],
        ));
        assert_eq!(
            login.redact("s3cr3t hunter22 k3y-value kube europe".into()),
            "*** *** *** kube europe"
        );
    }

    #[test]
    fn reads_the_credential() {
        let token = parse(
            br#"{"kind":"ExecCredential","status":{"token":"abc","expirationTimestamp":"2030-01-02T03:04:05Z"}}"#,
        )
        .unwrap();
        assert_eq!(token.header, "Bearer abc");
        assert!(token.header.is_sensitive());
        assert_eq!(
            token.expires,
            Some(SystemTime::from(
                "2030-01-02T03:04:05Z".parse::<jiff::Timestamp>().unwrap()
            ))
        );

        let token = parse(br#"{"status":{"token":"abc"}}"#).unwrap();
        assert_eq!(token.expires, None);

        let problem = |stdout: &[u8]| parse(stdout).err().unwrap();
        assert!(
            problem(br#"{"status":{"clientCertificateData":"x"}}"#).contains("client certificate")
        );
        assert_eq!(
            problem(br#"{"kind":"ExecCredential"}"#),
            "returned no credentials"
        );
        assert!(problem(b"not json").starts_with("printed no valid ExecCredential"));
    }

    #[test]
    fn logs_in_again_shortly_before_the_token_expires() {
        let login = login(exec("kubectl", &[], &[]));
        assert!(login.needed());
        let set = |expires| {
            *login.token.write().unwrap() = Some(Token {
                header: HeaderValue::from_static("Bearer abc"),
                expires,
            });
        };
        set(None);
        assert!(!login.needed());
        set(Some(SystemTime::now() + Duration::from_secs(3600)));
        assert!(!login.needed());
        set(Some(SystemTime::now() + Duration::from_secs(30)));
        assert!(login.needed());
        login.forget();
        assert!(!login.has_token());
    }

    #[tokio::test]
    async fn runs_the_plugin_without_a_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = fake::plugin(dir.path());
        let runs = dir.path().join("runs");
        let runs_env = runs.to_str().unwrap();
        let path = plugin.to_str().unwrap();

        let ok = login(exec(path, &["get-token"], &[("RUNS", runs_env)]));
        ok.run().await.unwrap();
        assert!(ok.has_token());
        let info: serde_json::Value = serde_json::from_str(&fake::runs(&runs)[0]).unwrap();
        assert_eq!(info["spec"]["interactive"], false);
        assert_eq!(info["apiVersion"], "client.authentication.k8s.io/v1");

        let failing = login(exec(
            path,
            &["get-token", "--client-secret=s3cr3t"],
            &[("RUNS", runs_env), ("MODE", "fail")],
        ));
        let message = failing.run().await.unwrap_err();
        assert_eq!(
            message,
            "plugin get-token failed (exit status: 1): error: login timed out, \
             args: get-token --client-secret=***"
        );
        assert!(!failing.has_token());

        let missing = login(exec("/does/not/exist", &[], &[]));
        assert_eq!(
            missing.run().await.unwrap_err(),
            "/does/not/exist not found"
        );
    }
}
