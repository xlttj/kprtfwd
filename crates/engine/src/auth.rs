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
//! the token of the last successful run, or no credentials at all. Plugins
//! run one at a time across all contexts; see [`turn`].

use std::path::Path;
use std::process::{Output, Stdio};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use hyper::Request;
use hyper::header::{AUTHORIZATION, HeaderValue};
use kube::client::Body;
use kube::config::{AuthInfo, ExecAuthCluster, ExecConfig};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, ChildStdout};
use tokio::sync::{Mutex, MutexGuard};
use tower::util::MapRequestLayer;

/// Log in again this long before the token expires, as kube-rs does.
const REFRESH_BEFORE: Duration = Duration::from_secs(60);
/// The longest a login may take. A browser login waits for the user;
/// kubelogin gives up after 3 minutes.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// How long the plugin's output is read after it has exited. It is in the
/// pipes by then; only a process the plugin started can keep them open.
const AFTER_EXIT: Duration = Duration::from_millis(500);
/// Values shorter than this stay in messages: replacing them would garble
/// the text, and a secret is longer.
const MIN_SECRET: usize = 4;
/// Runs of token characters at least this long, with digits and both cases,
/// count as tokens.
const MIN_TOKEN: usize = 32;

/// Held by the plugin that runs; see [`turn`].
static TURN: Mutex<()> = Mutex::const_new(());

/// The right to run a plugin.
pub struct Turn {
    _running: MutexGuard<'static, ()>,
    /// Whether another plugin ran meanwhile.
    pub waited: bool,
}

/// Waits until no other plugin runs. Plugins run one at a time across all
/// contexts, discovery and prune included: a browser login then opens one
/// tab at a time, and the next plugin may find the first one's tokens in
/// its cache.
pub async fn turn() -> Turn {
    match TURN.try_lock() {
        Ok(running) => Turn {
            _running: running,
            waited: false,
        },
        Err(_) => Turn {
            _running: TURN.lock().await,
            waited: true,
        },
    }
}

/// A context's exec plugin and the token it returned last.
#[derive(Clone)]
pub struct Login {
    exec: Arc<ExecConfig>,
    token: Arc<RwLock<Option<Token>>>,
}

struct Token {
    header: HeaderValue,
    /// When to log in again; see [`renewal`].
    renew: Option<SystemTime>,
}

