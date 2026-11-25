//! Exclusive lock files, taken without blocking, with the primitive Python's
//! `aisquare.core.locking` takes (handoff 0.10.3, 4.1), so the two conflict:
//!
//! - **POSIX:** `flock(fd, LOCK_EX | LOCK_NB)`, which is what `File::try_lock` does there.
//! - **Windows:** `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY)` on one
//!   byte at offset 0, the byte Python's `msvcrt.locking(fd, LK_NBLCK, 1)` locks. The file
//!   is opened sharing read and write but never delete, so a held lock file can't be
//!   deleted under its holder.
//!
//! The OS drops a lock when its holder dies, however it dies.

use std::fs::File;
use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

/// A held lock. Dropping it lets the lock go.
#[derive(Debug)]
pub struct Held {
    file: File,
}

/// How often [`acquire`] asks again, as `core.locking` polls.
const POLL: Duration = Duration::from_millis(10);

/// Takes the lock file at `path`, created 0600 if missing, asking again for up to `wait`
/// while another process holds it. `None` when it was still held at the end.
pub fn acquire(path: &Path, wait: Duration) -> io::Result<Option<Held>> {
    let file = open(path, true)?;
    let deadline = Instant::now() + wait;
    loop {
        if os::try_lock(&file)? {
            return Ok(Some(Held { file }));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(POLL);
    }
}

/// Whether another process holds the lock file at `path` now. A missing file is not
/// held, and asking creates nothing. To ask, this takes the lock for an instant when it
/// is free, as Python's registry does.
pub fn is_held(path: &Path) -> io::Result<bool> {
    let file = match open(path, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let free = os::try_lock(&file)?;
    if free {
        os::unlock(&file);
    }
    Ok(!free)
}

impl Drop for Held {
    fn drop(&mut self) {
        os::unlock(&self.file);
    }
}

fn open(path: &Path, create: bool) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    options.open(path)
}

#[cfg(unix)]
mod os {
    use std::fs::{File, TryLockError};
    use std::io;

    /// Takes the lock if it is free: `flock(LOCK_EX | LOCK_NB)`.
    pub fn try_lock(file: &File) -> io::Result<bool> {
        match file.try_lock() {
            Ok(()) => Ok(true),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }

    pub fn unlock(file: &File) {
        let _ = file.unlock();
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod os {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, UnlockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    /// Takes the lock on byte 0 if it is free.
    pub fn try_lock(file: &File) -> io::Result<bool> {
        // An OVERLAPPED of zeros names offset 0.
        let mut at_zero = OVERLAPPED::default();
        // SAFETY: the handle is open while `file` is borrowed, and `at_zero` outlives the
        // call, which completes at once on a synchronous handle.
        let locked = unsafe {
            LockFileEx(
                file.as_raw_handle() as HANDLE,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut at_zero,
            )
        };
        if locked != 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
            Ok(false)
        } else {
            Err(error)
        }
    }

    pub fn unlock(file: &File) {
        let mut at_zero = OVERLAPPED::default();
        // SAFETY: as in `try_lock`. Unlocking a byte this handle does not hold fails
        // harmlessly.
        unsafe { UnlockFileEx(file.as_raw_handle() as HANDLE, 0, 1, 0, &mut at_zero) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rlxd-lock-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("x.lock")
    }

    #[test]
    fn a_held_lock_is_not_taken_again_until_it_is_let_go() {
        let path = scratch("held");
        let held = acquire(&path, Duration::ZERO)
            .unwrap()
            .expect("free at first");
        assert!(acquire(&path, Duration::from_millis(30)).unwrap().is_none());
        assert!(is_held(&path).unwrap());
        drop(held);
        assert!(!is_held(&path).unwrap());
        assert!(acquire(&path, Duration::ZERO).unwrap().is_some());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn asking_about_a_missing_lock_creates_nothing() {
        let path = scratch("missing");
        assert!(!is_held(&path).unwrap());
        assert!(!path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
