//! The daemon's log files as `kprtfwd logs` reads them: the newest one, and
//! with `-f` what is appended, across the daily switch to a new file.
//!
//! Lines are passed on as bytes, as written; nothing is decoded, so a read
//! that ends inside a multi-byte character does no harm.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The newest of the daemon's log files in `dir`. They are named
/// `daemon.<date>.log`, so the newest has the greatest name.
pub fn newest(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("daemon.") && name.ends_with(".log"))
        })
        .max()
}

/// The last `n` lines of `text`.
pub fn last_lines(text: &[u8], n: usize) -> &[u8] {
    if n == 0 {
        return &[];
    }
    let body = text.strip_suffix(b"\n").unwrap_or(text);
    let start = body
        .iter()
        .enumerate()
        .rev()
        .filter(|&(_, &byte)| byte == b'\n')
        .nth(n - 1)
        .map_or(0, |(newline, _)| newline + 1);
    &text[start..]
}

/// Reads what the daemon writes to its log, in whole lines.
pub struct Follower {
    dir: PathBuf,
    file: PathBuf,
    /// How many bytes of `file` have been read.
    position: u64,
    /// The start of a line whose end has not been read yet.
    partial: Vec<u8>,
}

impl Follower {
    /// Starts at the beginning of the newest log file in `dir`; `None` if
    /// there is none.
    pub fn new(dir: &Path) -> Option<Self> {
        Some(Self {
            dir: dir.to_owned(),
            file: newest(dir)?,
            position: 0,
            partial: Vec::new(),
        })
    }

    /// The whole lines written since the last call. When the daemon has
    /// switched to a newer file, the rest of the old one comes first.
    pub fn read(&mut self) -> io::Result<Vec<u8>> {
        let mut lines = Vec::new();
        self.read_file(&mut lines)?;
        if let Some(newer) = newest(&self.dir).filter(|newest| *newest > self.file) {
            // The daemon writes no more to the old file: a line it left
            // without its end stays so.
            lines.append(&mut self.rest());
            self.file = newer;
            self.position = 0;
            self.read_file(&mut lines)?;
        }
        Ok(lines)
    }

    /// The start of a line whose end has not been read yet, with a newline;
    /// empty if there is none.
    pub fn rest(&mut self) -> Vec<u8> {
        let mut rest = std::mem::take(&mut self.partial);
        if !rest.is_empty() {
            rest.push(b'\n');
        }
        rest
    }

    /// Appends to `lines` the whole lines of the current file from where the
    /// last read ended. The position moves by the bytes actually read, also
    /// those written after the read began.
    fn read_file(&mut self, lines: &mut Vec<u8>) -> io::Result<()> {
        let mut file = match File::open(&self.file) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err),
        };
        if file.metadata()?.len() < self.position {
            // Truncated or replaced: start over.
            self.position = 0;
            self.partial.clear();
        }
        file.seek(SeekFrom::Start(self.position))?;
        let read = file.read_to_end(&mut self.partial)?;
        self.position += read as u64;
        if let Some(end) = self.partial.iter().rposition(|&byte| byte == b'\n') {
            lines.extend(self.partial.drain(..=end));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn append(path: &Path, bytes: &[u8]) {
        File::options()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    fn text(bytes: &[u8]) -> &str {
        std::str::from_utf8(bytes).unwrap()
    }

    #[test]
    fn the_newest_file_is_the_latest_day() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(newest(dir.path()), None);
        for name in [
            "daemon.2026-11-06.log",
            "daemon.2026-11-07.log",
            "daemon.stderr",
            "other.2026-11-08.log",
        ] {
            append(&dir.path().join(name), b"");
        }
        assert_eq!(
            newest(dir.path()),
            Some(dir.path().join("daemon.2026-11-07.log"))
        );
    }

    #[test]
    fn the_last_lines() {
        assert_eq!(text(last_lines(b"a\nb\nc\n", 2)), "b\nc\n");
        assert_eq!(text(last_lines(b"a\nb\nc", 2)), "b\nc");
        assert_eq!(text(last_lines(b"a\nb\n", 5)), "a\nb\n");
        assert_eq!(text(last_lines(b"a\n", 0)), "");
        assert_eq!(text(last_lines(b"", 3)), "");
    }

    #[test]
    fn whole_lines_once_also_when_a_character_is_split() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("daemon.2026-11-07.log");
        append(&log, b"one\ntw");
        let mut follower = Follower::new(dir.path()).unwrap();
        assert_eq!(text(&follower.read().unwrap()), "one\n");
        assert_eq!(text(&follower.read().unwrap()), "");

        // "ö" is two bytes; the line ends with the second.
        let ouml = "ö".as_bytes();
        append(&log, b"o\nsch");
        append(&log, &ouml[..1]);
        assert_eq!(text(&follower.read().unwrap()), "two\n");
        append(&log, &ouml[1..]);
        append(&log, b"n\n");
        assert_eq!(text(&follower.read().unwrap()), "schön\n");
        assert_eq!(text(&follower.read().unwrap()), "");
        assert_eq!(follower.rest(), b"");
    }

    #[test]
    fn the_old_file_is_read_to_its_end_before_the_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("daemon.2026-11-06.log");
        append(&old, b"before\n");
        let mut follower = Follower::new(dir.path()).unwrap();
        assert_eq!(text(&follower.read().unwrap()), "before\n");

        append(&old, b"last of the day\nunfinished");
        append(
            &dir.path().join("daemon.2026-11-07.log"),
            b"first of the day\n",
        );
        assert_eq!(
            text(&follower.read().unwrap()),
            "last of the day\nunfinished\nfirst of the day\n"
        );
        // The old file is not read again.
        append(&old, b"late\n");
        assert_eq!(text(&follower.read().unwrap()), "");
    }

    #[test]
    fn a_truncated_file_is_read_from_its_start() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("daemon.2026-11-07.log");
        append(&log, b"a long first line\n");
        let mut follower = Follower::new(dir.path()).unwrap();
        follower.read().unwrap();
        std::fs::write(&log, b"new\n").unwrap();
        assert_eq!(text(&follower.read().unwrap()), "new\n");
    }

    #[test]
    fn the_rest_of_an_unfinished_line_ends_with_a_newline() {
        let dir = tempfile::tempdir().unwrap();
        append(&dir.path().join("daemon.2026-11-07.log"), b"a\nb");
        let mut follower = Follower::new(dir.path()).unwrap();
        assert_eq!(text(&follower.read().unwrap()), "a\n");
        assert_eq!(text(&follower.rest()), "b\n");
        assert_eq!(text(&follower.rest()), "");
    }
}
