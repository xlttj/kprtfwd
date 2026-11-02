//! The user's settings in `config.toml`, next to the database.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Written when there is no file yet, so there is something to edit.
pub const TEMPLATE: &str = r#"# kprtfwd settings

# What quitting the TUI does to the forwards:
#   false  switch them all off, also those switched on with `kprtfwd up`,
#          as kprtfwd v1 did
#   true   keep them running in the background
keep_forwards_running = false
"#;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Keep forwards running after the last TUI quits.
    pub keep_forwards_running: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Reads the file; a missing file means the defaults. Unknown keys are an
    /// error, so a typo does not go unnoticed.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|err| ConfigError::Invalid {
                path: path.to_owned(),
                message: err.message().to_owned(),
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_owned(),
                source,
            }),
        }
    }

    /// Writes [`TEMPLATE`] to `path` unless the file exists. Returns whether
    /// it wrote it.
    pub fn write_template(path: &Path) -> io::Result<bool> {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                file.write_all(TEMPLATE.as_bytes())?;
                Ok(true)
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_holds_the_defaults() {
        assert_eq!(Config::parse(TEMPLATE).unwrap(), Config::default());
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn reads_settings() {
        let config = Config::parse("keep_forwards_running = true").unwrap();
        assert!(config.keep_forwards_running);
    }

    #[test]
    fn rejects_typos_and_wrong_types() {
        assert!(Config::parse("keep_forward_running = true").is_err());
        assert!(Config::parse("keep_forwards_running = \"yes\"").is_err());
    }

    #[test]
    fn loads_and_writes_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());

        assert!(Config::write_template(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), TEMPLATE);

        fs::write(&path, "keep_forwards_running = true\n").unwrap();
        assert!(
            !Config::write_template(&path).unwrap(),
            "overwrote the file"
        );
        assert!(Config::load(&path).unwrap().keep_forwards_running);

        fs::write(&path, "keep_forwards_running = 1\n").unwrap();
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(err.starts_with(&path.display().to_string()), "{err}");
    }
}
