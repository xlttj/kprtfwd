//! The client side of the daemon connection.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::{
    ClientMessage, PROTOCOL_VERSION, Request, Response, ServerMessage, Snapshot, read_message,
    write_message,
};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("the daemon is not running")]
    NotRunning(#[source] io::Error),
    #[error("the daemon did not start within {0:?}; see its log")]
    StartTimeout(Duration),
    #[error("the daemon exited right after starting ({0}); see its log")]
    StartFailed(std::process::ExitStatus),
    #[error(
        "the daemon (version {daemon_version}) speaks protocol {daemon}, this kprtfwd speaks {client}; restart it with `kprtfwd daemon restart`"
    )]
    ProtocolMismatch {
        daemon: u32,
        client: u32,
        daemon_version: String,
    },
    /// A daemon answers, but not in this protocol: most likely an older
    /// version that is still running.
    #[error(
        "a daemon is running that this kprtfwd cannot talk to (probably another version); restart it with `kprtfwd daemon restart`"
    )]
    Incompatible,
    #[error("{0}")]
    Daemon(String),
    #[error("{0}")]
    CannotStop(String),
    #[error("the daemon closed the connection")]
    Closed,
    #[error("unexpected answer from the daemon")]
    Unexpected,
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
    pub daemon_version: String,
    pub daemon_pid: u32,
}

impl Client {
    /// Connects to a running daemon and checks that it speaks our protocol.
    pub async fn connect(socket: &Path) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket).await.map_err(|err| {
            // Only these mean "nobody listens"; anything else, like a socket
            // path that is too long, will not go away by starting a daemon.
            match err.kind() {
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                    ClientError::NotRunning(err)
                }
                _ => ClientError::Io(err),
            }
        })?;
        let (reader, writer) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(reader),
            writer,
            next_id: 0,
            daemon_version: String::new(),
            daemon_pid: 0,
        };
        let hello = Request::Hello {
            protocol: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        let welcome = match client.request(hello).await {
            Err(ClientError::Closed | ClientError::Unexpected) => {
                return Err(ClientError::Incompatible);
            }
            other => other?,
        };
        match welcome {
            Response::Welcome {
                protocol,
                daemon_version,
                pid,
            } => {
                if protocol != PROTOCOL_VERSION {
                    return Err(ClientError::ProtocolMismatch {
                        daemon: protocol,
                        client: PROTOCOL_VERSION,
                        daemon_version,
                    });
                }
                client.daemon_version = daemon_version;
                client.daemon_pid = pid;
                Ok(client)
            }
            _ => Err(ClientError::Unexpected),
        }
    }

    pub async fn request(&mut self, request: Request) -> Result<Response, ClientError> {
        self.next_id += 1;
        let id = self.next_id;
        write_message(&mut self.writer, &ClientMessage { id, request }).await?;
        match read_message(&mut self.reader).await? {
            Some(ServerMessage::Response {
                id: answered,
                response,
            }) if answered == id => Ok(response),
            Some(ServerMessage::Error {
                id: answered,
                message,
            }) if answered == id => Err(ClientError::Daemon(message)),
            Some(_) => Err(ClientError::Unexpected),
            None => Err(ClientError::Closed),
        }
    }

    pub async fn snapshot(&mut self) -> Result<Snapshot, ClientError> {
        match self.request(Request::Snapshot).await? {
            Response::Snapshot(snapshot) => Ok(snapshot),
            _ => Err(ClientError::Unexpected),
        }
    }

    /// Sends a request that only confirms it was done.
    pub async fn act(&mut self, request: Request) -> Result<(), ClientError> {
        match self.request(request).await? {
            Response::Done => Ok(()),
            _ => Err(ClientError::Unexpected),
        }
    }
}

/// Starts `<exe> daemon run` in the background, in its own process group, so
/// it outlives the terminal it was started from.
pub fn spawn_daemon(exe: &Path) -> io::Result<Child> {
    Command::new(exe)
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
}

/// Connects to the daemon, starting it first if it is not running.
pub async fn connect_or_start(socket: &Path, exe: &Path) -> Result<Client, ClientError> {
    const WAIT: Duration = Duration::from_secs(10);
    match Client::connect(socket).await {
        Err(ClientError::NotRunning(_)) => {}
        other => return other,
    }
    let mut daemon = spawn_daemon(exe)?;
    let deadline = tokio::time::Instant::now() + WAIT;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        match Client::connect(socket).await {
            Err(ClientError::NotRunning(_)) => {}
            other => return other,
        }
        // Exits at once when another daemon won the race (then the next
        // connect succeeds) or when it cannot start.
        if let Some(status) = daemon.try_wait()?
            && !status.success()
        {
            return Err(ClientError::StartFailed(status));
        }
    }
    Err(ClientError::StartTimeout(WAIT))
}

/// Stops the daemon that serves `socket` and waits until it is gone. One of
/// another version, which does not understand the request, is stopped by the
/// PID in `pid_file`. Returns `false` if no daemon was running.
pub async fn stop_daemon(socket: &Path, pid_file: &Path) -> Result<bool, ClientError> {
    match Client::connect(socket).await {
        Ok(mut client) => match client.act(Request::Shutdown).await {
            // A daemon of an earlier build may exit before its answer.
            Ok(()) | Err(ClientError::Closed) => {}
            Err(err) => return Err(err),
        },
        Err(ClientError::NotRunning(_)) => return Ok(false),
        Err(ClientError::Incompatible | ClientError::ProtocolMismatch { .. }) => {
            let pid = std::fs::read_to_string(pid_file)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
                .ok_or_else(|| {
                    ClientError::CannotStop(format!(
                        "a daemon of another version is running and {} is missing; stop it by hand",
                        pid_file.display()
                    ))
                })?;
            let status = Command::new("kill").arg(pid.to_string()).status()?;
            if !status.success() {
                return Err(ClientError::CannotStop(format!("kill {pid} failed")));
            }
        }
        Err(err) => return Err(err),
    }
    // The daemon removes its socket first and releases the lock of its
    // directory last, with the process.
    let lock = socket.with_file_name("daemon.lock");
    for _ in 0..100 {
        if !socket.exists() && unlocked(&lock) {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(ClientError::CannotStop(format!(
        "the daemon on {} did not stop within 5 s",
        socket.display()
    )))
}

/// Whether no daemon holds the lock file `path`. Builds before the lock have
/// no such file, and one that cannot be opened cannot be checked; both count
/// as unlocked. Holds the lock only for the check.
fn unlocked(path: &Path) -> bool {
    std::fs::File::open(path).map_or(true, |file| file.try_lock().is_ok())
}