impl Login {
    /// Takes the exec plugin out of `auth`, so that kube-rs never runs it, and
    /// returns it if that is how the context logs in. kube-rs prefers, in
    /// this order, an auth provider, a username together with a password, a
    /// token and a token file over the plugin; with one of those the plugin
    /// is never used.
    pub fn take(auth: &mut AuthInfo) -> Option<Self> {
        let exec = auth.exec.take()?;
        let other_credentials = auth.auth_provider.is_some()
            || (auth.username.is_some() && auth.password.is_some())
            || auth.token.is_some()
            || auth.token_file.is_some();
        if other_credentials {
            return None;
        }
        Some(Self {
            exec: Arc::new(exec),
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

    /// Whether there is no token, or it is time to renew it.
    pub fn needed(&self) -> bool {
        match &*self.token.read().unwrap() {
            None => true,
            Some(token) => token.renew.is_some_and(|at| SystemTime::now() >= at),
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

    /// Runs the plugin in its `turn` and keeps the token it returns. The run
    /// ends when the plugin exits, even if a process it started keeps its
    /// output open. Dropping the future ends the plugin. The error never
    /// contains the plugin's arguments.
    pub async fn run(&self, _turn: &Turn) -> Result<(), String> {
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
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let hint = self
                    .exec
                    .install_hint
                    .as_deref()
                    .and_then(|hint| hint.lines().map(str::trim).find(|line| !line.is_empty()))
                    .map(|hint| format!(" ({hint})"))
                    .unwrap_or_default();
                return Err(format!("{command} not found{hint}"));
            }
            Err(err) => return Err(format!("cannot run {name}: {err}")),
        };
        let mut pipes = Pipes::of(&mut child);
        let output = match tokio::time::timeout(LOGIN_TIMEOUT, pipes.output(&mut child)).await {
            Ok(Ok(output)) => output,
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
                message = format!("{message}: {}", self.redact(&line));
            }
            return Err(message);
        }
        let token = parse(&output.stdout).map_err(|problem| format!("{name} {problem}"))?;
        *self.token.write().unwrap() = Some(token);
        Ok(())
    }

    /// Masks in the plugin's own text every value it was given and every
    /// word that looks like a token. Which values are secret cannot be told
    /// from their names, so none is kept.
    fn redact(&self, text: &str) -> String {
        let text = secrets(&self.exec)
            .iter()
            .fold(text.to_owned(), |text, secret| replace_word(&text, secret));
        mask_tokens(&text)
    }
}

/// The plugin's stdout and stderr.
///
/// A process the plugin starts, such as a browser opened for the login, may
/// inherit them and hold them open long after the plugin has exited. So the
/// login ends when the plugin exits, not when the pipes close, and once
/// dropped they are emptied in the background until every holder has closed
/// them: closing them instead would make that process's writes fail, and
/// leaving them unread could block it.
struct Pipes {
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
}

impl Pipes {
    fn of(child: &mut Child) -> Self {
        Self {
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
        }
    }

    /// Waits for the plugin to exit and returns what it wrote. Reads both
    /// pipes meanwhile, so that it never blocks on a full one, and for at
    /// most [`AFTER_EXIT`] once it has exited.
    async fn output(&mut self, child: &mut Child) -> std::io::Result<Output> {
        let (Some(stdout), Some(stderr)) = (&mut self.stdout, &mut self.stderr) else {
            return Err(std::io::Error::other("the plugin's output is not piped"));
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let status = {
            let reading = async {
                let _ = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
            };
            tokio::pin!(reading);
            tokio::select! {
                status = child.wait() => {
                    // What the plugin wrote is in the pipes by now.
                    let _ = tokio::time::timeout(AFTER_EXIT, &mut reading).await;
                    status?
                }
                () = &mut reading => child.wait().await?,
            }
        };
        Ok(Output {
            status,
            stdout: out,
            stderr: err,
        })
    }
}

impl Drop for Pipes {
    fn drop(&mut self) {
        let (Some(mut stdout), Some(mut stderr)) = (self.stdout.take(), self.stderr.take()) else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let (mut out, mut err) = (tokio::io::sink(), tokio::io::sink());
                let _ = tokio::join!(
                    tokio::io::copy(&mut stdout, &mut out),
                    tokio::io::copy(&mut stderr, &mut err),
                );
            });
        }
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

/// Every value the plugin was given: its arguments except the first one when
/// that names the plugin (see [`Login::name`]), the words inside combined
/// arguments such as `sh -c "<plugin> --secret=…"`, and its variables'
/// values. Flag names stay. Each value also in percent-encoded form, as
/// plugins put values into URLs. Longest first, so that a value inside
/// another one does not break it up.
fn secrets(exec: &ExecConfig) -> Vec<String> {
    let mut found = Vec::new();
    let args = exec.args.as_deref().unwrap_or_default();
    let shown = args.first().filter(|first| !first.starts_with('-'));
    for arg in args.iter().skip(usize::from(shown.is_some())) {
        let words: Vec<&str> = arg.split_whitespace().collect();
        if words.len() > 1 {
            found.push(arg.clone());
        }
        for word in words {
            let word = word.trim_matches(['\'', '"']);
            match word.split_once('=') {
                Some((flag, value)) if flag.starts_with('-') => {
                    found.push(value.trim_matches(['\'', '"']).to_owned());
                }
                // A flag's name; its value is the next word or argument.
                _ if word.starts_with('-') => {}
                _ => found.push(word.to_owned()),
            }
        }
    }
    found.extend(variables(exec).map(|(_, value)| value.clone()));
    let encoded: Vec<String> = found
        .iter()
        .map(|value| percent_encoded(value))
        .filter(|encoded| !found.contains(encoded))
        .collect();
    found.extend(encoded);
    found.retain(|value| value.len() >= MIN_SECRET);
    found.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    found.dedup();
    found
}

/// `value` as a URL query would carry it.
fn percent_encoded(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Characters that continue a word, so that a value is not masked inside a
/// longer word: the client ID "kube" stays in "kubectl".
fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// Replaces `value` with `***` wherever it stands as a word of its own.
fn replace_word(text: &str, value: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(value) {
        out.push_str(&rest[..at]);
        let before = out.chars().next_back();
        let after = rest[at + value.len()..].chars().next();
        if before.is_some_and(word_char) || after.is_some_and(word_char) {
            out.push_str(value);
        } else {
            out.push_str("***");
        }
        rest = &rest[at + value.len()..];
    }
    out.push_str(rest);
    out
}

/// Masks words that look like credentials: the word after "Bearer", and
/// runs of base64 characters of [`MIN_TOKEN`] or more that mix digits and
/// both cases, which covers access tokens and each part of a JWT.
fn mask_tokens(text: &str) -> String {
    let token_char = |c: char| c.is_ascii_alphanumeric() || "-_+/=".contains(c);
    let looks_like_token = |run: &str| {
        run.len() >= MIN_TOKEN
            && run.chars().any(|c| c.is_ascii_digit())
            && run.chars().any(|c| c.is_ascii_lowercase())
            && run.chars().any(|c| c.is_ascii_uppercase())
    };
    let mut out = String::with_capacity(text.len());
    let mut after_bearer = false;
    let mut rest = text;
    while let Some(start) = rest.find(token_char) {
        out.push_str(&rest[..start]);
        let run = &rest[start..];
        let end = run.find(|c| !token_char(c)).unwrap_or(run.len());
        let word = &run[..end];
        if after_bearer || looks_like_token(word) {
            out.push_str("***");
        } else {
            out.push_str(word);
        }
        after_bearer = word.eq_ignore_ascii_case("bearer") && run[end..].starts_with(' ');
        rest = &run[end..];
    }
    out.push_str(rest);
    out
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
    Ok(Token {
        header,
        renew: renewal(expires, SystemTime::now()),
    })
}

/// When to log in again for a token that expires at `expires`, returned at
/// `now`: [`REFRESH_BEFORE`] ahead. A token that already expires within that
/// time, such as one a plugin hands out from its cache until it really
/// expires, is renewed only once it has expired, else the plugin would run on
/// every probe; one that has already expired by this machine's clock only
/// when the API server rejects it.
fn renewal(expires: Option<SystemTime>, now: SystemTime) -> Option<SystemTime> {
    let expires = expires?;
    match expires.checked_sub(REFRESH_BEFORE) {
        Some(early) if early > now => Some(early),
        _ if expires > now => Some(expires),
        _ => None,
    }
}

/// Fake exec plugins for tests.
#[cfg(test)]
pub(crate) mod fake {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// A plugin that appends its `KUBERNETES_EXEC_INFO` to `$RUNS` and then,
    /// by `$MODE`: prints the token "good", which expires at `$EXPIRES` or
    /// in the year 2999 (`ok`), the token "bad" (`bad`),
    /// fails with its arguments on stderr (`fail`), or leaves a process
    /// behind that holds its stdout and stderr, as a browser it opened
    /// would, writes that process's ID to `$LINGER` and prints the token
    /// "good" (`linger`), or waits until the file `$GATE` exists and then
    /// prints the token "good" (`wait`).
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
  wait)
    while [ ! -e "$GATE" ]; do sleep 0.02; done
    echo '{"apiVersion":"client.authentication.k8s.io/v1","kind":"ExecCredential","status":{"token":"good"}}' ;;
  linger)
    sleep 30 &
    echo $! > "$LINGER"
    echo '{"apiVersion":"client.authentication.k8s.io/v1","kind":"ExecCredential","status":{"token":"good"}}' ;;
  *)
    echo '{"apiVersion":"client.authentication.k8s.io/v1","kind":"ExecCredential","status":{"token":"good","expirationTimestamp":"'"${EXPIRES:-2999-01-01T00:00:00Z}"'"}}' ;;
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
        let with_exec = |auth: AuthInfo| AuthInfo {
            exec: Some(exec("kubectl", &[], &[])),
            ..auth
        };
        // kube-rs uses a username only together with a password, so with a
        // username alone it would run the plugin itself.
        for auth in [
            AuthInfo::default(),
            AuthInfo {
                username: Some("admin".into()),
                ..Default::default()
            },
        ] {
            let mut auth = with_exec(auth);
            assert!(Login::take(&mut auth).is_some(), "{auth:?}");
            assert!(auth.exec.is_none());
        }

