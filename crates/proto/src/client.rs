//! The client side of the daemon connection.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tokio::time::Instant;

use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::{
    ClientMessage, PROTOCOL_VERSION, Request, Response, ServerMessage, Snapshot, VERSION,
    read_message, write_message,
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
        /// The daemon's own, from its welcome.
        pid: u32,
    },
    /// A daemon answers, but not in this protocol: most likely an older
    /// version that is still running.
    #[error(
        "a daemon is running that this kprtfwd cannot talk to (probably another version); restart it with `kprtfwd daemon restart`"
    )]
    Incompatible,
    /// Something accepts on the socket but does not welcome the client: a
    /// daemon that hangs or is stopped.
    #[error("the daemon does not answer within {0:?}; restart it with `kprtfwd daemon restart`")]
    NoAnswer(Duration),
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

/// How long a running daemon may take to welcome a client. It answers at
/// once; one that is still starting is waited for by [`connect_or_start`].
const HANDSHAKE_WITHIN: Duration = Duration::from_secs(5);

impl Client {
    /// Connects to a running daemon and checks that it speaks our protocol,
    /// waiting at most [`HANDSHAKE_WITHIN`] for its welcome.
    pub async fn connect(socket: &Path) -> Result<Self, ClientError> {
        Self::connect_within(socket, HANDSHAKE_WITHIN).await
    }

    /// [`Client::connect`], waiting at most `limit` for the welcome.
    pub async fn connect_within(socket: &Path, limit: Duration) -> Result<Self, ClientError> {
        tokio::time::timeout(limit, Self::handshake(socket))
            .await
            .unwrap_or(Err(ClientError::NoAnswer(limit)))
    }

    async fn handshake(socket: &Path) -> Result<Self, ClientError> {
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
            client_version: VERSION.to_owned(),
        };
        // A daemon that closes the connection, refuses the hello or answers
        // with something else than a welcome speaks another protocol.
        let welcome = match client.request(hello).await {
            Err(ClientError::Closed | ClientError::Unexpected | ClientError::Daemon(_)) => {
                return Err(ClientError::Incompatible);
            }
            Err(ClientError::Io(err)) if err.kind() == io::ErrorKind::InvalidData => {
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
                        pid,
                    });
                }
                client.daemon_version = daemon_version;
                client.daemon_pid = pid;
                Ok(client)
            }
            _ => Err(ClientError::Incompatible),
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
            // 0: the daemon could not read the request, e.g. one too long.
            Some(ServerMessage::Error {
                id: answered,
                message,
            }) if answered == id || answered == 0 => Err(ClientError::Daemon(message)),
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
    let deadline = Instant::now() + WAIT;
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ClientError::StartTimeout(WAIT));
        }
        // A daemon that is starting accepts before it answers; it may take
        // the time that is left.
        match Client::connect_within(socket, left).await {
            Ok(client) => return Ok(client),
            Err(ClientError::NotRunning(_)) => {}
            Err(err) => {
                let err = match err {
                    ClientError::NoAnswer(_) => ClientError::StartTimeout(WAIT),
                    err => err,
                };
                return Err(start_failed(&mut daemon).await?.unwrap_or(err));
            }
        }
        // Exits at once when another daemon won the race (then the next
        // connect succeeds) or when it cannot start.
        if let Some(status) = daemon.try_wait()?
            && !status.success()
        {
            return Err(ClientError::StartFailed(status));
        }
    }
}

