use std::ffi::OsString;
use std::path::PathBuf;

/// kprtfwd's directory for its settings, database, socket and logs:
/// `$KPRTFWD_HOME` if set, else `$XDG_CONFIG_HOME/kprtfwd`, else
/// `~/.config/kprtfwd`. A relative `KPRTFWD_HOME` is made absolute against
/// the current directory, so the paths a client checks and shows are the
/// ones the daemon it starts uses, which runs elsewhere.
///
/// Returns `None` if neither `KPRTFWD_HOME` nor `HOME` is set.
pub fn data_dir() -> Option<PathBuf> {
    choose_dir(env("KPRTFWD_HOME"), env("XDG_CONFIG_HOME"), env("HOME"))
}

/// Where kprtfwd v1 and the first v2 builds kept their data: `~/.kprtfwd`.
/// `None` when `KPRTFWD_HOME` is set, so tests never touch it.
pub fn legacy_dir() -> Option<PathBuf> {
    if env("KPRTFWD_HOME").is_some() {
        return None;
    }
    env("HOME").map(|home| PathBuf::from(home).join(".kprtfwd"))
}

fn env(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

fn choose_dir(
    kprtfwd_home: Option<OsString>,
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(dir) = kprtfwd_home.map(PathBuf::from) {
        return Some(std::path::absolute(&dir).unwrap_or(dir));
    }
    // The XDG spec says to ignore relative paths.
    if let Some(dir) = xdg_config_home
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
    {
        return Some(dir.join("kprtfwd"));
    }
    home.map(|home| PathBuf::from(home).join(".config").join("kprtfwd"))
}

/// The settings: `<data dir>/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("config.toml"))
}

/// The database: `<data dir>/kprtfwd.db`.
pub fn default_db_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("kprtfwd.db"))
}

/// The daemon's Unix socket: `<data dir>/daemon.sock`.
pub fn socket_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("daemon.sock"))
}

/// The socket path, or why it cannot be used: no home directory, or too long
/// for a Unix socket (macOS allows 103 bytes, Linux 107).
pub fn usable_socket_path() -> Result<PathBuf, String> {
    const MAX: usize = 103;
    let path = socket_path().ok_or("neither KPRTFWD_HOME nor HOME is set")?;
    if path.as_os_str().len() > MAX {
        return Err(format!(
            "the socket path {} is too long for a Unix socket; set KPRTFWD_HOME to a shorter directory",
            path.display()
        ));
    }
    Ok(path)
}

/// The daemon's log files: `<data dir>/logs`.
pub fn log_dir() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("logs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(kprtfwd_home: Option<&str>, xdg: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
        choose_dir(
            kprtfwd_home.map(OsString::from),
            xdg.map(OsString::from),
            home.map(OsString::from),
        )
    }

    #[test]
    fn chooses_the_data_dir() {
        let path = |p: &str| Some(PathBuf::from(p));
        assert_eq!(
            dir(None, None, Some("/home/me")),
            path("/home/me/.config/kprtfwd")
        );
        assert_eq!(
            dir(None, Some("/xdg"), Some("/home/me")),
            path("/xdg/kprtfwd")
        );
        assert_eq!(
            dir(None, Some("relative"), Some("/home/me")),
            path("/home/me/.config/kprtfwd")
        );
        assert_eq!(
            dir(Some("/test"), Some("/xdg"), Some("/home/me")),
            path("/test")
        );
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            dir(Some("kh"), Some("/xdg"), Some("/home/me")),
            Some(cwd.join("kh"))
        );
        assert_eq!(dir(None, None, None), None);
    }
}
