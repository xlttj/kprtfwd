//! The WebSocket port-forward protocol (`v4.channel.k8s.io`).
//!
//! kube-rs opens the upgraded connection (TLS, auth, handshake). Framing and
//! the connection's lifetime are handled here.
//!
//! Every binary message starts with a channel byte: 0 carries data, 1 carries
//! an error text, both for the single forwarded port. The first message on
//! each channel is the port number (u16, little endian). The protocol has no
//! half-close, so when the local side ends, the whole stream is closed.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use k8s_openapi::api::core::v1::Pod;
use kube::Resource;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{self, Message};

const DATA: u8 = 0;
const ERROR: u8 = 1;

/// How long to wait for the pod side to finish after the local side closed.
const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// How long opening a stream may take. Over a dead VPN the TLS handshake can
/// hang until the health probe notices; this bounds every attempt.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// The upgraded connection kube-rs hands out.
pub type KubeIo = TokioIo<Upgraded>;

#[derive(Debug, thiserror::Error)]
pub enum PortForwardError {
    #[error(transparent)]
    Connect(kube::Error),
    #[error("no answer from the API server within {0:?}")]
    Timeout(Duration),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("websocket error: {0}")]
    WebSocket(#[from] tungstenite::Error),
    /// Text the kubelet sent on the error channel, e.g. when nothing listens
    /// on the port inside the pod.
    #[error("{0}")]
    Pod(String),
    #[error("stream closed")]
    Closed,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Bytes moved by one connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Transfer {
    pub sent: u64,
    pub received: u64,
}

/// An open port-forward stream to one port of one pod.
pub struct PortForward<S> {
    ws: WebSocketStream<S>,
    port: u16,
    /// Whether the port header arrived on the data and error channel.
    initialized: [bool; 2],
}

/// Opens a port-forward stream to `port` of `pod`.
pub async fn open(
    client: &kube::Client,
    namespace: &str,
    pod: &str,
    port: u16,
) -> Result<PortForward<KubeIo>, PortForwardError> {
    let request = kube::core::Request::new(Pod::url_path(&(), Some(namespace)))
        .portforward(pod, &[port])
        .map_err(|err| PortForwardError::Protocol(err.to_string()))?;
    let connection = tokio::time::timeout(OPEN_TIMEOUT, client.connect(request))
        .await
        .map_err(|_| PortForwardError::Timeout(OPEN_TIMEOUT))?
        .map_err(PortForwardError::Connect)?;
    Ok(PortForward::new(connection.into_stream(), port))
}

impl<S> PortForward<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(ws: WebSocketStream<S>, port: u16) -> Self {
        Self {
            ws,
            port,
            initialized: [false; 2],
        }
    }

    /// Waits on a stream that was opened ahead of time and is not used yet.
    /// Port headers are taken in; anything else makes the stream unusable, and
    /// this returns why. Cancel-safe, so it can run in a `select!`.
    pub async fn idle(&mut self) -> PortForwardError {
        loop {
            let message = match self.ws.next().await {
                Some(Ok(message)) => message,
                Some(Err(err)) => return err.into(),
                None => return PortForwardError::Closed,
            };
            match message {
                Message::Binary(frame) => {
                    match read_frame(self.port, &mut self.initialized, &frame) {
                        Ok(Frame::Header) => {}
                        Ok(Frame::Data(_)) => {
                            return PortForwardError::Protocol(
                                "data before the stream was used".into(),
                            );
                        }
                        Err(err) => return err,
                    }
                }
                Message::Close(_) => return PortForwardError::Closed,
                _ => {}
            }
        }
    }

    /// Copies data between `local` and the pod until one side closes.
    pub async fn pump<L>(self, local: L) -> Result<Transfer, PortForwardError>
    where
        L: AsyncRead + AsyncWrite,
    {
        let Self {
            ws,
            port,
            mut initialized,
        } = self;
        let (mut sink, mut source) = ws.split();
        let (mut local_read, mut local_write) = tokio::io::split(local);
        let mut transfer = Transfer::default();

        let upload = async {
            let mut buf = vec![0u8; 32 * 1024];
            let mut sent = 0u64;
            loop {
                let n = local_read.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                let mut frame = Vec::with_capacity(n + 1);
                frame.push(DATA);
                frame.extend_from_slice(&buf[..n]);
                match sink.send(Message::binary(frame)).await {
                    Ok(()) => sent += n as u64,
                    // The pod side closed first; the download half reports it.
                    Err(err) if is_closed(&err) => break,
                    Err(err) => return Err(err.into()),
                }
            }
            // No half-close in the protocol: closing ends both directions.
            let _ = sink.send(Message::Close(None)).await;
            Ok::<u64, PortForwardError>(sent)
        };

        let download = async {
            let mut received = 0u64;
            while let Some(message) = source.next().await {
                match message {
                    Ok(Message::Binary(frame)) => {
                        if let Frame::Data(data) = read_frame(port, &mut initialized, &frame)? {
                            local_write.write_all(data).await?;
                            received += data.len() as u64;
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(err) if is_closed(&err) => break,
                    Err(err) => return Err(err.into()),
                }
            }
            let _ = local_write.shutdown().await;
            Ok::<u64, PortForwardError>(received)
        };

        tokio::pin!(upload, download);
        tokio::select! {
            received = &mut download => {
                transfer.received = received?;
            }
            sent = &mut upload => {
                transfer.sent = sent?;
                if let Ok(received) = tokio::time::timeout(CLOSE_GRACE, &mut download).await {
                    transfer.received = received?;
                }
            }
        }
        Ok(transfer)
    }
}

/// A received frame, once port headers are taken in.
enum Frame<'a> {
    Header,
    Data(&'a [u8]),
}

/// Takes in the port header that starts each channel and checks it; returns
/// data frames. A message on the error channel becomes [`PortForwardError::Pod`].
fn read_frame<'a>(
    port: u16,
    initialized: &mut [bool; 2],
    frame: &'a [u8],
) -> Result<Frame<'a>, PortForwardError> {
    let Some((&channel, payload)) = frame.split_first() else {
        return Err(PortForwardError::Protocol("empty message".into()));
    };
    let index = match channel {
        DATA => 0,
        ERROR => 1,
        other => {
            return Err(PortForwardError::Protocol(format!(
                "unknown channel {other}"
            )));
        }
    };
    if !initialized[index] {
        if payload != port.to_le_bytes() {
            return Err(PortForwardError::Protocol(format!(
                "port header {payload:?} does not match port {port}"
            )));
        }
        initialized[index] = true;
        return Ok(Frame::Header);
    }
    if channel == ERROR {
        return Err(PortForwardError::Pod(
            String::from_utf8_lossy(payload).into_owned(),
        ));
    }
    Ok(Frame::Data(payload))
}

