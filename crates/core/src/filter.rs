//! Filtering, shared by the TUI and the CLI.
//!
//! The forward filter: every word must appear somewhere in the forward,
//! case-insensitively; `stag mysql` finds the MySQL forwards in staging. A word
//! starting with `@` names a project and keeps only its members.

use crate::model::Forward;

/// Whether `forward` matches every whitespace-separated word of `query`. An
/// empty query matches everything. `in_project` answers `@name` words.
pub fn matches_with(query: &str, forward: &Forward, in_project: impl Fn(&str) -> bool) -> bool {
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
        .all(|word| match word.strip_prefix('@') {
            Some(project) => in_project(project),
            None => {
                let word = word.to_lowercase();
                fields.iter().any(|field| field.contains(&word))
            }
        })
}

/// [`matches_with`] without projects: `@name` words match nothing.
pub fn matches(query: &str, forward: &Forward) -> bool {
    matches_with(query, forward, |_| false)
}

/// Shell-style match where `*` stands for any text, anchored at both ends, as
/// v1's namespace filter: `my-app-*`, `*-staging`, `*`.
pub fn wildcard(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        // No `*` at all: the whole text must equal the pattern.
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
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

    #[test]
    fn at_words_ask_for_project_membership() {
        let f = forward();
        let in_shop = |name: &str| name == "shop";
        assert!(matches_with("@shop", &f, in_shop));
        assert!(matches_with("@shop stag", &f, in_shop));
        assert!(!matches_with("@other", &f, in_shop));
        assert!(!matches("@shop", &f));
    }

    #[test]
    fn wildcard_matches_like_v1() {
        for (text, pattern, want) in [
            ("anything", "*", true),
            ("", "*", true),
            ("", "", true),
            ("text", "", false),
            ("my-app-prod", "my-app-*", true),
            ("other-prod", "my-app-*", false),
            ("team-staging", "*-staging", true),
            ("team-prod", "*-staging", false),
            ("a-middle-b", "*middle*", true),
            ("a-end-b", "*middle*", false),
            ("exact", "exact", true),
            ("exact", "exact2", false),
            ("prefix-only", "prefix", false),
            ("abab", "*ab*ab", true),
            ("ab", "a*b*b", false),
        ] {
            assert_eq!(wildcard(pattern, text), want, "{pattern:?} on {text:?}");
        }
    }
}
