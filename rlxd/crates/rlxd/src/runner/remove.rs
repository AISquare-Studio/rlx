//! Removing what `space.erase` removes without ever following a link, even one put in
//! place after it was checked (handoff 0.10.3, 2.5).
//!
//! Each folder on the way is opened once, without following a link at its last component,
//! and the check and the removal both go through what was opened, never through the path
//! again:
//!
//! - **macOS and Linux:** every folder is a descriptor opened with `O_NOFOLLOW |
//!   O_DIRECTORY` relative to its parent's (`openat`), and files and folders are removed
//!   relative to it (`unlinkat`). A link put at a path that was checked is never reached.
//! - **Windows:** every folder on the way is a handle opened with
//!   `FILE_FLAG_OPEN_REPARSE_POINT`, checked through the handle, and held without
//!   `FILE_SHARE_DELETE` until the erase is done: while it is held, nothing can delete or
//!   rename that folder, so no link can take its place. What is removed below it is
//!   removed by std, which follows no reparse point.
//!
//! A root (`<home>`, a Claude Code config directory) is opened following links: it is
//! resolved once and taken as it is (manager #2548). Only what is below it is held to the
//! rule.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

/// What opening a folder below another found.
pub enum Opened {
    /// Nothing there.
    Missing,
    /// A link (a junction on Windows), which is not followed.
    Link,
    /// A folder, held.
    Dir(Dir),
}

#[cfg(unix)]
pub use unix::Dir;
#[cfg(windows)]
pub use windows::Dir;

#[cfg(unix)]
#[allow(unsafe_code)]
mod unix {
    use std::ffi::{CStr, CString, OsStr};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use super::Opened;

    /// A folder, held open by a descriptor.
    pub struct Dir {
        fd: OwnedFd,
        /// Its path, for what is reported.
        shown: PathBuf,
    }