/// Errors that only mean the stream is over. A peer that drops the connection
/// without finishing the close handshake ends the forward just the same.
fn is_closed(err: &tungstenite::Error) -> bool {
    use tungstenite::error::ProtocolError;
    matches!(
        err,
        tungstenite::Error::ConnectionClosed
            | tungstenite::Error::AlreadyClosed
            | tungstenite::Error::Protocol(
                ProtocolError::SendAfterClosing | ProtocolError::ResetWithoutClosingHandshake
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::DuplexStream;
    use tokio_tungstenite::tungstenite::protocol::Role;

    const PORT: u16 = 3000;

    /// A port-forward stream and the kubelet's end of it.
    async fn pair() -> (PortForward<DuplexStream>, WebSocketStream<DuplexStream>) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        (PortForward::new(client, PORT), server)
    }

    fn frame(channel: u8, payload: &[u8]) -> Message {
        let mut frame = vec![channel];
        frame.extend_from_slice(payload);
        Message::binary(frame)
    }

    async fn send_headers(kubelet: &mut WebSocketStream<DuplexStream>) {
        kubelet
            .send(frame(DATA, &PORT.to_le_bytes()))
            .await
            .unwrap();
        kubelet
            .send(frame(ERROR, &PORT.to_le_bytes()))
            .await
            .unwrap();
    }

    /// Echoes data frames until the client closes.
    async fn echo(mut kubelet: WebSocketStream<DuplexStream>) {
        send_headers(&mut kubelet).await;
        while let Some(Ok(message)) = kubelet.next().await {
            match message {
                Message::Binary(data) => kubelet.send(Message::binary(data)).await.unwrap(),
                Message::Close(_) => break,
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn copies_both_ways_until_local_closes() {
        let (pf, kubelet) = pair().await;
        let kubelet = tokio::spawn(echo(kubelet));
        let (mut app, local) = tokio::io::duplex(1024);
        let pump = tokio::spawn(pf.pump(local));

        app.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        app.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
        drop(app);

        let transfer = pump.await.unwrap().unwrap();
        assert_eq!(
            transfer,
            Transfer {
                sent: 5,
                received: 5
            }
        );
        kubelet.await.unwrap();
    }

    #[tokio::test]
    async fn ends_when_the_pod_side_closes() {
        let (pf, mut kubelet) = pair().await;
        let (mut app, local) = tokio::io::duplex(1024);
        let pump = tokio::spawn(pf.pump(local));

        send_headers(&mut kubelet).await;
        kubelet.send(frame(DATA, b"bye")).await.unwrap();
        kubelet.close(None).await.unwrap();

        let mut received = Vec::new();
        app.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, b"bye");
        assert_eq!(pump.await.unwrap().unwrap().received, 3);
    }

    #[tokio::test]
    async fn reports_the_error_channel() {
        let (pf, mut kubelet) = pair().await;
        let (_app, local) = tokio::io::duplex(1024);
        let pump = tokio::spawn(pf.pump(local));

        send_headers(&mut kubelet).await;
        kubelet
            .send(frame(ERROR, b"connection refused"))
            .await
            .unwrap();

        match pump.await.unwrap() {
            Err(PortForwardError::Pod(message)) => assert_eq!(message, "connection refused"),
            other => panic!("expected a pod error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_a_wrong_port_header() {
        let (pf, mut kubelet) = pair().await;
        let (_app, local) = tokio::io::duplex(1024);
        let pump = tokio::spawn(pf.pump(local));

        kubelet
            .send(frame(DATA, &8080u16.to_le_bytes()))
            .await
            .unwrap();

        assert!(matches!(
            pump.await.unwrap(),
            Err(PortForwardError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn idle_takes_headers_and_the_stream_stays_usable() {
        let (mut pf, mut kubelet) = pair().await;
        send_headers(&mut kubelet).await;

        // The headers arrive while idle; idle keeps waiting afterwards.
        let waited = tokio::time::timeout(Duration::from_millis(100), pf.idle()).await;
        assert!(waited.is_err(), "idle must not end on headers");
        assert_eq!(pf.initialized, [true, true]);

        // A kubelet that echoes but no longer sends headers.
        let kubelet = tokio::spawn(async move {
            while let Some(Ok(message)) = kubelet.next().await {
                match message {
                    Message::Binary(data) => kubelet.send(Message::binary(data)).await.unwrap(),
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });
        let (mut app, local) = tokio::io::duplex(1024);
        let pump = tokio::spawn(pf.pump(local));
        app.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        app.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        drop(app);
        assert_eq!(pump.await.unwrap().unwrap().sent, 4);
        kubelet.await.unwrap();
    }

    #[tokio::test]
    async fn idle_ends_when_the_stream_becomes_unusable() {
        let (mut pf, mut kubelet) = pair().await;
        send_headers(&mut kubelet).await;
        kubelet.send(frame(DATA, b"greeting")).await.unwrap();
        assert!(matches!(pf.idle().await, PortForwardError::Protocol(_)));

        let (mut pf, mut kubelet) = pair().await;
        kubelet.close(None).await.unwrap();
        assert!(matches!(pf.idle().await, PortForwardError::Closed));
    }
}