        // Other credentials win; the plugin is still taken out, so that
        // kube-rs never runs it.
        for auth in [
            AuthInfo {
                username: Some("admin".into()),
                password: Some("secret".to_owned().into()),
                ..Default::default()
            },
            AuthInfo {
                token: Some("abc".to_owned().into()),
                ..Default::default()
            },
            AuthInfo {
                token_file: Some("/token".into()),
                ..Default::default()
            },
            AuthInfo {
                auth_provider: Some(kube::config::AuthProviderConfig {
                    name: "oidc".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ] {
            let mut auth = with_exec(auth);
            assert!(Login::take(&mut auth).is_none(), "{auth:?}");
            assert!(auth.exec.is_none());
        }

        assert!(Login::take(&mut AuthInfo::default()).is_none());
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
    fn keeps_every_value_out_of_messages() {
        let login = login(exec(
            "kubectl",
            &[
                "oidc-login",
                "get-token",
                "--oidc-client-secret=s3cr3t",
                "--passwd",
                "hunter22",
                "--oidc-client-id=kube",
                "--assertion='quoted-value'",
            ],
            &[("API_KEY", "k3y-value"), ("PROFILE", "staging-admin")],
        ));
        assert_eq!(
            login.redact(
                "oidc-login get-token s3cr3t hunter22 kube quoted-value k3y-value staging-admin --passwd"
            ),
            "oidc-login *** *** *** *** *** *** *** --passwd"
        );
        // A value is masked as a word of its own, not inside other words.
        assert_eq!(
            login.redact("kubectl kube, kube-system kube"),
            "kubectl ***, kube-system ***"
        );
    }

    #[test]
    fn keeps_values_of_combined_arguments_out_of_messages() {
        let login = login(exec(
            "sh",
            &["-c", "kubelogin get-token --client-secret=Sup3rS3cret"],
            &[],
        ));
        assert_eq!(
            login.redact("error: client Sup3rS3cret rejected"),
            "error: client *** rejected"
        );
    }

    #[test]
    fn keeps_encoded_values_out_of_messages() {
        let login = login(exec(
            "plugin",
            &["get-token", "--client-secret=a+b/c=d&e"],
            &[],
        ));
        assert_eq!(
            login.redact("POST /token?client_secret=a%2Bb%2Fc%3Dd%26e&grant_type=x failed"),
            "POST /token?client_secret=***&grant_type=x failed"
        );
    }

    #[test]
    fn keeps_tokens_out_of_messages() {
        let login = login(exec("plugin", &["get-token"], &[]));
        assert_eq!(
            login.redact(
                "got eyJhbGciOiJSUzI1NiIsImtpZCI6IjEifQ.eyJzdWIiOiJ1c2VyQGV4YW1wbGUuY29tIiwiZXhwIjoxfQ.c2lnbmF0dXJlLXNpZ25hdHVyZS1zaWduYXR1cmU, Authorization: Bearer abc123"
            ),
            "got ***.***.***, Authorization: Bearer ***"
        );
        // Paths, hosts and hashes are not tokens.
        let plain = "/usr/local/bin/kubectl-oidc_login https://login.example.com \
                     3f9a1c0b7e2d4a6f8b9c0d1e2f3a4b5c6d7e8f90";
        assert_eq!(login.redact(plain), plain);
    }

    #[test]
    fn reads_the_credential() {
        let token = parse(
            br#"{"kind":"ExecCredential","status":{"token":"abc","expirationTimestamp":"2030-01-02T03:04:05Z"}}"#,
        )
        .unwrap();
        assert_eq!(token.header, "Bearer abc");
        assert!(token.header.is_sensitive());
        // A minute before it expires.
        assert_eq!(
            token.renew,
            Some(SystemTime::from(
                "2030-01-02T03:03:05Z".parse::<jiff::Timestamp>().unwrap()
            ))
        );

        let token = parse(br#"{"status":{"token":"abc"}}"#).unwrap();
        assert_eq!(token.renew, None);

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
    fn logs_in_again_when_the_token_is_due() {
        let login = login(exec("kubectl", &[], &[]));
        assert!(login.needed());
        let set = |renew| {
            *login.token.write().unwrap() = Some(Token {
                header: HeaderValue::from_static("Bearer abc"),
                renew,
            });
        };
        set(None);
        assert!(!login.needed());
        set(Some(SystemTime::now() + Duration::from_secs(3600)));
        assert!(!login.needed());
        set(Some(SystemTime::now() - Duration::from_secs(1)));
        assert!(login.needed());
        login.forget();
        assert!(!login.has_token());
    }

    #[test]
    fn renews_a_token_a_minute_early_unless_it_came_with_less() {
        let at = |seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        let now = at(1000);
        assert_eq!(renewal(None, now), None);
        assert_eq!(renewal(Some(at(4600)), now), Some(at(4540)));
        assert_eq!(renewal(Some(at(1061)), now), Some(at(1001)));
        // Within the minute, as a plugin's cached token in its last minute:
        // only once it has expired.
        assert_eq!(renewal(Some(at(1060)), now), Some(at(1060)));
        assert_eq!(renewal(Some(at(1030)), now), Some(at(1030)));
        // Expired by this machine's clock: only when the API server rejects
        // it.
        assert_eq!(renewal(Some(at(1000)), now), None);
        assert_eq!(renewal(Some(at(900)), now), None);
    }

    #[tokio::test]
    async fn runs_the_plugin_without_a_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = fake::plugin(dir.path());
        let runs = dir.path().join("runs");
        let runs_env = runs.to_str().unwrap();
        let path = plugin.to_str().unwrap();

        let ok = login(exec(path, &["get-token"], &[("RUNS", runs_env)]));
        ok.run(&turn().await).await.unwrap();
        assert!(ok.has_token());
        let info: serde_json::Value = serde_json::from_str(&fake::runs(&runs)[0]).unwrap();
        assert_eq!(info["spec"]["interactive"], false);
        assert_eq!(info["apiVersion"], "client.authentication.k8s.io/v1");

        let failing = login(exec(
            path,
            &["get-token", "--client-secret=s3cr3t"],
            &[("RUNS", runs_env), ("MODE", "fail")],
        ));
        let message = failing.run(&turn().await).await.unwrap_err();
        assert_eq!(
            message,
            "plugin get-token failed (exit status: 1): error: login timed out, \
             args: get-token --client-secret=***"
        );
        assert!(!failing.has_token());

        let missing = login(exec("/does/not/exist", &[], &[]));
        assert_eq!(
            missing.run(&turn().await).await.unwrap_err(),
            "/does/not/exist not found"
        );
    }

    /// A browser the plugin opens may inherit its stdout and stderr and keep
    /// them open; the login still ends when the plugin exits.
    #[tokio::test]
    async fn ends_when_the_plugin_exits() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = fake::plugin(dir.path());
        let runs = dir.path().join("runs");
        let lingering = dir.path().join("lingering");
        let login = login(exec(
            plugin.to_str().unwrap(),
            &["get-token"],
            &[
                ("RUNS", runs.to_str().unwrap()),
                ("MODE", "linger"),
                ("LINGER", lingering.to_str().unwrap()),
            ],
        ));
        let started = std::time::Instant::now();
        let result = login.run(&turn().await).await;
        let took = started.elapsed();
        let pid = std::fs::read_to_string(&lingering).unwrap();
        std::process::Command::new("kill")
            .arg(pid.trim())
            .status()
            .unwrap();
        result.unwrap();
        assert!(login.has_token());
        assert!(took < Duration::from_secs(5), "took {took:?}");
    }
}
