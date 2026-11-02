//! IDs and local ports for new forwards.

use std::collections::HashSet;

use crate::model::ForwardId;

/// The ID for a new forward: `<context>.<namespace>.<service>-<port>`, each
/// part sanitized as in v1, with `-2`, `-3`, ... appended if it is taken.
pub fn forward_id(
    context: &str,
    namespace: &str,
    service: &str,
    remote_port: u16,
    taken: &HashSet<ForwardId>,
) -> ForwardId {
    let base = format!(
        "{}.{}.{}-{remote_port}",
        sanitize(context),
        sanitize(namespace),
        sanitize(service)
    );
    let mut id = ForwardId::new(base.clone());
    let mut n = 2;
    while taken.contains(&id) {
        id = ForwardId::new(format!("{base}-{n}"));
        n += 1;
    }
    id
}

/// Keeps ASCII alphanumerics and turns runs of `-`, `_` and `.` into one `-`,
/// without one at the end (v1's `sanitizeIDPart`). Context names like EKS ARNs
/// shrink to their letters and digits.
fn sanitize(part: &str) -> String {
    let mut out = String::with_capacity(part.len());
    for c in part.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if matches!(c, '-' | '_' | '.') && !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_end_matches('-');
    if trimmed.is_empty() {
        "unknown".into()
    } else {
        trimmed.to_owned()
    }
}

/// The local port proposed for a new forward to `remote_port`, in this order:
/// the remote port if it is 1024 or higher, `10000 + remote`, then the next
/// port upwards. A port qualifies if no forward uses it (`taken`) and
/// `is_free` says nothing listens on it.
pub fn propose_local_port(
    remote_port: u16,
    taken: &HashSet<u16>,
    is_free: impl Fn(u16) -> bool,
) -> Option<u16> {
    let usable = |port: u16| port != 0 && !taken.contains(&port) && is_free(port);
    if remote_port >= 1024 && usable(remote_port) {
        return Some(remote_port);
    }
    let start = 10_000u16.checked_add(remote_port).unwrap_or(10_000);
    (start..=u16::MAX)
        .chain(1024..start)
        .find(|port| usable(*port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_readable_unique_ids() {
        let mut taken = HashSet::new();
        let id = forward_id("staging", "shop", "frontend", 80, &taken);
        assert_eq!(id.as_str(), "staging.shop.frontend-80");
        taken.insert(id);
        assert_eq!(
            forward_id("staging", "shop", "frontend", 80, &taken).as_str(),
            "staging.shop.frontend-80-2"
        );
        assert_eq!(
            forward_id(
                "arn:aws:eks:eu-central-1:123:cluster/stg",
                "a__b",
                "c",
                1,
                &taken
            )
            .as_str(),
            "arnawsekseu-central-1123clusterstg.a-b.c-1"
        );
        assert_eq!(
            forward_id("", "-", "x", 1, &taken).as_str(),
            "unknown.unknown.x-1"
        );
    }

    #[test]
    fn proposes_ports_in_order() {
        let none = HashSet::new();
        let all_free = |_| true;
        assert_eq!(propose_local_port(8080, &none, all_free), Some(8080));
        assert_eq!(propose_local_port(80, &none, all_free), Some(10080));

        let taken: HashSet<u16> = [8080, 18080, 18081].into();
        assert_eq!(propose_local_port(8080, &taken, all_free), Some(18082));

        let mysql_busy = |port| port != 3306;
        assert_eq!(propose_local_port(3306, &none, mysql_busy), Some(13306));

        // Near the top of the range it wraps around instead of overflowing.
        assert_eq!(
            propose_local_port(60000, &[60000].into(), all_free),
            Some(10000)
        );
    }
}