/// [`ClientError::StartFailed`] if `daemon` fails within a second. One that
/// fails after binding the socket closes the connections waiting there
/// before it exits; that is no daemon of another version.
async fn start_failed(daemon: &mut Child) -> io::Result<Option<ClientError>> {
    for _ in 0..20 {
        match daemon.try_wait()? {
            Some(status) if !status.success() => return Ok(Some(ClientError::StartFailed(status))),
            Some(_) => return Ok(None),
            None => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    Ok(None)
}

/// Stops the daemon that serves `socket` and waits until it is gone. One of
/// another protocol, which does not understand the request, is stopped by the
/// PID in its welcome; one that answers in no known way, or not at all, by
/// the PID in `pid_file`. Returns `false` if no daemon was running.
pub async fn stop_daemon(socket: &Path, pid_file: &Path) -> Result<bool, ClientError> {
    match Client::connect(socket).await {
        Ok(mut client) => match client.act(Request::Shutdown).await {
            // A daemon of an earlier build may exit before its answer.
            Ok(()) | Err(ClientError::Closed) => {}
            Err(err) => return Err(err),
        },
        Err(ClientError::NotRunning(_)) => return Ok(false),
        Err(ClientError::ProtocolMismatch { pid, .. }) => kill(pid)?,
        Err(err @ (ClientError::Incompatible | ClientError::NoAnswer(_))) => {
            let daemon = match err {
                ClientError::NoAnswer(_) => "a daemon that does not answer",
                _ => "a daemon of another version",
            };
            let pid = std::fs::read_to_string(pid_file)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
                .ok_or_else(|| {
                    ClientError::CannotStop(format!(
                        "{daemon} is running and {} is missing; stop it by hand",
                        pid_file.display()
                    ))
                })?;
            kill(pid)?;
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

/// Sends SIGTERM to `pid`.
fn kill(pid: u32) -> Result<(), ClientError> {
    let status = Command::new("kill").arg(pid.to_string()).status()?;
    if !status.success() {
        return Err(ClientError::CannotStop(format!("kill {pid} failed")));
    }
    Ok(())
}

/// Whether no daemon holds the lock file `path`. Builds before the lock have
/// no such file, and one that cannot be opened cannot be checked; both count
/// as unlocked. Holds the lock only for the check.
fn unlocked(path: &Path) -> bool {
    std::fs::File::open(path).map_or(true, |file| file.try_lock().is_ok())
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    use super::*;

    /// A socket in a directory of its own, short enough for macOS.
    fn socket() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::Builder::new().prefix("p").tempdir().unwrap();
        let path = dir.path().join("s");
        (dir, path)
    }

    /// Answers the first line of every connection with `answer`.
    fn answering(listener: UnixListener, answer: &'static str) {
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut hello = String::new();
                let _ = BufReader::new(reader).read_line(&mut hello).await;
                let _ = writer.write_all(answer.as_bytes()).await;
            }
        });
    }

    #[tokio::test]
    async fn a_daemon_that_does_not_answer_is_given_up() {
        let (_dir, path) = socket();
        // Bound, so connections queue, but nothing ever accepts them.
        let _listener = UnixListener::bind(&path).unwrap();
        let limit = Duration::from_millis(200);
        let result = Client::connect_within(&path, limit).await;
        assert!(
            matches!(result, Err(ClientError::NoAnswer(d)) if d == limit),
            "{:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn a_welcome_that_cannot_be_read_is_another_protocol() {
        let answers = [
            "garbage\n",
            "{\"type\":\"response\",\"id\":1,\"response\":{\"type\":\"welcome\",\"protocol\":\"8\"}}\n",
            "{\"type\":\"error\",\"id\":1,\"message\":\"unknown request\"}\n",
            "{\"type\":\"response\",\"id\":1,\"response\":{\"type\":\"done\"}}\n",
            "",
        ];
        for answer in answers {
            let (_dir, path) = socket();
            answering(UnixListener::bind(&path).unwrap(), answer);
            let result = Client::connect(&path).await;
            assert!(
                matches!(result, Err(ClientError::Incompatible)),
                "{answer}: {:?}",
                result.err()
            );
        }
    }

    #[tokio::test]
    async fn another_protocol_comes_with_the_daemons_pid() {
        let (_dir, path) = socket();
        let welcome = "{\"type\":\"response\",\"id\":1,\"response\":{\"type\":\"welcome\",\"protocol\":999,\"daemon_version\":\"9.9.9\",\"pid\":4242}}\n";
        answering(UnixListener::bind(&path).unwrap(), welcome);
        match Client::connect(&path).await {
            Err(ClientError::ProtocolMismatch {
                daemon: 999, pid, ..
            }) => assert_eq!(pid, 4242),
            other => panic!("{:?}", other.err()),
        }
    }
}
