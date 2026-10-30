//! Messages between the kprtfwd daemon and its clients.
//!
//! Newline-delimited JSON over a Unix socket. A client starts with
//! [`Request::Hello`]; the daemon answers with [`Response::Welcome`]. After
//! that every request carries an `id`, and its response echoes it.

mod client;

use std::io;
use std::time::SystemTime;

use kprtfwd_core::{ContextState, Forward, ForwardId, ForwardState};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub use client::{Client, ClientError, connect_or_start, spawn_daemon};

/// Bumped on every incompatible change to the messages.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientMessage {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello {
        protocol: u32,
        client_version: String,
    },
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
    fn requests_are_flat_json() {
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
            r#"{"id":7,"type":"set_enabled","ids":["a"],"enabled":true}"#
        );
        assert_eq!(
            serde_json::from_str::<ClientMessage>(&json).unwrap(),
            message
        );

        let shutdown = r#"{"id":1,"type":"shutdown"}"#;
        assert_eq!(
            serde_json::from_str::<ClientMessage>(shutdown)
                .unwrap()
                .request,
            Request::Shutdown
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
}
