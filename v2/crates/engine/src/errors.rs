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
    use super::*;

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
}
