//! Files written whole and durably: the bus's seq mark, the settings file, and a run's
//! record and settings. A write goes to a 0600 sibling of a random name, made new
//! (`O_CREAT | O_EXCL | O_NOFOLLOW`; on Windows `CREATE_NEW`, no reparse point followed),
//! so nothing put at a sibling's name beforehand (a FIFO, a link) is ever opened or
//! written through. It is fsynced, then renamed over the file, and the rename is made
//! durable too: the folder is fsynced on POSIX, and Windows renames with
//! `MOVEFILE_WRITE_THROUGH`. So after a crash the file holds the old text or the new,
//! whole.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Writes `text` to `path` whole and durably. An error says which file it was about,
/// `path` or its temporary sibling (`<name>.<random>.tmp`).
pub fn write(path: &Path, text: &[u8]) -> io::Result<()> {
    let about = |file: &Path| {
        let file = file.display().to_string();
        move |e: io::Error| io::Error::new(e.kind(), format!("{file}: {e}"))
    };
    let (temporary, mut file) = temporary(path).map_err(about(path))?;
    let written = file
        .write_all(text)
        .and_then(|()| file.sync_all())
        .map_err(about(&temporary));
    drop(file);
    let replaced = written.and_then(|()| replace(&temporary, path).map_err(about(path)));
    if replaced.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    replaced
}

/// A new 0600 sibling of `path` to write, `<name>.<random>.tmp`, and its path. It is
/// made, never opened: a name that is taken, by a file, a FIFO or a link, is passed over
/// for another.
fn temporary(path: &Path) -> io::Result<(PathBuf, File)> {
    let mut last = None;
    for _ in 0..8 {
        let mut bytes = [0_u8; 8];
        getrandom::fill(&mut bytes)
            .map_err(|e| io::Error::other(format!("no random bytes: {e}")))?;
        let random: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let temporary = sibling(path, &format!("{random}.tmp"));
        match create_new(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("{}: {e}", temporary.display()),
                ));
            }
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no free temporary name")))
}

/// Makes `path`, a new file private to the user, for writing: it fails `AlreadyExists`
/// when anything is at `path`, and never follows a link there.
pub(crate) fn create_new(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        // The umask may have taken bits off 0600, never added any; make it 0600 exactly.
        use std::os::unix::fs::PermissionsExt;
        if file.metadata()?.permissions().mode() & 0o777 != 0o600 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(file)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_stale_sibling_left_0644_still_gives_a_0600_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rlxd-durable-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let stale = sibling(&path, "tmp");
        std::fs::write(&stale, "left by a crash").unwrap();
        std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o644)).unwrap();
        write(&path, b"{}\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_write_names_the_file_it_failed_on() {
        let dir = std::env::temp_dir().join(format!("rlxd-durable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Its folder is not there: the temporary sibling can't be made.
        let error = write(&dir.join("gone").join("seq"), b"1\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("seq"), "{error}");
        // A folder where the file goes: the rename fails, naming it, and the temporary
        // sibling goes.
        std::fs::create_dir_all(dir.join("seq").join("in")).unwrap();
        let error = write(&dir.join("seq"), b"1\n").unwrap_err().to_string();
        assert!(error.contains("seq"), "{error}");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(left.len(), 1, "{left:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn a_fifo_or_a_link_at_a_temporary_name_is_never_opened() {
        let dir = std::env::temp_dir().join(format!("rlxd-durable-planted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let outside = dir.join("outside");
        std::fs::write(&outside, "keep\n").unwrap();
        let fifo = dir.join("fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let link = dir.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        // Made new or not at all: a FIFO is not opened (which would wait for a reader),
        // and a link is not written through.
        for planted in [&fifo, &link] {
            let refused = create_new(planted).unwrap_err();
            assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists, "{planted:?}");
        }
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep\n");
        // The old fixed names, planted: a write goes past them, and its file is its own.
        let settings = dir.join("settings.json");
        let made = std::process::Command::new("mkfifo")
            .arg(sibling(&settings, "tmp"))
            .status()
            .unwrap();
        assert!(made.success());
        let record = dir.join("run.json");
        std::os::unix::fs::symlink(&outside, sibling(&record, "tmp")).unwrap();
        write(&settings, b"{}\n").unwrap();
        write(&record, b"{}\n").unwrap();
        for file in [&settings, &record] {
            let kind = std::fs::symlink_metadata(file).unwrap().file_type();
            assert!(kind.is_file(), "{file:?} is {kind:?}");
            assert_eq!(std::fs::read_to_string(file).unwrap(), "{}\n");
        }
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn a_link_at_a_temporary_name_is_never_written_through() {
        let dir = std::env::temp_dir().join(format!("rlxd-durable-planted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let outside = dir.join("outside");
        std::fs::write(&outside, "keep\n").unwrap();
        let link = dir.join("link");
        // Making a link needs a privilege a test may not have: then only the file case runs.
        if std::os::windows::fs::symlink_file(&outside, &link).is_ok() {
            let refused = create_new(&link).unwrap_err();
            assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
        }
        let refused = create_new(&outside).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
