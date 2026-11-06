//! Messages between the kprtfwd daemon and its clients.
//!
//! Newline-delimited JSON over a Unix socket. A client starts with
//! [`Request::Hello`]; the daemon answers with [`Response::Welcome`]. After
//! that every request carries an `id`, and its response echoes it.

mod client;

use std::io;
use std::time::SystemTime;

use kprtfwd_core::svctype::ServiceType;
use kprtfwd_core::{ContextState, Forward, ForwardId, ForwardState, Project};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub use client::{Client, ClientError, connect_or_start, spawn_daemon, stop_daemon};

/// Bumped on every incompatible change to the messages.
pub const PROTOCOL_VERSION: u32 = 5;

/// A request with its ID. The request is nested, not flattened, because
/// requests have fields of their own named `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientMessage {
    pub id: u64,
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello {
        protocol: u32,
        client_version: String,
    },
    /// Sent by the TUI. When the last such connection closes, the daemon
    /// switches all forwards off and exits, unless `config.toml` says to keep
    /// them running.
    Attach,
    Snapshot,
    SetEnabled {
        ids: Vec<ForwardId>,
        enabled: bool,
    },
    SetProjectEnabled {
        name: String,
        enabled: bool,
    },
    /// Enables the project's forwards and disables all others.
    SwitchProject {
        name: String,
    },
    SetPaused {
        paused: bool,
    },
    ReconnectContext {
        name: String,
    },
    /// Adds forwards or replaces those with the same ID.
    UpsertForwards {
        forwards: Vec<Forward>,
    },
    DeleteForward {
        id: ForwardId,
    },
    /// Creates a project or replaces its members.
    UpsertProject {
        project: Project,
    },
    DeleteProject {
        name: String,
    },
    /// The contexts in the kubeconfig.
    ListContexts,
    /// Services in a context whose namespace matches `namespace_filter`
    /// (`*` wildcards; empty for all).
    Discover {
        context: String,
        namespace_filter: String,
    },
    /// Lists the forwards in `context` whose service no longer exists, or
    /// deletes some of them. With `delete`, only the forwards of these IDs
    /// that are still gone are deleted: a client lists first and sends the
    /// IDs the user confirmed.
    Prune {
        context: String,
        namespace_filter: String,
        delete: Option<Vec<ForwardId>>,
    },
    /// Recent events of these forwards and of their contexts.
    Events {
        ids: Vec<ForwardId>,
    },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Response { id: u64, response: Response },
    Error { id: u64, message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Welcome {
        protocol: u32,
        daemon_version: String,
        pid: u32,
    },
    Snapshot(Snapshot),
    Done,
    Contexts {
        names: Vec<String>,
        current: Option<String>,
    },
    Discovered {
        ports: Vec<DiscoveredPort>,
    },
    /// The forwards whose service is gone, or with `delete` those deleted.
    Pruned {
        forwards: Vec<Forward>,
    },
    Events {
        entries: Vec<EventEntry>,
    },
}

/// One service port found by discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredPort {
    pub namespace: String,
    pub service: String,
    pub port: u16,
    pub port_name: Option<String>,
    pub service_type: ServiceType,
    /// The forward that already exists for this port.
    pub existing: Option<ForwardId>,
    /// The ID a new forward gets, or the existing forward's.
    pub id: ForwardId,
    /// The proposed local port, or the existing forward's.
    pub local_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEntry {
    pub at: SystemTime,
    /// The forward's ID or the context's name.
    pub source: String,
    pub message: String,
}

/// Everything a client shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub daemon_version: String,
    pub paused: bool,
    /// Every context that a forward uses, sorted by name.
    pub contexts: Vec<ContextView>,
    /// All forwards, sorted by context, namespace, service and port.
    pub forwards: Vec<ForwardView>,
    pub projects: Vec<ProjectView>,
    /// The kubeconfig files the daemon reads; one that does not exist ends
    /// with ` (missing)`.
    pub kubeconfig_files: Vec<String>,
}