    fn c_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
    }

    impl Dir {
        /// The folder `path`, following links: a root, taken as it resolves. `None` when
        /// it isn't there.
        pub fn open_root(path: &Path) -> io::Result<Option<Dir>> {
            let c = c_name(path.as_os_str())?;
            // SAFETY: open(2) of a NUL-terminated path; the descriptor is owned below.
            let fd = unsafe {
                libc::open(
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let e = io::Error::last_os_error();
                return match e.kind() {
                    io::ErrorKind::NotFound => Ok(None),
                    _ => Err(e),
                };
            }
            Ok(Some(Dir {
                // SAFETY: `fd` was just opened, and nothing else owns it.
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
                shown: path.to_path_buf(),
            }))
        }

        /// Its path.
        pub fn path(&self) -> &Path {
            &self.shown
        }

        /// The folder `name` in it, opened without following a link.
        pub fn child(&self, name: &OsStr) -> io::Result<Opened> {
            let c = c_name(name)?;
            // SAFETY: openat(2) relative to a held descriptor; the result is owned below.
            let fd = unsafe {
                libc::openat(
                    self.fd.as_raw_fd(),
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd >= 0 {
                return Ok(Opened::Dir(Dir {
                    // SAFETY: `fd` was just opened, and nothing else owns it.
                    fd: unsafe { OwnedFd::from_raw_fd(fd) },
                    shown: self.shown.join(name),
                }));
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::ENOENT) => Ok(Opened::Missing),
                Some(libc::ELOOP) => Ok(Opened::Link),
                // Not a folder: a link (some systems say ENOTDIR for O_NOFOLLOW) or a file.
                Some(libc::ENOTDIR) => {
                    if self.is_link(name)? {
                        Ok(Opened::Link)
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "{} is a file, not a folder",
                                self.shown.join(name).display()
                            ),
                        ))
                    }
                }
                _ => Err(e),
            }
        }

        /// Whether `name` in it is a link.
        fn is_link(&self, name: &OsStr) -> io::Result<bool> {
            let c = c_name(name)?;
            // SAFETY: stat is plain data, for which zero is a valid value.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: fstatat(2) relative to a held descriptor, not following a link.
            let done = unsafe {
                libc::fstatat(
                    self.fd.as_raw_fd(),
                    c.as_ptr(),
                    &raw mut stat,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if done != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(stat.st_mode & libc::S_IFMT == libc::S_IFLNK)
        }

        /// Removes the file (or link) `name` in it. Whether there was one.
        pub fn remove_file(&self, name: &OsStr) -> io::Result<bool> {
            let c = c_name(name)?;
            // SAFETY: unlinkat(2) relative to a held descriptor; a link goes, not what it
            // names.
            if unsafe { libc::unlinkat(self.fd.as_raw_fd(), c.as_ptr(), 0) } == 0 {
                return Ok(true);
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::ENOENT) => Ok(false),
                _ => Err(e),
            }
        }

        /// Removes the folder `name` in it and all it holds, following no link: each link
        /// inside goes as a link, and is added to `links`.
        pub fn remove_tree(&self, name: &OsStr, links: &mut Vec<PathBuf>) -> io::Result<()> {
            match self.child(name)? {
                Opened::Missing => return Ok(()),
                Opened::Link => {
                    self.remove_file(name)?;
                    links.push(self.shown.join(name));
                    return Ok(());
                }
                Opened::Dir(dir) => dir.empty(links)?,
            }
            let c = c_name(name)?;
            // SAFETY: unlinkat(2) of the emptied folder, relative to a held descriptor.
            if unsafe { libc::unlinkat(self.fd.as_raw_fd(), c.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// Removes all it holds.
        fn empty(&self, links: &mut Vec<PathBuf>) -> io::Result<()> {
            for name in self.names()? {
                let name = OsStr::from_bytes(name.to_bytes());
                if self.is_link(name)? {
                    self.remove_file(name)?;
                    links.push(self.shown.join(name));
                    continue;
                }
                match self.child(name) {
                    Ok(Opened::Dir(_)) => self.remove_tree(name, links)?,
                    Ok(Opened::Link) => {
                        self.remove_file(name)?;
                        links.push(self.shown.join(name));
                    }
                    Ok(Opened::Missing) => {}
                    // A file.
                    Err(_) => {
                        self.remove_file(name)?;
                    }
                }
            }
            Ok(())
        }

        /// The names in it, `.` and `..` aside.
        fn names(&self) -> io::Result<Vec<CString>> {
            // SAFETY: a duplicate of the held descriptor, which fdopendir then owns.
            let dup = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fdopendir(3) of a descriptor nothing else owns.
            let dir = unsafe { libc::fdopendir(dup) };
            if dir.is_null() {
                let e = io::Error::last_os_error();
                // SAFETY: the duplicate is still ours when fdopendir fails.
                unsafe { libc::close(dup) };
                return Err(e);
            }
            // fdopendir starts where the descriptor is; read from the start.
            // SAFETY: `dir` is a valid DIR*.
            unsafe { libc::rewinddir(dir) };
            let mut names = Vec::new();
            loop {
                // SAFETY: readdir(3) on a valid DIR*; the entry is read before the next call.
                let entry = unsafe { libc::readdir(dir) };
                if entry.is_null() {
                    break;
                }
                // SAFETY: d_name is NUL-terminated within the entry.
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
                if name.to_bytes() != b"." && name.to_bytes() != b".." {
                    names.push(name.to_owned());
                }
            }
            // SAFETY: closing the DIR* opened above, and its descriptor with it.
            unsafe { libc::closedir(dir) };
            Ok(names)
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
        OPEN_EXISTING,
    };

    use super::Opened;

    /// A folder, held by a handle that lets nothing delete or rename it meanwhile.
    pub struct Dir {
        path: PathBuf,
        _held: OwnedHandle,
    }

    /// `path` opened for its attributes, shared for reading and writing but not deleting,
    /// following a reparse point unless `follow` is false: its handle and attributes, or
    /// `None` when nothing is there.
    fn open(path: &Path, follow: bool) -> io::Result<Option<(OwnedHandle, u32)>> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
        if !follow {
            flags |= FILE_FLAG_OPEN_REPARSE_POINT;
        }
        // SAFETY: a NUL-terminated wide path; no security attributes or template.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                flags,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let e = io::Error::last_os_error();
            return match e.kind() {
                io::ErrorKind::NotFound => Ok(None),
                _ => Err(e),
            };
        }
        // SAFETY: `handle` was just opened, and nothing else owns it.
        let held = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `info` is writable, and the handle is open.
        if unsafe { GetFileInformationByHandle(held.as_raw_handle(), &raw mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some((held, info.dwFileAttributes)))
    }

    impl Dir {
        /// The folder `path`, following links: a root, taken as it resolves.
        pub fn open_root(path: &Path) -> io::Result<Option<Dir>> {
            Ok(open(path, true)?.map(|(held, _)| Dir {
                path: path.to_path_buf(),
                _held: held,
            }))
        }

        /// Its path.
        pub fn path(&self) -> &Path {
            &self.path
        }

        /// The folder `name` in it, its reparse point not followed.
        pub fn child(&self, name: &OsStr) -> io::Result<Opened> {
            let path = self.path.join(name);
            let Some((held, attributes)) = open(&path, false)? else {
                return Ok(Opened::Missing);
            };
            if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
                && std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink())
            {
                return Ok(Opened::Link);
            }
            if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is a file, not a folder", path.display()),
                ));
            }
            Ok(Opened::Dir(Dir { path, _held: held }))
        }

        /// Removes the file (or link) `name` in it. Whether there was one.
        pub fn remove_file(&self, name: &OsStr) -> io::Result<bool> {
            match std::fs::remove_file(self.path.join(name)) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e),
            }
        }

        /// Removes the folder `name` in it and all it holds, following no link: each link
        /// inside goes as a link, and is added to `links`. This folder stays held, so
        /// nothing can take `name`'s place by a rename above it.
        pub fn remove_tree(&self, name: &OsStr, links: &mut Vec<PathBuf>) -> io::Result<()> {
            let path = self.path.join(name);
            links.extend(crate::spaces::links_in(&path));
            match std::fs::remove_dir_all(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        }
    }
}

