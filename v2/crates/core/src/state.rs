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
            Self::Healthy { .. } => "ok".into(),
            Self::Unreachable { cause, .. } => format!("unreachable ({})", cause.short()),
            Self::AuthFailed { .. } => "login needed".into(),
            Self::ConfigError { .. } => "kubeconfig error".into(),
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
    pub fn info(&self, context_name: &str, context: &ContextState) -> String {
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
                ContextState::AuthFailed { .. } => {
                    format!("login needed: kubectl --context {context_name} get ns")
                }
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
    match &observed.context {
        ContextState::Healthy { .. } => {}
        ContextState::Idle | ContextState::Connecting => return ForwardState::Starting,
        _ => return ForwardState::WaitingForContext,
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
                ForwardState::Starting,
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
            ForwardState::WaitingForContext.info("staging", &unreachable),
            "API unreachable (DNS)"
        );
        let auth = ContextState::AuthFailed {
            since: SystemTime::UNIX_EPOCH,
            message: "exec plugin failed".into(),
        };
        assert_eq!(
            ForwardState::WaitingForContext.info("staging", &auth),
            "login needed: kubectl --context staging get ns"
        );
        assert_eq!(
            ForwardState::WaitingForPods { ready: 0, total: 2 }.info("staging", &healthy()),
            "0/2 pods ready"
        );
        assert_eq!(
            ForwardState::PortBusy {
                holder: Some("mysqld (pid 812)".into())
            }
            .info("staging", &healthy()),
            "port busy: mysqld (pid 812)"
        );
        assert_eq!(unreachable.summary(), "unreachable (DNS)");
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
