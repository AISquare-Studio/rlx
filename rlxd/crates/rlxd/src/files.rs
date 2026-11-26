//! Private files: written whole, 0600 on macOS and Linux.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// Writes `text` and a newline to `path` whole: a 0600 sibling, flushed to disk, renamed
/// over it, and the folder flushed too, so the rename itself survives a crash.
pub fn write_private(path: &Path, text: &str) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    restrict(&temporary)?;
    file.write_all(text.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    sync_dir(path)
}

/// Flushes the folder holding `path`, so a rename or removal in it is on disk.
#[cfg(unix)]
fn sync_dir(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => fs::File::open(dir)?.sync_all(),
        _ => Ok(()),
    }
}

/// Windows has no folder flush that std offers; NTFS journals the rename.
#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Narrows `path` to 0600 when it is looser.
#[cfg(unix)]
pub fn restrict(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)?.permissions().mode() & 0o777;
    if mode != 0o600 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// On Windows a file takes its folder's ACL, which is the user's profile's.
#[cfg(not(unix))]
pub fn restrict(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Removes `path`, and flushes its folder; a missing file is already removed.
pub fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_dir(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
