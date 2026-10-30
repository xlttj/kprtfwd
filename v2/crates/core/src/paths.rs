use std::path::PathBuf;

/// Directory for kprtfwd's data: `~/.kprtfwd`, the same as in v1.
///
/// Returns `None` if `HOME` is not set.
pub fn data_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".kprtfwd"))
}

/// Default database path: `~/.kprtfwd/kprtfwd.db`.
pub fn default_db_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("kprtfwd.db"))
}
