//! Validation of user-supplied forward and project data.

use crate::model::{Forward, ForwardSettings, Project};

/// Maximum length of a Kubernetes name (RFC 1123 label).
const MAX_NAME_LEN: usize = 63;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("{0} must not be empty")]
    Empty(&'static str),
    #[error("{field} {value:?} contains whitespace or control characters")]
    InvalidCharacters { field: &'static str, value: String },
    #[error("{field} {value:?} is not a valid Kubernetes name")]
    InvalidName { field: &'static str, value: String },
    #[error("{field} {value:?} exceeds {MAX_NAME_LEN} characters")]
    NameTooLong { field: &'static str, value: String },
    #[error("{0} must not be 0")]
    ZeroPort(&'static str),
}

pub fn validate_forward(forward: &Forward) -> Result<(), ValidationError> {
    validate_text("id", forward.id.as_str())?;
    validate_settings(&forward.settings())
}

pub fn validate_settings(settings: &ForwardSettings) -> Result<(), ValidationError> {
    validate_text("context", &settings.context)?;
    validate_kubernetes_name("namespace", &settings.namespace)?;
    validate_kubernetes_name("service", &settings.service)?;
    if settings.remote_port == 0 {
        return Err(ValidationError::ZeroPort("remote port"));
    }
    if settings.local_port == 0 {
        return Err(ValidationError::ZeroPort("local port"));
    }
    Ok(())
}

pub fn validate_project(project: &Project) -> Result<(), ValidationError> {
    validate_project_name(&project.name)
}

/// One word: the filter splits on whitespace, so `@my shop` could never
/// select a project named "my shop". v1 allowed spaces; such names stay
/// readable, but are not written again.
pub fn validate_project_name(name: &str) -> Result<(), ValidationError> {
    if name.trim().is_empty() {
        return Err(ValidationError::Empty("project name"));
    }
    validate_text("project name", name)
}

/// Non-empty, without whitespace or control characters. Used for IDs and
/// context names, which may contain characters like `:`, `/` and `@`.
fn validate_text(field: &'static str, value: &str) -> Result<(), ValidationError> {
    if value.is_empty() {
        return Err(ValidationError::Empty(field));
    }
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ValidationError::InvalidCharacters {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

/// A namespace or service name: an RFC 1123 label, i.e. lowercase
/// alphanumerics and `-`, starting and ending with an alphanumeric.
fn validate_kubernetes_name(field: &'static str, value: &str) -> Result<(), ValidationError> {
    if value.is_empty() {
        return Err(ValidationError::Empty(field));
    }
    if value.len() > MAX_NAME_LEN {
        return Err(ValidationError::NameTooLong {
            field,
            value: value.to_owned(),
        });
    }
    let allowed = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
    if !value.chars().all(allowed) || value.starts_with('-') || value.ends_with('-') {
        return Err(ValidationError::InvalidName {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ForwardId;

    fn forward() -> Forward {
        Forward {
            id: ForwardId::new("staging.shop.frontend-80"),
            context: "arn:aws:eks:eu-central-1:123:cluster/stg".into(),
            namespace: "shop".into(),
            service: "frontend".into(),
            remote_port: 80,
            local_port: 18090,
            enabled: false,
        }
    }

    #[test]
    fn accepts_valid_forward() {
        assert_eq!(validate_forward(&forward()), Ok(()));
    }

    #[test]
    fn rejects_bad_names() {
        for (namespace, expected) in [
            ("", ValidationError::Empty("namespace")),
            (
                "Upper",
                ValidationError::InvalidName {
                    field: "namespace",
                    value: "Upper".into(),
                },
            ),
            (
                "-lead",
                ValidationError::InvalidName {
                    field: "namespace",
                    value: "-lead".into(),
                },
            ),
            (
                "trail-",
                ValidationError::InvalidName {
                    field: "namespace",
                    value: "trail-".into(),
                },
            ),
            (
                "under_score",
                ValidationError::InvalidName {
                    field: "namespace",
                    value: "under_score".into(),
                },
            ),
        ] {
            let f = Forward {
                namespace: namespace.into(),
                ..forward()
            };
            assert_eq!(
                validate_forward(&f),
                Err(expected),
                "namespace {namespace:?}"
            );
        }

        let long = "a".repeat(64);
        let f = Forward {
            service: long.clone(),
            ..forward()
        };
        assert_eq!(
            validate_forward(&f),
            Err(ValidationError::NameTooLong {
                field: "service",
                value: long
            })
        );
    }

    #[test]
    fn rejects_bad_context_and_id() {
        let f = Forward {
            context: "my context".into(),
            ..forward()
        };
        assert!(matches!(
            validate_forward(&f),
            Err(ValidationError::InvalidCharacters {
                field: "context",
                ..
            })
        ));
        let f = Forward {
            id: ForwardId::new(""),
            ..forward()
        };
        assert_eq!(validate_forward(&f), Err(ValidationError::Empty("id")));
    }

    #[test]
    fn rejects_zero_ports() {
        let f = Forward {
            local_port: 0,
            ..forward()
        };
        assert_eq!(
            validate_forward(&f),
            Err(ValidationError::ZeroPort("local port"))
        );
        let f = Forward {
            remote_port: 0,
            ..forward()
        };
        assert_eq!(
            validate_forward(&f),
            Err(ValidationError::ZeroPort("remote port"))
        );
    }

    #[test]
    fn validates_project_names() {
        let p = Project {
            name: "  ".into(),
            forwards: vec![],
        };
        assert_eq!(
            validate_project(&p),
            Err(ValidationError::Empty("project name"))
        );
        let p = Project {
            name: "shop".into(),
            forwards: vec![],
        };
        assert_eq!(validate_project(&p), Ok(()));
        assert_eq!(validate_project_name("shop-2:eu/web@x"), Ok(()));

        for name in [
            "my shop",
            " shop",
            "shop ",
            "my\u{a0}shop",
            "tab\tbed",
            "bell\u{7}",
        ] {
            assert_eq!(
                validate_project_name(name),
                Err(ValidationError::InvalidCharacters {
                    field: "project name",
                    value: name.into(),
                }),
                "{name:?}"
            );
        }
        assert_eq!(
            validate_project_name("my shop").unwrap_err().to_string(),
            r#"project name "my shop" contains whitespace or control characters"#
        );
    }
}
