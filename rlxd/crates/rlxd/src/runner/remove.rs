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
//!   A folder on another filesystem than its parent's, or (Linux) on another mount of the
//!   same one, a bind mount, is a mount point, and is not gone into.
//!   The removal walks the tree with a stack of its own, at most [`MAX_DEPTH`] folders
//!   deep, so no tree is deep enough to overflow a thread's stack.
//! - **Windows:** every folder on the way is a handle opened with
//!   `FILE_FLAG_OPEN_REPARSE_POINT`, checked through the handle, and held without
//!   `FILE_SHARE_DELETE` until the erase is done: while it is held, nothing can delete or
//!   rename that folder, so no link can take its place. What is removed below it is
//!   removed by std, which follows no reparse point (a volume mount point is one).
//!
//! A root (`<home>`, a Claude Code config directory) is opened following links: it is
//! resolved once, and everything below it goes by what it resolved to (manager #2548).
//! Only what is below it is held to the rule.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

/// How many folders deep a removal goes below the folder it removes; deeper, it stops
/// and fails, naming where. (On Windows std removes the tree, with no recursion.)
#[cfg_attr(windows, allow(dead_code))]
pub const MAX_DEPTH: usize = 256;

/// What opening a folder below another found.
pub enum Opened {
    /// Nothing there.
    Missing,
    /// A link (a junction on Windows), which is not followed.
    Link,
    /// A folder on another filesystem (a mount point), which is not gone into. (On
    /// Windows a volume mount point is a junction, so a [`Opened::Link`].)
    #[cfg_attr(windows, allow(dead_code))]
    Mount,
    /// A file, not a folder.
    File,
    /// A folder, held.
    Dir(Dir),
}

/// A removal that failed: where, and why.
#[derive(Debug)]
pub struct Failed {
    pub path: PathBuf,
    pub error: io::Error,
}

impl Failed {
    /// `error`, at `path`.
    #[cfg_attr(windows, allow(dead_code))]
    fn at(path: PathBuf) -> impl FnOnce(io::Error) -> Failed {
        move |error| Failed { path, error }
    }

    /// A mount point at `path`, which a removal doesn't go into.
    #[cfg_attr(windows, allow(dead_code))]
    fn mount(path: PathBuf) -> Self {
        Failed {
            path,
            error: io::Error::other(
                "another filesystem is mounted here, and rlxd does not erase into it: \
                 unmount it first",
            ),
        }
    }
}

#[cfg(unix)]
pub use unix::Dir;
#[cfg(windows)]
pub use windows::Dir;

#[cfg(unix)]
#[allow(unsafe_code)]
mod unix {
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use super::{Failed, MAX_DEPTH, Opened};

    /// A folder, held open by a descriptor.
    pub struct Dir {
        fd: OwnedFd,
        /// Its path, for what is reported.
        shown: PathBuf,
        /// The filesystem it is on.
        dev: libc::dev_t,
        /// The mount it is on (Linux, `statx`'s `STATX_MNT_ID`): a bind mount of a folder of
        /// the same filesystem has its `dev`, and another mount.
        mount: Option<u64>,
    }

