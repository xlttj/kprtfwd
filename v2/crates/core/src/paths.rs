use std::path::PathBuf;

/// Directory for kprtfwd's data: `$KPRTFWD_HOME` if set, else `~/.kprtfwd`
/// (the same as in v1).
///
/// Returns `None` if neither `KPRTFWD_HOME` nor `HOME` is set.
pub fn data_dir() -> Option<PathBuf> {
    let non_empty = |name| std::env::var_os(name).filter(|value| !value.is_empty());
    if let Some(dir) = non_empty("KPRTFWD_HOME") {
        return Some(PathBuf::from(dir));
    }
    non_empty("HOME").map(|home| PathBuf::from(home).join(".kprtfwd"))
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
