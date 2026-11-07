//! Runtime state of contexts and forwards.
//!
//! A forward's state is never stored or counted up: [`derive`] computes it from
//! what the engine observes right now.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Reachability of a kubeconfig context's API server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ContextState {
    /// No enabled forward uses the context, so nothing runs for it.
    Idle,
    Connecting,
    /// The context's exec plugin is logging in, maybe through the browser,
    /// and the API server does not accept the context's credentials until
    /// it is done.
    LoggingIn,
    Healthy {
        server_version: String,
    },
    Unreachable {
        since: SystemTime,
        cause: NetCause,
    },
    AuthFailed {
        since: SystemTime,
        message: String,
    },
    /// The context is missing from the kubeconfig or cannot be loaded.
    ConfigError {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum NetCause {
    Dns,
    Refused,
    Timeout,
    Tls(String),
    Other(String),
}

impl NetCause {
    pub fn short(&self) -> &'static str {
        match self {
            Self::Dns => "DNS",
            Self::Refused => "refused",
            Self::Timeout => "timeout",
            Self::Tls(_) => "TLS",
            Self::Other(_) => "error",
        }
    }

    /// The error text of a TLS or unclassified failure, which
    /// [`NetCause::short`] leaves out; the other causes say it all there.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Tls(text) | Self::Other(text) if !text.is_empty() => Some(text),
            _ => None,
        }
    }
}

impl ContextState {
    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy { .. })
    }

    /// Short text for the header line and `kprtfwd status`.
    pub fn summary(&self) -> String {
        match self {
            Self::Idle => "idle".into(),
            Self::Connecting => "connecting".into(),
            Self::LoggingIn => "logging in".into(),
            Self::Healthy { .. } => "ok".into(),
            Self::Unreachable { cause, .. } => format!("unreachable ({})", cause.short()),
            Self::AuthFailed { .. } => "login needed".into(),
            Self::ConfigError { .. } => "kubeconfig error".into(),
        }
    }

    /// What the summary leaves out: the error text of a TLS or unclassified
    /// failure, or why the login failed. For the log, the context's events
    /// and `kprtfwd status`; the header line and the INFO column keep to the
    /// summary.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Unreachable { cause, .. } => cause.detail(),
            Self::AuthFailed { message, .. } if !message.is_empty() => Some(message),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ForwardState {
    Disabled,
    Paused,
    /// Enabled, but the listener or the watch cache is not ready yet.
    Starting,
    PortBusy {
        holder: Option<String>,
    },
    /// The context is not healthy; its state says why.
    WaitingForContext,
    WaitingForPods {
        ready: u16,
        total: u16,
    },
    Misconfigured {
        problem: Misconfig,
    },
    Ready {
        pod: Option<String>,
        active_conns: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum Misconfig {
    ServiceNotFound,
    PortNotOnService,
    Forbidden(String),
}

impl ForwardState {
    /// The STATE column: `ready`, `waiting`, `error`, `paused` or `off`.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Disabled => "off",
            Self::Paused => "paused",
            Self::Ready { .. } => "ready",
            Self::Starting | Self::WaitingForContext | Self::WaitingForPods { .. } => "waiting",
            Self::PortBusy { .. } | Self::Misconfigured { .. } => "error",
        }
    }

    /// The INFO column: why the forward is not ready; empty if it is.
    /// `reconnect` says how the user reconnects the forward's context in the
    /// frontend that shows the text, such as `reconnect (r)` in the TUI.
    pub fn info(&self, context: &ContextState, reconnect: &str) -> String {
        match self {
            Self::Disabled | Self::Paused | Self::Ready { .. } => String::new(),
            Self::Starting => "starting".into(),
            Self::PortBusy {
                holder: Some(holder),
            } => format!("port busy: {holder}"),
            Self::PortBusy { holder: None } => "port busy".into(),
            Self::WaitingForContext => match context {
                ContextState::Unreachable { cause, .. } => {
                    format!("API unreachable ({})", cause.short())
                }
                ContextState::LoggingIn => "logging in".into(),
                ContextState::AuthFailed { .. } => format!("login needed: {reconnect} to log in"),
                ContextState::ConfigError { message } => format!("kubeconfig: {message}"),
                ContextState::Idle | ContextState::Connecting => "connecting".into(),
                ContextState::Healthy { .. } => String::new(),
            },
            Self::WaitingForPods { ready, total } => format!("{ready}/{total} pods ready"),
            Self::Misconfigured { problem } => match problem {
                Misconfig::ServiceNotFound => "service not found".into(),
                Misconfig::PortNotOnService => "port not on service".into(),
                Misconfig::Forbidden(message) => format!("forbidden: {message}"),
            },
        }
    }
}

