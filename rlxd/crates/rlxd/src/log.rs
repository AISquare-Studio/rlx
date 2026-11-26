//! rlxd's log, `<home>/rlxd/logs/rlxd.log` (handoff 0.10.3, 1.3): one line per entry,
//! `<RFC 3339 time> <level> <message>`. At [`MAX_BYTES`] it rotates: `rlxd.log` becomes
//! `rlxd.log.1`, the older ones move up one, and [`KEEP`] are kept.
//!
//! A foreground `rlxd serve` also writes each line to stderr. Until [`open`] is called,
//! lines go to stderr only.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::clock;

/// The size at which the log rotates: 5 MiB.
pub const MAX_BYTES: u64 = 5 << 20;

/// How many rotated logs are kept: `rlxd.log.1` to `rlxd.log.3`.
pub const KEEP: u32 = 3;

struct Log {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    max: u64,
    echo: bool,
}

static LOG: OnceLock<Mutex<Log>> = OnceLock::new();

/// Logs to `path` from here on, created 0600, and to stderr as well when `echo`.
pub fn open(path: &Path, echo: bool) -> io::Result<()> {
    let log = Log::open(path, MAX_BYTES, echo)?;
    if LOG.set(Mutex::new(log)).is_err() {
        return Err(io::Error::other("the log is already open"));
    }
    Ok(())
}

/// Writes one entry.
pub fn write(level: &str, message: fmt::Arguments<'_>) {
    let line = format!("{} {level} {message}\n", clock::now());
    let Some(log) = LOG.get() else {
        let _ = io::stderr().write_all(line.as_bytes());
        return;
    };
    let mut log = log.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    log.write_line(&line);
}

impl Log {
    fn open(path: &Path, max: u64, echo: bool) -> io::Result<Self> {
        let file = append(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            file: Some(file),
            size,
            max,
            echo,
        })
    }

    fn write_line(&mut self, line: &str) {
        if self.echo {
            let _ = io::stderr().write_all(line.as_bytes());
        }
        let len = line.len() as u64;
        if self.size > 0 && self.size + len > self.max {
            self.rotate();
        }
        if let Some(file) = self.file.as_mut()
            && file.write_all(line.as_bytes()).is_ok()
        {
            self.size += len;
        }
    }

    /// Moves `rlxd.log.N` to `.N+1` (the oldest dropped) and `rlxd.log` to `.1`, then
    /// starts a new `rlxd.log`. A step that fails leaves the log as it is.
    fn rotate(&mut self) {
        self.file = None;
        let numbered = |n: u32| {
            let mut name = self.path.as_os_str().to_owned();
            name.push(format!(".{n}"));
            PathBuf::from(name)
        };
        let _ = fs::remove_file(numbered(KEEP));
        for n in (1..KEEP).rev() {
            let _ = fs::rename(numbered(n), numbered(n + 1));
        }
        let _ = fs::rename(&self.path, numbered(1));
        self.file = append(&self.path).ok();
        self.size = self
            .file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map_or(0, |m| m.len());
    }
}

fn append(path: &Path) -> io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Logs an entry at level `info`.
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::write("info", format_args!($($arg)*)) };
}

/// Logs an entry at level `warn`.
macro_rules! warning {
    ($($arg:tt)*) => { $crate::log::write("warn", format_args!($($arg)*)) };
}

/// Logs an entry at level `error`.
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::write("error", format_args!($($arg)*)) };
}

pub(crate) use {error, info, warning};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_rotates_at_its_size_and_keeps_three() {
        let dir = std::env::temp_dir().join(format!("rlxd-log-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rlxd.log");
        let mut log = Log::open(&path, 20, false).unwrap();
        for n in 0..6 {
            log.write_line(&format!("entry {n} ....\n"));
        }
        let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap();
        assert_eq!(read("rlxd.log"), "entry 5 ....\n");
        assert_eq!(read("rlxd.log.1"), "entry 4 ....\n");
        assert_eq!(read("rlxd.log.3"), "entry 2 ....\n");
        assert!(!dir.join("rlxd.log.4").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