    fn c_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
    }

    /// `fd`, just opened, as a [`Dir`] at `shown`.
    fn held(fd: libc::c_int, shown: PathBuf) -> io::Result<Dir> {
        // SAFETY: `fd` was just opened, and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: stat is plain data, for which zero is a valid value.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fstat(2) of a held descriptor into a writable stat.
        if unsafe { libc::fstat(fd.as_raw_fd(), &raw mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mount = mount_of(&fd);
        Ok(Dir {
            fd,
            shown,
            dev: stat.st_dev,
            mount,
        })
    }

    /// The id of the mount `fd` is on: `statx`'s `STATX_MNT_ID` (Linux 5.8 and on), or
    /// `None` where there is none to read.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    fn mount_of(fd: &OwnedFd) -> Option<u64> {
        // SAFETY: statx is plain data, for which zero is a valid value.
        let mut stx: libc::statx = unsafe { std::mem::zeroed() };
        // SAFETY: statx(2) of a held descriptor itself (AT_EMPTY_PATH, an empty path) into
        // a writable statx.
        let done = unsafe {
            libc::statx(
                fd.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH,
                libc::STATX_MNT_ID,
                &raw mut stx,
            )
        };
        (done == 0 && stx.stx_mask & libc::STATX_MNT_ID != 0).then_some(stx.stx_mnt_id)
    }

    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    fn mount_of(_fd: &OwnedFd) -> Option<u64> {
        None
    }

    /// One level of a removal: a folder being emptied, its name in its parent, and the
    /// names in it still to remove.
    struct Level {
        name: OsString,
        dir: Dir,
        names: Vec<CString>,
    }

    impl Dir {
        /// The folder `path`, following links: a root, taken as it resolves. `None` when
        /// it isn't there.
        pub fn open_root(path: &Path) -> io::Result<Option<Dir>> {
            let c = c_name(path.as_os_str())?;
            // SAFETY: open(2) of a NUL-terminated path; the descriptor is owned by held().
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
            held(fd, path.to_path_buf()).map(Some)
        }

        /// Its path.
        pub fn path(&self) -> &Path {
            &self.shown
        }

        /// The folder `name` in it, opened without following a link, and only on its own
        /// filesystem.
        pub fn child(&self, name: &OsStr) -> io::Result<Opened> {
            let c = c_name(name)?;
            // SAFETY: openat(2) relative to a held descriptor; the result is owned by
            // held().
            let fd = unsafe {
                libc::openat(
                    self.fd.as_raw_fd(),
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd >= 0 {
                let dir = held(fd, self.shown.join(name))?;
                let other_mount =
                    matches!((dir.mount, self.mount), (Some(it), Some(parent)) if it != parent);
                if dir.dev != self.dev || other_mount {
                    return Ok(Opened::Mount);
                }
                return Ok(Opened::Dir(dir));
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::ENOENT) => Ok(Opened::Missing),
                Some(libc::ELOOP) => Ok(Opened::Link),
                // Not a folder: a link (some systems say ENOTDIR for O_NOFOLLOW) or a file.
                Some(libc::ENOTDIR) => match self.kind(name)? {
                    Some(libc::S_IFLNK) => Ok(Opened::Link),
                    Some(_) => Ok(Opened::File),
                    None => Ok(Opened::Missing),
                },
                _ => Err(e),
            }
        }

        /// What `name` in it is (`S_IFMT` of its mode, a link not followed), or `None` when
        /// nothing is there.
        fn kind(&self, name: &OsStr) -> io::Result<Option<libc::mode_t>> {
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
                let e = io::Error::last_os_error();
                return match e.raw_os_error() {
                    Some(libc::ENOENT) => Ok(None),
                    _ => Err(e),
                };
            }
            Ok(Some(stat.st_mode & libc::S_IFMT))
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

        /// Removes the empty folder `name` in it. A link put there meanwhile is not
        /// followed: it fails with ENOTDIR.
        fn remove_dir(&self, name: &OsStr) -> io::Result<()> {
            let c = c_name(name)?;
            // SAFETY: unlinkat(2) of a folder, relative to a held descriptor.
            if unsafe { libc::unlinkat(self.fd.as_raw_fd(), c.as_ptr(), libc::AT_REMOVEDIR) } == 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::ENOENT) => Ok(()),
                _ => Err(e),
            }
        }

        /// Removes the folder `name` in it and all it holds, following no link and going
        /// into no mount point: each link inside goes as a link, and is added to `links`.
        /// It walks the tree with a stack of its own, at most [`MAX_DEPTH`] deep. What
        /// failed, where, stops it.
        pub fn remove_tree(&self, name: &OsStr, links: &mut Vec<PathBuf>) -> Result<(), Failed> {
            let path = self.shown.join(name);
            let top = match self.child(name).map_err(Failed::at(path.clone()))? {
                Opened::Missing => return Ok(()),
                Opened::Link => {
                    self.remove_file(name).map_err(Failed::at(path.clone()))?;
                    links.push(path);
                    return Ok(());
                }
                Opened::File => {
                    self.remove_file(name).map_err(Failed::at(path))?;
                    return Ok(());
                }
                Opened::Mount => return Err(Failed::mount(path)),
                Opened::Dir(dir) => dir,
            };
            let names = top.names().map_err(Failed::at(path))?;
            let mut stack = vec![Level {
                name: name.to_owned(),
                dir: top,
                names,
            }];
            while let Some(level) = stack.last_mut() {
                let Some(entry) = level.names.pop() else {
                    // Emptied: it goes, from its parent.
                    let Some(done) = stack.pop() else { break };
                    let parent = stack.last().map_or(self, |up| &up.dir);
                    parent
                        .remove_dir(&done.name)
                        .map_err(Failed::at(done.dir.shown.clone()))?;
                    continue;
                };
                let name = OsStr::from_bytes(entry.to_bytes());
                let path = level.dir.shown.join(name);
                let opened = match level.dir.kind(name).map_err(Failed::at(path.clone()))? {
                    // Gone meanwhile.
                    None => continue,
                    Some(libc::S_IFDIR) => match level.dir.child(name) {
                        Ok(opened) => opened,
                        // A folder it may not read (mode 000, say) goes if it is empty;
                        // else what failed is said of it.
                        Err(e) => {
                            if level.dir.remove_dir(name).is_ok() {
                                continue;
                            }
                            return Err(Failed { path, error: e });
                        }
                    },
                    Some(libc::S_IFLNK) => Opened::Link,
                    Some(_) => Opened::File,
                };
                match opened {
                    Opened::Missing => {}
                    Opened::Link => {
                        level
                            .dir
                            .remove_file(name)
                            .map_err(Failed::at(path.clone()))?;
                        links.push(path);
                    }
                    Opened::File => {
                        level.dir.remove_file(name).map_err(Failed::at(path))?;
                    }
                    Opened::Mount => return Err(Failed::mount(path)),
                    Opened::Dir(dir) => {
                        if stack.len() >= MAX_DEPTH {
                            return Err(Failed {
                                path,
                                error: io::Error::other(format!(
                                    "the tree goes deeper than {MAX_DEPTH} folders, and rlxd \
                                     stops here: remove it by hand"
                                )),
                            });
                        }
                        let names = dir.names().map_err(Failed::at(path))?;
                        stack.push(Level {
                            name: name.to_owned(),
                            dir,
                            names,
                        });
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
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileAttributeTagInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx, GetFinalPathNameByHandleW, OPEN_EXISTING, VOLUME_NAME_DOS,
    };

    use super::{Failed, Opened};

    /// A reparse tag's name-surrogate bit: a link of some kind (a symbolic link, a
    /// junction, a volume mount point), as std's `is_symlink` reads it.
    const NAME_SURROGATE: u32 = 0x2000_0000;

    /// A folder, held by a handle that lets nothing delete or rename it meanwhile.
    pub struct Dir {
        /// Its path below the resolved root, which every call goes by.
        by: PathBuf,
        /// Its path as it is shown, without the verbatim prefix.
        shown: PathBuf,
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

    /// The path `held` was opened at, as the OS resolved it (`\\?\C:\…`).
    fn final_path(held: &OwnedHandle) -> io::Result<PathBuf> {
        let mut wide = vec![0_u16; 512];
        loop {
            let size = u32::try_from(wide.len()).unwrap_or(u32::MAX);
            // SAFETY: `wide` is writable for `size` wide characters; the handle is open.
            let len = unsafe {
                GetFinalPathNameByHandleW(
                    held.as_raw_handle(),
                    wide.as_mut_ptr(),
                    size,
                    FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
                )
            };
            let len = usize::try_from(len).unwrap_or(0);
            if len == 0 {
                return Err(io::Error::last_os_error());
            }
            if len < wide.len() {
                wide.truncate(len);
                return Ok(PathBuf::from(std::ffi::OsString::from_wide(&wide)));
            }
            // Too small: `len` is the size it needs.
            wide.resize(len + 1, 0);
        }
    }

    /// Whether the reparse point `held` is a link of some kind (a name surrogate).
    fn is_link(held: &OwnedHandle) -> io::Result<bool> {
        let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
        let size = u32::try_from(std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>()).unwrap_or(0);
        // SAFETY: `tag` is a writable FILE_ATTRIBUTE_TAG_INFO of `size`; the handle is open.
        let read = unsafe {
            GetFileInformationByHandleEx(
                held.as_raw_handle(),
                FileAttributeTagInfo,
                (&raw mut tag).cast(),
                size,
            )
        };
        if read == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(tag.ReparseTag & NAME_SURROGATE != 0)
    }

    impl Dir {
        /// The folder `path`, following links: a root, taken as it resolves, once. All
        /// below it goes by the path its handle resolved to, so a link at `path`
        /// retargeted meanwhile changes nothing.
        pub fn open_root(path: &Path) -> io::Result<Option<Dir>> {
            let Some((held, _)) = open(path, true)? else {
                return Ok(None);
            };
            let resolved = final_path(&held)?;
            Ok(Some(Dir {
                shown: crate::spaces::plain(&resolved),
                by: resolved,
                _held: held,
            }))
        }

        /// Its path.
        pub fn path(&self) -> &Path {
            &self.shown
        }

        /// The folder `name` in it, its reparse point not followed.
        pub fn child(&self, name: &OsStr) -> io::Result<Opened> {
            let path = self.by.join(name);
            let Some((held, attributes)) = open(&path, false)? else {
                return Ok(Opened::Missing);
            };
            if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && is_link(&held)? {
                return Ok(Opened::Link);
            }
            if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
                return Ok(Opened::File);
            }
            Ok(Opened::Dir(Dir {
                shown: self.shown.join(name),
                by: path,
                _held: held,
            }))
        }

        /// Removes the file (or link) `name` in it. Whether there was one.
        pub fn remove_file(&self, name: &OsStr) -> io::Result<bool> {
            match std::fs::remove_file(self.by.join(name)) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e),
            }
        }

        /// Removes the folder `name` in it and all it holds, following no link: each link
        /// inside goes as a link, and is added to `links`. This folder stays held, so
        /// nothing can take `name`'s place by a rename above it.
        pub fn remove_tree(&self, name: &OsStr, links: &mut Vec<PathBuf>) -> Result<(), Failed> {
            let path = self.by.join(name);
            let shown = self.shown.join(name);
            links.extend(crate::spaces::links_in(&path).iter().map(|link| {
                match link.strip_prefix(&path) {
                    Ok(rest) => shown.join(rest),
                    Err(_) => crate::spaces::plain(link),
                }
            }));
            match std::fs::remove_dir_all(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(Failed {
                    path: shown,
                    error: e,
                }),
            }
        }
    }
}

/// `path`'s last component, for a [`Dir`] call.
pub fn name_of(path: &Path) -> &OsStr {
    path.file_name().unwrap_or_default()
}

/// The folders on the way from `root` down `steps`, each opened without following a link
/// or going into a mount point: all of them, or the path of the first that is a link or a
/// mount point, or `None` when one is missing.
pub fn walk(root: &Path, steps: &[&OsStr]) -> io::Result<Result<Option<Vec<Dir>>, PathBuf>> {
    let Some(mut dir) = Dir::open_root(root)? else {
        return Ok(Ok(None));
    };
    let mut held = Vec::new();
    for step in steps {
        match dir.child(step)? {
            Opened::Missing => return Ok(Ok(None)),
            Opened::Link | Opened::Mount => return Ok(Err(dir.path().join(step))),
            Opened::File => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} is a file, not a folder",
                        dir.path().join(step).display()
                    ),
                ));
            }
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

    /// A test's own folder, removed when the test ends, passed or failed.
    struct Temp(PathBuf);

    impl std::ops::Deref for Temp {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl AsRef<Path> for Temp {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp(name: &str) -> Temp {
        let dir = std::env::temp_dir().join(format!("rlxd-remove-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Temp(dir)
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

    /// `levels` folders, one in the other, made in `root` from `top`: the deepest.
    fn chain(root: &Path, top: &str, levels: usize) -> PathBuf {
        let mut deepest = root.join(top);
        for _ in 1..levels {
            deepest.push("d");
        }
        std::fs::create_dir_all(&deepest).unwrap();
        deepest
    }

    #[test]
    fn a_deep_tree_goes_to_its_bound_and_stops_past_it_naming_where() {
        // A folder held per level: more than a default limit of 256 open files.
        assert!(crate::runner::process::raise_fd_limit().is_some_and(|n| n > 512));
        let root = temp("deep");
        let dir = Dir::open_root(&root).unwrap().unwrap();
        // MAX_DEPTH folders, the removed one the first: all go, with no recursion.
        std::fs::write(chain(&root, "fits", MAX_DEPTH).join("f"), "x").unwrap();
        let mut links = Vec::new();
        dir.remove_tree(OsStr::new("fits"), &mut links).unwrap();
        assert!(!root.join("fits").exists());
        // One more: it stops there, and says where.
        let deepest = chain(&root, "deeper", MAX_DEPTH + 1);
        let failed = dir
            .remove_tree(OsStr::new("deeper"), &mut links)
            .unwrap_err();
        assert_eq!(failed.path, deepest);
        assert!(
            failed.error.to_string().contains("deeper than"),
            "{failed:?}"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_mount_point_is_not_gone_into() {
        // /dev is a filesystem of its own (devfs, devtmpfs) mounted on /.
        let root = Dir::open_root(Path::new("/")).unwrap().unwrap();
        assert!(matches!(
            root.child(OsStr::new("dev")).unwrap(),
            Opened::Mount
        ));
        assert!(matches!(
            root.child(OsStr::new("etc")).unwrap(),
            Opened::Dir(_) | Opened::Link
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_bind_mount_of_the_same_filesystem_is_not_gone_into() {
        let sudo = |args: &[&std::ffi::OsStr]| {
            std::process::Command::new("sudo")
                .arg("-n")
                .args(args)
                .status()
                .is_ok_and(|status| status.success())
        };
        if !sudo(&[OsStr::new("true")]) {
            eprintln!("skipped: binding a folder needs `sudo -n`, which isn't here");
            return;
        }
        /// Unmounts its folder when the test ends, passed or failed.
        struct Bound(PathBuf);
        impl Drop for Bound {
            fn drop(&mut self) {
                let _ = std::process::Command::new("sudo")
                    .args([OsStr::new("-n"), OsStr::new("umount"), self.0.as_os_str()])
                    .status();
            }
        }
        // A folder of the same filesystem, bound inside the space: its files are the
        // source's, which an erase must not touch.
        let source = temp("bind-source");
        std::fs::write(source.join("keep"), "x").unwrap();
        let root = temp("bind");
        let at = root.join("space").join("repo");
        std::fs::create_dir_all(&at).unwrap();
        assert!(sudo(&[
            OsStr::new("mount"),
            OsStr::new("--bind"),
            source.as_os_str(),
            at.as_os_str(),
        ]));
        let _bound = Bound(at.clone());
        let dir = Dir::open_root(&root).unwrap().unwrap();
        let failed = dir
            .remove_tree(OsStr::new("space"), &mut Vec::new())
            .unwrap_err();
        assert_eq!(failed.path, at, "{failed:?}");
        assert_eq!(std::fs::read_to_string(source.join("keep")).unwrap(), "x");
    }

    #[test]
    #[allow(unsafe_code)]
    fn a_folder_it_may_not_read_is_named_unless_it_is_empty() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid(2) can't fail.
        if unsafe { libc::geteuid() } == 0 {
            return; // root reads every folder
        }
        let root = temp("locked");
        let dir = Dir::open_root(&root).unwrap().unwrap();
        let mode = |path: &Path, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        // Empty, it goes.
        let empty = chain(&root, "a", 2);
        mode(&empty, 0o000);
        dir.remove_tree(OsStr::new("a"), &mut Vec::new()).unwrap();
        assert!(!root.join("a").exists());
        // Not empty, the erase stops at it, and names it.
        let full = chain(&root, "b", 2);
        std::fs::write(full.join("f"), "x").unwrap();
        mode(&full, 0o000);
        let failed = dir
            .remove_tree(OsStr::new("b"), &mut Vec::new())
            .unwrap_err();
        mode(&full, 0o755);
        assert_eq!(failed.path, full);
        assert_eq!(
            failed.error.kind(),
            io::ErrorKind::PermissionDenied,
            "{failed:?}"
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