/// Whether the forward could listen on its local port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bind {
    Pending,
    Bound,
    Busy { holder: Option<String> },
}

/// The forward's service and endpoints, as the watch cache sees them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The cache has not finished its first listing.
    Syncing,
    ServiceNotFound,
    PortNotOnService,
    Forbidden(String),
    Endpoints {
        ready: u16,
        total: u16,
    },
}

/// What the engine observes for an enabled forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub bind: Bind,
    pub context: ContextState,
    pub target: Target,
    /// The pod connections currently go to.
    pub pod: Option<String>,
    pub active_conns: u32,
}

/// The forward's state. `observed` is `None` while the engine runs nothing for
/// it yet.
pub fn derive(enabled: bool, paused: bool, observed: Option<&Observed>) -> ForwardState {
    if !enabled {
        return ForwardState::Disabled;
    }
    if paused {
        return ForwardState::Paused;
    }
    let Some(observed) = observed else {
        return ForwardState::Starting;
    };
    match &observed.bind {
        Bind::Pending => return ForwardState::Starting,
        Bind::Busy { holder } => {
            return ForwardState::PortBusy {
                holder: holder.clone(),
            };
        }
        Bind::Bound => {}
    }
    // Connecting and logging in too: the context's state says what the
    // forward waits for.
    if !observed.context.is_healthy() {
        return ForwardState::WaitingForContext;
    }
    match &observed.target {
        Target::Syncing => ForwardState::Starting,
        Target::ServiceNotFound => ForwardState::Misconfigured {
            problem: Misconfig::ServiceNotFound,
        },
        Target::PortNotOnService => ForwardState::Misconfigured {
            problem: Misconfig::PortNotOnService,
        },
        Target::Forbidden(message) => ForwardState::Misconfigured {
            problem: Misconfig::Forbidden(message.clone()),
        },
        Target::Endpoints { ready: 0, total } => ForwardState::WaitingForPods {
            ready: 0,
            total: *total,
        },
        Target::Endpoints { .. } => ForwardState::Ready {
            pod: observed.pod.clone(),
            active_conns: observed.active_conns,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> ContextState {
        ContextState::Healthy {
            server_version: "v1.33.0".into(),
        }
    }

    fn observed(bind: Bind, context: ContextState, target: Target) -> Observed {
        Observed {
            bind,
            context,
            target,
            pod: Some("web-1".into()),
            active_conns: 2,
        }
    }

    #[test]
    fn derives_state_from_observations() {
        let unreachable = ContextState::Unreachable {
            since: SystemTime::UNIX_EPOCH,
            cause: NetCause::Dns,
        };
        let pods = Target::Endpoints { ready: 1, total: 2 };
        let cases = [
            // Desired state comes first.
            (false, false, None, ForwardState::Disabled),
            (true, true, None, ForwardState::Paused),
            (true, false, None, ForwardState::Starting),
            // Then the local port, the context, the service and its pods.
            (
                true,
                false,
                Some(observed(
                    Bind::Busy { holder: None },
                    unreachable.clone(),
                    pods.clone(),
                )),
                ForwardState::PortBusy { holder: None },
            ),
            (
                true,
                false,
                Some(observed(Bind::Pending, healthy(), pods.clone())),
                ForwardState::Starting,
            ),
            (
                true,
                false,
                Some(observed(Bind::Bound, unreachable.clone(), pods.clone())),
                ForwardState::WaitingForContext,
            ),
            (
                true,
                false,
                Some(observed(
                    Bind::Bound,
                    ContextState::Connecting,
                    pods.clone(),
                )),
                ForwardState::WaitingForContext,
            ),
            (
                true,
                false,
                Some(observed(Bind::Bound, ContextState::LoggingIn, pods.clone())),
                ForwardState::WaitingForContext,
            ),
            (
                true,
                false,
                Some(observed(Bind::Bound, healthy(), Target::Syncing)),
                ForwardState::Starting,
            ),
            (
                true,
                false,
                Some(observed(Bind::Bound, healthy(), Target::ServiceNotFound)),
                ForwardState::Misconfigured {
                    problem: Misconfig::ServiceNotFound,
                },
            ),
            (
                true,
                false,
                Some(observed(
                    Bind::Bound,
                    healthy(),
                    Target::Endpoints { ready: 0, total: 2 },
                )),
                ForwardState::WaitingForPods { ready: 0, total: 2 },
            ),
            (
                true,
                false,
                Some(observed(Bind::Bound, healthy(), pods)),
                ForwardState::Ready {
                    pod: Some("web-1".into()),
                    active_conns: 2,
                },
            ),
        ];
        for (enabled, paused, observed, want) in cases {
            assert_eq!(
                derive(enabled, paused, observed.as_ref()),
                want,
                "{observed:?}"
            );
        }
    }

    #[test]
    fn info_explains_why() {
        let unreachable = ContextState::Unreachable {
            since: SystemTime::UNIX_EPOCH,
            cause: NetCause::Dns,
        };
        assert_eq!(
            ForwardState::WaitingForContext.info(&unreachable, "reconnect (r)"),
            "API unreachable (DNS)"
        );
        let auth = ContextState::AuthFailed {
            since: SystemTime::UNIX_EPOCH,
            message: "exec plugin failed".into(),
        };
        assert_eq!(
            ForwardState::WaitingForContext.info(&auth, "reconnect (r)"),
            "login needed: reconnect (r) to log in"
        );
        assert_eq!(
            ForwardState::WaitingForContext.info(&auth, "kprtfwd reconnect staging"),
            "login needed: kprtfwd reconnect staging to log in"
        );
        assert_eq!(
            ForwardState::WaitingForContext.info(&ContextState::Connecting, ""),
            "connecting"
        );
        assert_eq!(
            ForwardState::WaitingForContext.info(&ContextState::LoggingIn, ""),
            "logging in"
        );
        assert_eq!(ContextState::LoggingIn.summary(), "logging in");
        assert_eq!(
            ForwardState::WaitingForPods { ready: 0, total: 2 }.info(&healthy(), ""),
            "0/2 pods ready"
        );
        assert_eq!(
            ForwardState::PortBusy {
                holder: Some("mysqld (pid 812)".into())
            }
            .info(&healthy(), ""),
            "port busy: mysqld (pid 812)"
        );
        assert_eq!(unreachable.summary(), "unreachable (DNS)");
    }

    #[test]
    fn detail_keeps_what_the_summary_leaves_out() {
        let down = |cause| ContextState::Unreachable {
            since: SystemTime::UNIX_EPOCH,
            cause,
        };
        let expired = "invalid peer certificate: Expired";
        let tls = down(NetCause::Tls(expired.into()));
        assert_eq!(tls.summary(), "unreachable (TLS)");
        assert_eq!(tls.detail(), Some(expired));
        let reset = down(NetCause::Other("connection reset".into()));
        assert_eq!(reset.detail(), Some("connection reset"));
        let auth = ContextState::AuthFailed {
            since: SystemTime::UNIX_EPOCH,
            message: "login timed out".into(),
        };
        assert_eq!(auth.detail(), Some("login timed out"));
        for state in [
            down(NetCause::Dns),
            down(NetCause::Other(String::new())),
            ContextState::Connecting,
            healthy(),
        ] {
            assert_eq!(state.detail(), None, "{state:?}");
        }
    }

    #[test]
    fn serializes_with_readable_tags() {
        let json =
            serde_json::to_string(&ForwardState::WaitingForPods { ready: 0, total: 2 }).unwrap();
        assert_eq!(json, r#"{"state":"waiting_for_pods","ready":0,"total":2}"#);
        let json = serde_json::to_string(&ForwardState::Misconfigured {
            problem: Misconfig::Forbidden("no".into()),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"state":"misconfigured","problem":{"kind":"forbidden","detail":"no"}}"#
        );
    }
}
