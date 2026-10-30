//! The filter used by the TUI and the CLI: every word must appear somewhere in
//! the forward, case-insensitively. `stag mysql` finds the MySQL forwards in
//! staging.

use crate::model::Forward;

/// Whether `forward` matches every whitespace-separated word of `query`. An
/// empty query matches everything.
pub fn matches(query: &str, forward: &Forward) -> bool {
    let fields = [
        forward.id.as_str().to_lowercase(),
        forward.context.to_lowercase(),
        format!(
            "{}/{}:{}",
            forward.namespace, forward.service, forward.remote_port
        )
        .to_lowercase(),
        forward.local_port.to_string(),
    ];
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .all(|word| fields.iter().any(|field| field.contains(&word)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ForwardId;

    fn forward() -> Forward {
        Forward {
            id: ForwardId::new("staging.shop.mysql-3306"),
            context: "staging".into(),
            namespace: "shop".into(),
            service: "mysql".into(),
            remote_port: 3306,
            local_port: 13306,
            enabled: false,
        }
    }

    #[test]
    fn every_word_must_match_some_field() {
        let f = forward();
        assert!(matches("", &f));
        assert!(matches("stag mysql", &f));
        assert!(matches("STAG  MySQL", &f));
        assert!(matches("shop/mysql", &f));
        assert!(matches("13306", &f));
        assert!(matches(":3306", &f));
        assert!(!matches("stag redis", &f));
        assert!(!matches("production", &f));
    }
}