impl Snapshot {
    pub fn context_state(&self, name: &str) -> ContextState {
        self.contexts
            .iter()
            .find(|c| c.name == name)
            .map_or(ContextState::Idle, |c| c.state.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextView {
    pub name: String,
    pub state: ContextState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForwardView {
    pub forward: Forward,
    pub state: ForwardState,
    pub last_error: Option<LastError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastError {
    pub at: SystemTime,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectView {
    pub name: String,
    pub forwards: Vec<ForwardId>,
    /// How many of the project's forwards are enabled.
    pub enabled: usize,
}

/// Writes one message as a JSON line.
pub async fn write_message<W, T>(writer: &mut W, message: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

/// Reads one JSON line; `None` when the other side closed the connection.
pub async fn read_message<R, T>(reader: &mut R) -> io::Result<Option<T>>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    serde_json::from_str(&line)
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_survive_the_wire() {
        let message = ClientMessage {
            id: 7,
            request: Request::SetEnabled {
                ids: vec![ForwardId::new("a")],
                enabled: true,
            },
        };
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(
            json,
            r#"{"id":7,"request":{"type":"set_enabled","ids":["a"],"enabled":true}}"#
        );

        // Every kind of request, including those with an `id` field of their own.
        let id = || ForwardId::new("staging.shop.api-80");
        let forward = Forward {
            id: id(),
            context: "staging".into(),
            namespace: "shop".into(),
            service: "api".into(),
            remote_port: 80,
            local_port: 18080,
            enabled: false,
        };
        let requests = [
            Request::Hello {
                protocol: PROTOCOL_VERSION,
                client_version: "test".into(),
            },
            Request::Attach,
            Request::Snapshot,
            Request::SetProjectEnabled {
                name: "shop".into(),
                enabled: true,
            },
            Request::SwitchProject {
                name: "shop".into(),
            },
            Request::SetPaused { paused: true },
            Request::ReconnectContext {
                name: "staging".into(),
            },
            Request::UpsertForwards {
                forwards: vec![forward],
            },
            Request::DeleteForward { id: id() },
            Request::UpsertProject {
                project: Project {
                    name: "shop".into(),
                    forwards: vec![id()],
                },
            },
            Request::DeleteProject {
                name: "shop".into(),
            },
            Request::ListContexts,
            Request::Discover {
                context: "staging".into(),
                namespace_filter: "shop-*".into(),
            },
            Request::Prune {
                context: "staging".into(),
                namespace_filter: "*".into(),
                delete: None,
            },
            Request::Prune {
                context: "staging".into(),
                namespace_filter: "*".into(),
                delete: Some(vec![id()]),
            },
            Request::Events { ids: vec![id()] },
            Request::Shutdown,
        ];
        for (n, request) in requests.into_iter().enumerate() {
            let message = ClientMessage {
                id: n as u64,
                request,
            };
            let json = serde_json::to_string(&message).unwrap();
            assert_eq!(
                serde_json::from_str::<ClientMessage>(&json).unwrap(),
                message,
                "{json}"
            );
        }
    }

    #[tokio::test]
    async fn round_trips_over_a_stream() {
        let (client, server) = tokio::io::duplex(1024);
        let (_, mut client_write) = tokio::io::split(client);
        let (server_read, _) = tokio::io::split(server);
        let mut server_read = tokio::io::BufReader::new(server_read);

        let response = ServerMessage::Response {
            id: 1,
            response: Response::Done,
        };
        write_message(&mut client_write, &response).await.unwrap();
        write_message(&mut client_write, &response).await.unwrap();
        drop(client_write);

        for _ in 0..2 {
            let read: Option<ServerMessage> = read_message(&mut server_read).await.unwrap();
            assert_eq!(read, Some(response.clone()));
        }
        let end: Option<ServerMessage> = read_message(&mut server_read).await.unwrap();
        assert_eq!(end, None);
    }
}
