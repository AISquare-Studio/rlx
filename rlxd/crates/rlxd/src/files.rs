//! Private files: written whole, 0600 on macOS and Linux.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// Writes `text` and a newline to `path` whole: a 0600 sibling, flushed to disk, renamed
/// over it.
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
    fs::rename(&temporary, path)
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

/// Removes `path`; a missing file is already removed.
pub fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}
