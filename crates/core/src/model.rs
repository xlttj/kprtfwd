use std::fmt;

use serde::{Deserialize, Serialize};

/// Identifier of a forward.
///
/// IDs are opaque: v1 created them in several shapes, and existing ones are
/// kept as they are.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ForwardId(String);

impl ForwardId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ForwardId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ForwardId {
    fn from(id: &str) -> Self {
        Self::new(id)
    }
}

impl From<String> for ForwardId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// A configured port-forward from a local port to a service port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forward {
    pub id: ForwardId,
    /// Kubeconfig context name.
    pub context: String,
    pub namespace: String,
    pub service: String,
    /// Service port, as listed in the Service spec.
    pub remote_port: u16,
    /// Port on localhost.
    pub local_port: u16,
    /// Whether the user wants this forward running.
    pub enabled: bool,
}

/// A named set of forwards that can be switched on and off together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub forwards: Vec<ForwardId>,
}
