//! Sorts kube errors into what the user can act on.

use std::error::Error;

use kprtfwd_core::state::NetCause;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Net(NetCause),
    /// Login missing or expired: the exec plugin failed, or HTTP 401.
    Auth(String),
    /// HTTP 403.
    Forbidden(String),
    /// HTTP 404.
    NotFound(String),
    Other(String),
}

/// Classifies an error by its kube error variants and status codes, then by
/// the text of its source chain (network errors arrive as nested I/O errors).
pub fn classify(err: &(dyn Error + 'static)) -> Failure {
    let text = chain(err);
    let mut source: Option<&(dyn Error + 'static)> = Some(err);
    while let Some(current) = source {
        if let Some(kube_err) = current.downcast_ref::<kube::Error>() {
            match kube_err {
                kube::Error::Auth(_) => return Failure::Auth(text),
                kube::Error::Api(status) => match status.code {
                    401 => return Failure::Auth(status.message.clone()),
                    403 => return Failure::Forbidden(status.message.clone()),
                    404 => return Failure::NotFound(status.message.clone()),
                    _ => {}
                },
                _ => {}
            }
        }
        source = current.source();
    }

    let lower = text.to_lowercase();
    let has = |markers: &[&str]| markers.iter().any(|m| lower.contains(m));
    if has(&[
        "dns error",
        "failed to lookup address",
        "nodename nor servname",
        "name or service not known",
        "no such host",
        "temporary failure in name resolution",
    ]) {
        Failure::Net(NetCause::Dns)
    } else if has(&["connection refused"]) {
        Failure::Net(NetCause::Refused)
    } else if has(&["timed out", "timeout", "deadline has elapsed"]) {
        Failure::Net(NetCause::Timeout)
    } else if has(&["certificate", "tls", "handshake"]) {
        Failure::Net(NetCause::Tls(text))
    } else if has(&["401 unauthorized"]) {
        Failure::Auth(text)
    } else if has(&["403 forbidden"]) {
        Failure::Forbidden(text)
    } else if has(&["404 not found"]) {
        Failure::NotFound(text)
    } else if has(&[
        "connection reset",
        "broken pipe",
        "network is unreachable",
        "no route to host",
        "error trying to connect",
    ]) {
        Failure::Net(NetCause::Other(text))
    } else {
        Failure::Other(text)
    }
}

/// The error and all its sources, joined with ": ", without repeats (many
/// wrappers repeat their source's text).
pub fn chain(err: &(dyn Error + 'static)) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut source: Option<&(dyn Error + 'static)> = Some(err);
    while let Some(current) = source {
        let text = current.to_string();
        if !parts.iter().any(|part| part.contains(&text)) {
            parts.push(text);
        }
        source = current.source();
    }
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::fake::{FakeApi, Reply, closed_port, status};
    use crate::portforward::PortForwardError;

    fn io(kind: std::io::ErrorKind, text: &str) -> std::io::Error {
        std::io::Error::new(kind, text.to_owned())
    }

    #[test]
    fn classifies_network_errors_by_text() {
        let dns = io(
            std::io::ErrorKind::Other,
            "dns error: failed to lookup address information: nodename nor servname provided",
        );
        assert_eq!(classify(&dns), Failure::Net(NetCause::Dns));
        let refused = io(
            std::io::ErrorKind::ConnectionRefused,
            "Connection refused (os error 61)",
        );
        assert_eq!(classify(&refused), Failure::Net(NetCause::Refused));
        let other = io(std::io::ErrorKind::Other, "something odd");
        assert_eq!(classify(&other), Failure::Other("something odd".into()));
    }

    #[test]
    fn classifies_api_status_codes() {
        let status = |code: u16| {
            kube::Error::Api(Box::new(
                serde_json::from_value(serde_json::json!({
                    "status": "Failure",
                    "message": "nope",
                    "reason": "",
                    "code": code,
                }))
                .unwrap(),
            ))
        };
        assert_eq!(classify(&status(401)), Failure::Auth("nope".into()));
        assert_eq!(classify(&status(403)), Failure::Forbidden("nope".into()));
        assert_eq!(classify(&status(404)), Failure::NotFound("nope".into()));
    }

    fn client(url: &str) -> kube::Client {
        kube::Client::try_from(kube::Config::new(url.parse().unwrap())).unwrap()
    }

    /// A server that answers anything with an HTTP 400, as a plain HTTP
    /// server answers a TLS handshake.
    async fn plain_http_server() -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
                    .await;
            }
        });
        addr
    }

    /// The errors as kube-rs, hyper-util and rustls really return them.
    #[tokio::test]
    async fn classifies_the_errors_a_client_returns() {
        let port = closed_port();
        let refused = client(&format!("http://127.0.0.1:{port}"))
            .apiserver_version()
            .await
            .unwrap_err();
        assert!(chain(&refused).contains("tcp connect error"), "{refused}");
        assert_eq!(classify(&refused), Failure::Net(NetCause::Refused));

        // .invalid never resolves (RFC 6761).
        let unknown = tokio::time::timeout(
            Duration::from_secs(10),
            client("http://kprtfwd-test.invalid").apiserver_version(),
        )
        .await
        .expect("DNS answers")
        .unwrap_err();
        assert_eq!(classify(&unknown), Failure::Net(NetCause::Dns), "{unknown}");

        // Closed without an answer: unreachable, for want of a better cause.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                drop(socket);
            }
        });
        let closed = client(&format!("http://{addr}"))
            .apiserver_version()
            .await
            .unwrap_err();
        assert!(
            matches!(classify(&closed), Failure::Other(text) if text.contains("connection closed")),
            "{closed}"
        );
    }

    /// TLS failures as rustls words them. A handshake with a server that does
    /// not speak TLS is not recognised as TLS yet (it ends as Other).
    #[tokio::test]
    async fn classifies_tls_errors() {
        use rustls::{CertificateError, Error as Tls};
        for error in [
            Tls::InvalidCertificate(CertificateError::UnknownIssuer),
            Tls::InvalidCertificate(CertificateError::Expired),
            Tls::InvalidCertificate(CertificateError::NotValidForName),
        ] {
            let text = error.to_string();
            let err = std::io::Error::new(std::io::ErrorKind::InvalidData, error);
            assert_eq!(classify(&err), Failure::Net(NetCause::Tls(text)));
        }

        let addr = plain_http_server().await;
        let plain = client(&format!("https://{addr}"))
            .apiserver_version()
            .await
            .unwrap_err();
        assert!(chain(&plain).contains("corrupt message"), "{plain}");
        assert!(matches!(classify(&plain), Failure::Other(_)), "{plain}");
    }

    /// `portforward::open` fails with kube's upgrade error, which carries
    /// only the status line.
    #[tokio::test]
    async fn classifies_refused_upgrades() {
        for (code, expected) in [
            (401, "Unauthorized"),
            (403, "Forbidden"),
            (404, "Not Found"),
        ] {
            let api = FakeApi::start(move |_| Reply::Json(code, status(code, "nope"))).await;
            let Err(PortForwardError::Connect(err)) =
                crate::portforward::open(&api.client(), "shop", "web-0", 80).await
            else {
                panic!("{code}: the upgrade did not fail");
            };
            let text = format!(
                "failed to upgrade to a WebSocket connection: failed to switch protocol: {code} {expected}"
            );
            let failure = match code {
                401 => Failure::Auth(text),
                403 => Failure::Forbidden(text),
                _ => Failure::NotFound(text),
            };
            assert_eq!(classify(&err), failure);
        }
    }
}
