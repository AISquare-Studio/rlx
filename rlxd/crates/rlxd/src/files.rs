//! Private files: written whole, 0600 on macOS and Linux.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// Writes `text` and a newline to `path` whole: a 0600 sibling, flushed to disk, renamed
/// over it, and the folder flushed too, so the rename itself survives a crash.
///
/// The sibling is made new, never opened through a link: whatever is at its name first
/// (one a crash left, or a link someone planted to redirect the write) is removed, which
/// removes a link and not what it points to, then it is created exclusively
/// (`O_CREAT|O_EXCL`, and `O_NOFOLLOW` on macOS and Linux). Should something take the
/// name in between, the write fails rather than follow it. Its mode is set on the open
/// file, and the rename replaces `path` itself, a link there included.
pub fn write_private(path: &Path, text: &str) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    match fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temporary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
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

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn a_link_planted_at_the_temporary_name_is_not_written_through() {
        let dir = std::env::temp_dir().join(format!("rlxd-files-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let victim = dir.join("victim");
        fs::write(&victim, "keep").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
        let path = dir.join("credential");
        std::os::unix::fs::symlink(&victim, path.with_extension("tmp")).unwrap();

        write_private(&path, "secret").unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&victim), 0o644, "the link's target was not chmodded");
        assert_eq!(fs::read_to_string(&path).unwrap(), "secret\n");
        assert_eq!(mode(&path), 0o600);
        assert!(fs::symlink_metadata(path.with_extension("tmp")).is_err());

        // A stale temporary file of a crash is replaced too.
        fs::write(path.with_extension("tmp"), "stale").unwrap();
        write_private(&path, "again").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "again\n");
        fs::remove_dir_all(&dir).unwrap();
    }
}
