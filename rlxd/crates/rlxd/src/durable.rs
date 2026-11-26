//! Files written whole and durably: the bus's seq mark and the settings file. A write
//! goes to a 0600 sibling, is fsynced, then renamed over the file, and the rename is made
//! durable too: the folder is fsynced on POSIX, and Windows renames with
//! `MOVEFILE_WRITE_THROUGH`. So after a crash the file holds the old text or the new,
//! whole.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::registry::private_file;

/// Writes `text` to `path` whole and durably.
pub fn write(path: &Path, text: &[u8]) -> io::Result<()> {
    let temporary = sibling(path, "tmp");
    let mut file = private_file(&temporary)?;
    file.write_all(text)?;
    file.sync_all()?;
    drop(file);
    replace(&temporary, path)
}

/// Renames `from` over `to`, and returns once the rename is on disk: POSIX fsyncs the
/// folder after the rename.
#[cfg(unix)]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)?;
    let folder = to.parent().unwrap_or(Path::new("."));
    std::fs::File::open(folder)?.sync_all()
}

/// Renames `from` over `to`, and returns once the rename is on disk: Windows writes it
/// through. A rename another handle refuses for a moment is tried again.
#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    crate::registry::retry_refused(|| crate::windows::replace_durably(from, to))
}

/// `path` with `.extension` added to its name: `seq` gives `seq.tmp`.
pub fn sibling(path: &Path, extension: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(extension);
    path.with_file_name(name)
}
