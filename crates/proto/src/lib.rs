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
pub const PROTOCOL_VERSION: u32 = 7;

/// This kprtfwd's version, which a client sends in its hello.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A hint when the daemon runs another version than this kprtfwd. Such a
/// daemon speaks the same protocol and is served, but keeps running the
/// code of its own version until it is restarted.
pub fn other_version(daemon_version: &str) -> Option<String> {
    (daemon_version != VERSION).then(|| {
        format!(
            "the daemon runs kprtfwd {daemon_version}, this is {VERSION}; restart it with `kprtfwd daemon restart`"
        )
    })
}

/// The longest message, without its newline. A snapshot of thousands of
/// forwards takes a few MB; the limit keeps a writer that sends no newline
/// from filling the reader's memory.
pub const MAX_FRAME: usize = 16 << 20;

/// A request with its ID. The request is nested, not flattened, because
/// requests have fields of their own named `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientMessage {
    pub id: u64,
    pub request: Request,
}

impl ClientMessage {
    /// Parses a line of a client, without its newline. For a line that is no
    /// request, returns what to answer with in a [`ServerMessage::Error`]: the
    /// line's `id` if it has one, else 0, and why.
    pub fn parse(line: &[u8]) -> Result<Self, (u64, String)> {
        /// What may be readable of a line that is no request.
        #[derive(Deserialize)]
        struct WithId {
            id: u64,
        }
        serde_json::from_slice(line).map_err(|err| {
            let id = serde_json::from_slice::<WithId>(line).map_or(0, |line| line.id);
            (id, format!("cannot read the request: {err}"))
        })
    }
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
        /// Namespaces that match the filter but whose services the user may
        /// not list.
        skipped: Vec<String>,
    },
    /// The forwards whose service is gone, or with `delete` those deleted.
    /// Forwards in `skipped` namespaces are never among them: the user may
    /// not list those namespaces' services, so whether they exist is unknown.
    Pruned {
        forwards: Vec<Forward>,
        skipped: Vec<String>,
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
/// A line that is too long or no such message is an
/// [`io::ErrorKind::InvalidData`] error, after which the next line can be
/// read.
pub async fn read_message<R, T>(reader: &mut R) -> io::Result<Option<T>>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    let Some(line) = read_frame(reader).await? else {
        return Ok(None);
    };
    serde_json::from_slice(&line)
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Reads one line, without its newline, of at most [`MAX_FRAME`] bytes;
/// `None` when the other side closed the connection. A longer line is
/// skipped up to its newline without keeping it and is an
/// [`io::ErrorKind::InvalidData`] error, after which the next line can be
/// read.
pub async fn read_frame<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    read_frame_within(reader, MAX_FRAME).await
}

async fn read_frame_within<R>(reader: &mut R, max: usize) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    let mut too_long = false;
    let mut read_any = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            break; // closed, maybe after a last line without newline
        }
        read_any = true;
        let (chunk, complete) = match available.iter().position(|&b| b == b'\n') {
            Some(at) => (&available[..at], true),
            None => (available, false),
        };
        if !too_long {
            if line.len() + chunk.len() > max {
                too_long = true;
                line = Vec::new();
            } else {
                line.extend_from_slice(chunk);
            }
        }
        let used = chunk.len() + usize::from(complete);
        reader.consume(used);
        if complete {
            break;
        }
    }
    if too_long {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a message longer than {max} bytes"),
        ));
    }
    Ok(read_any.then_some(line))
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

    #[test]
    fn another_version_gets_a_hint() {
        assert_eq!(other_version(VERSION), None);
        let hint = other_version("1.0.0").unwrap();
        assert_eq!(
            hint,
            format!(
                "the daemon runs kprtfwd 1.0.0, this is {VERSION}; restart it with `kprtfwd daemon restart`"
            )
        );
    }

    #[test]
    fn a_line_that_is_no_request_gets_an_error_with_its_id() {
        let refusal = |line: &str| ClientMessage::parse(line.as_bytes()).unwrap_err();
        let (id, message) = refusal(r#"{"id":4,"request":{"type":"fly"}}"#);
        assert_eq!(id, 4);
        assert!(
            message.starts_with("cannot read the request: unknown variant `fly`"),
            "{message}"
        );
        assert_eq!(refusal(r#"{"request":{"type":"snapshot"}}"#).0, 0);
        assert_eq!(refusal("garbage").0, 0);
        assert_eq!(refusal("").0, 0);
        let parsed = ClientMessage::parse(br#"{"id":5,"request":{"type":"snapshot"}}"#);
        assert_eq!(
            parsed,
            Ok(ClientMessage {
                id: 5,
                request: Request::Snapshot
            })
        );
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

    #[tokio::test]
    async fn a_line_too_long_is_skipped_up_to_its_newline() {
        let input = b"0123456789\n01234567890123\n0123\n".as_slice();
        let mut reader = tokio::io::BufReader::with_capacity(4, input);
        let line = read_frame_within(&mut reader, 10).await.unwrap();
        assert_eq!(line.as_deref(), Some(b"0123456789".as_slice()));
        let err = read_frame_within(&mut reader, 10).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "a message longer than 10 bytes");
        let line = read_frame_within(&mut reader, 10).await.unwrap();
        assert_eq!(line.as_deref(), Some(b"0123".as_slice()));
        assert_eq!(read_frame_within(&mut reader, 10).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_last_line_without_newline_is_read() {
        let mut reader = tokio::io::BufReader::new(b"\nlast".as_slice());
        let empty = read_frame(&mut reader).await.unwrap();
        assert_eq!(empty.as_deref(), Some(b"".as_slice()));
        let last = read_frame(&mut reader).await.unwrap();
        assert_eq!(last.as_deref(), Some(b"last".as_slice()));
        assert_eq!(read_frame(&mut reader).await.unwrap(), None);

        let mut reader = tokio::io::BufReader::with_capacity(4, b"01234567890123".as_slice());
        let err = read_frame_within(&mut reader, 10).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(read_frame_within(&mut reader, 10).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_line_that_is_no_message_leaves_the_next_readable() {
        let input = b"garbage\n{\"type\":\"error\",\"id\":3,\"message\":\"no\"}\n";
        let mut reader = tokio::io::BufReader::new(input.as_slice());
        let err = read_message::<_, ServerMessage>(&mut reader)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let next: Option<ServerMessage> = read_message(&mut reader).await.unwrap();
        assert_eq!(
            next,
            Some(ServerMessage::Error {
                id: 3,
                message: "no".into()
            })
        );
    }
}
