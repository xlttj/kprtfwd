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

/// The daemon's log files: `<data dir>/logs`.
pub fn log_dir() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("logs"))
}