/// `path`'s last component, for a [`Dir`] call.
pub fn name_of(path: &Path) -> &OsStr {
    path.file_name().unwrap_or_default()
}

/// The folders on the way from `root` down `steps`, each opened without following a link:
/// all of them, or the path of the first that is a link, or `None` when one is missing.
pub fn walk(root: &Path, steps: &[&OsStr]) -> io::Result<Result<Option<Vec<Dir>>, PathBuf>> {
    let Some(mut dir) = Dir::open_root(root)? else {
        return Ok(Ok(None));
    };
    let mut held = Vec::new();
    for step in steps {
        match dir.child(step)? {
            Opened::Missing => return Ok(Ok(None)),
            Opened::Link => return Ok(Err(dir.path().join(step))),
            Opened::Dir(next) => {
                held.push(dir);
                dir = next;
            }
        }
    }
    held.push(dir);
    Ok(Ok(Some(held)))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rlxd-remove-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_tree_goes_with_its_links_as_links_and_their_targets_stay() {
        let root = temp("tree");
        let elsewhere = temp("tree-elsewhere");
        std::fs::write(elsewhere.join("keep"), "x").unwrap();
        let space = root.join("space");
        std::fs::create_dir_all(space.join("a").join("b")).unwrap();
        std::fs::write(space.join("a").join("b").join("f"), "y").unwrap();
        std::os::unix::fs::symlink(&elsewhere, space.join("a").join("out")).unwrap();
        std::os::unix::fs::symlink(elsewhere.join("keep"), space.join("file-link")).unwrap();
        let dir = Dir::open_root(&root).unwrap().unwrap();
        let mut links = Vec::new();
        dir.remove_tree(OsStr::new("space"), &mut links).unwrap();
        assert!(!space.exists());
        assert_eq!(links.len(), 2, "{links:?}");
        assert_eq!(
            std::fs::read_to_string(elsewhere.join("keep")).unwrap(),
            "x"
        );
    }

    #[test]
    fn a_walk_stops_at_a_link_and_names_it() {
        let root = temp("walk");
        let elsewhere = temp("walk-elsewhere");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("a").join("b")).unwrap();
        let found = walk(&root, &[OsStr::new("a"), OsStr::new("b")]).unwrap();
        assert_eq!(found.err(), Some(root.join("a").join("b")));
        let missing = walk(&root, &[OsStr::new("a"), OsStr::new("c")]).unwrap();
        assert!(matches!(missing, Ok(None)));
        let held = walk(&root, &[OsStr::new("a")]).unwrap().unwrap().unwrap();
        assert_eq!(held.len(), 2);
    }
}
