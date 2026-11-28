//! Where clients connect (handoff 0.10.3, 1.3):
//!
//! - **macOS and Linux:** the Unix socket `<home>/run/rlxd.sock`, mode 0600, in a 0700
//!   folder. When that path is too long for the OS, the short socket
//!   `/tmp/rlxd-<uid>/<hash of home>.sock` instead ([`Home::socket`]). Its folder must be
//!   this user's, mode 0700, and not a link, in a `/tmp` that is root's and sticky: rlxd
//!   refuses to start otherwise, and never changes a folder it finds into shape.
//! - **Replacing a socket.** A socket left at the path by an rlxd that is gone is
//!   replaced; one that something still listens on is refused, never taken over. A stop
//!   removes the socket only if it is still the one this rlxd made.
//! - **Windows:** the named pipe `\\.\pipe\rlxd-<hash of home>`, whose DACL holds one
//!   entry, for the user rlxd runs as. rlxd makes its first instance, so a pipe someone
//!   else made first under that name fails the start, and it reads the DACL back from
//!   the live pipe before it takes a connection. Remote clients are refused.
//!
//! Each connection comes with its [`Peer`](crate::peer::Peer), pinned as it is accepted,
//! for the gate's identity check (handoff 0.10.3, 5.1).

#[cfg(unix)]
pub use unix::{Hangup, Listener};
#[cfg(windows)]
pub use windows::{Hangup, Listener};

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
    use std::time::Duration;

    use rlxd_client::{Home, SOCKET_PATH_MAX, unix};
    use tokio::net::{UnixListener, UnixStream};

    use crate::peer::Peer;

    /// Whether a client has closed its connection both ways, not only its sending half.
    /// A client that half-closes still gets its replies.
    pub struct Hangup {
        socket: Option<OwnedFd>,
    }

    impl Hangup {
        fn of_socket(fd: BorrowedFd<'_>) -> Self {
            Self {
                socket: fd.try_clone_to_owned().ok(),
            }
        }

        /// Once reading has ended: whether the client is gone, rather than half-closed.
        /// Only `POLLHUP` (or `POLLERR`) says so, on macOS and Linux alike. A missing
        /// `POLLOUT` does not: replies the client has not read yet fill the send buffer
        /// of a client that is still there.
        ///
        /// It polls for `POLLOUT` only, never `POLLIN`: macOS reports `POLLHUP` on the
        /// read side's end of file, which a half-close is too, while the write side's
        /// end comes only once the client has closed the connection both ways.
        #[allow(unsafe_code)]
        pub fn gone(&self) -> bool {
            let Some(socket) = &self.socket else {
                return true;
            };
            let mut poll = libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            // SAFETY: one live pollfd, and a zero timeout.
            let ready = unsafe { libc::poll(&mut poll, 1, 0) };
            ready < 0 || poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
        }

        /// Resolves once the client is gone, checking a few times a second: for a
        /// connection rlxd isn't reading, half-closed or with frames queued.
        pub async fn closed(&self) {
            while !self.gone() {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }

    /// The socket, listening.
    pub struct Listener {
        listener: UnixListener,
        path: PathBuf,
        /// The socket file's device and inode, so a stop removes only this one.
        made: (u64, u64),
    }

    impl Listener {
        /// Listens on `home`'s socket, replacing a stale one: the caller holds the lock,
        /// so no other rlxd serves it. A short socket's folder is made first.
        pub fn bind(home: &Home) -> io::Result<Self> {
            let path = home.socket();
            let short = if home.uses_short_socket() {
                let in_home = home.socket_in_home();
                crate::log::info!(
                    "the home's socket path, {}, is {} bytes, over this OS's {SOCKET_PATH_MAX}: \
                     listening on {} instead",
                    in_home.display(),
                    in_home.as_os_str().len(),
                    path.display()
                );
                let dir = Home::short_socket_dir();
                unix::check_sticky(dir.parent().unwrap_or(Path::new("/tmp")))?;
                Some((private_short_dir(&dir)?, dir))
            } else {
                None
            };
            replace_stale(&path)?;
            let listener = UnixListener::bind(&path).map_err(|error| {
                let len = path.as_os_str().len();
                io::Error::new(
                    error.kind(),
                    format!("cannot listen on {} ({len} bytes): {error}", path.display()),
                )
            })?;
            // The folder was checked, then used by path. /tmp is sticky, so no one else
            // can have swapped it in between; look again all the same.
            if let Some((checked, dir)) = short {
                let now = fs::symlink_metadata(&dir)?;
                if (now.dev(), now.ino()) != checked || !unix::is_private_dir(&now) {
                    return Err(io::Error::other(format!(
                        "{} changed while rlxd made its socket in it; refusing it",
                        dir.display()
                    )));
                }
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            let made = fs::symlink_metadata(&path)?;
            Ok(Self {
                listener,
                path,
                made: (made.dev(), made.ino()),
            })
        }

        /// The next connection, its peer, and its hangup probe.
        pub async fn accept(&mut self) -> io::Result<(UnixStream, Peer, Hangup)> {
            let (stream, _) = self.listener.accept().await?;
            let peer = Peer::of_socket(stream.as_fd());
            let hangup = Hangup::of_socket(stream.as_fd());
            Ok((stream, peer, hangup))
        }

        /// What `status` says of the endpoint's access list: nothing here, where the
        /// socket's mode does that.
        pub fn acl(&self) -> Option<&'static str> {
            None
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            remove_if_made(&self.path, self.made);
        }
    }

    /// Removes the socket at `path` if it is still the one this rlxd made, `made` (its
    /// device and inode), and not one another rlxd has bound there since.
    fn remove_if_made(path: &Path, made: (u64, u64)) {
        if let Ok(found) = fs::symlink_metadata(path)
            && (found.dev(), found.ino()) == made
        {
            let _ = fs::remove_file(path);
        }
    }

    /// Clears `path` for rlxd to bind. The caller holds the home's lock, so no rlxd of
    /// this home listens there. A socket that still takes a connection is someone else's,
    /// such as another home's rlxd, and is refused rather than taken over. A socket
    /// nothing listens on, left by an rlxd that is gone, or any other file, is removed.
    fn replace_stale(path: &Path) -> io::Result<()> {
        use std::os::unix::fs::FileTypeExt;
        match fs::symlink_metadata(path) {
            Ok(found) => {
                if found.file_type().is_socket()
                    && std::os::unix::net::UnixStream::connect(path).is_ok()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!(
                            "something already listens on {}, and it is not this home's \
                             rlxd, whose lock this one holds: rlxd will not take it over",
                            path.display()
                        ),
                    ));
                }
                fs::remove_file(path)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Makes `dir`, the short sockets' folder in `/tmp`, 0700, and gives its device and
    /// inode. One that is there must be a folder, not a link, this user's, and 0700, else
    /// it is refused, never changed into shape: while it was otherwise, another user could
    /// have left entries in it. The folder is left behind on stop, as other homes' sockets
    /// may be in it.
    fn private_short_dir(dir: &Path) -> io::Result<(u64, u64)> {
        match fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("cannot make {}: {error}", dir.display()),
                ));
            }
        }
        let found = fs::symlink_metadata(dir)?;
        if !found.is_dir() {
            return Err(io::Error::other(format!(
                "{} is not a folder; remove it, and rlxd makes it",
                dir.display()
            )));
        }
        if !unix::is_owned(&found) {
            return Err(io::Error::other(format!(
                "{} belongs to uid {}, not to this user (uid {}): rlxd will not put its \
                 socket there",
                dir.display(),
                found.uid(),
                unix::uid()
            )));
        }
        if !unix::is_private_dir(&found) {
            return Err(io::Error::other(format!(
                "{} has mode {:o}, so others may enter it: rlxd will not put its socket \
                 there. Remove it once no rlxd uses it (`rm -r {}`), and rlxd makes it \
                 again, 0700",
                dir.display(),
                found.mode() & 0o7777,
                dir.display()
            )));
        }
        Ok((found.dev(), found.ino()))
    }

    #[cfg(test)]
    mod tests {
        use std::io::{ErrorKind, Write};
        use std::net::Shutdown;
        use std::os::unix::net::UnixStream;

        use super::*;

        fn fresh(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("rlxd-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            let _ = fs::remove_file(&dir);
            dir
        }

        fn mode(path: &Path) -> u32 {
            fs::symlink_metadata(path).unwrap().mode() & 0o777
        }

        #[test]
        fn the_short_folder_is_made_0700_and_a_wider_one_refused_as_it_is() {
            let dir = fresh("short-made");
            private_short_dir(&dir).unwrap();
            assert_eq!(mode(&dir), 0o700);
            private_short_dir(&dir).unwrap();
            for wider in [0o755, 0o750, 0o701, 0o1777] {
                fs::set_permissions(&dir, fs::Permissions::from_mode(wider)).unwrap();
                let refused = private_short_dir(&dir).unwrap_err().to_string();
                assert!(refused.contains("others may enter it"), "{refused}");
                assert!(refused.contains(&format!("mode {wider:o}")), "{refused}");
                let left = fs::symlink_metadata(&dir).unwrap().mode() & 0o7777;
                assert_eq!(left, wider, "never changed into shape");
            }
            fs::remove_dir(&dir).unwrap();
        }

        #[test]
        fn a_live_socket_is_refused_and_a_stale_one_or_a_file_replaced() {
            let dir = fresh("short-stale");
            fs::create_dir(&dir).unwrap();
            let path = dir.join("s.sock");
            replace_stale(&path).unwrap();
            let live = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let refused = replace_stale(&path).unwrap_err();
            assert_eq!(refused.kind(), io::ErrorKind::AddrInUse, "{refused}");
            assert!(path.exists());
            drop(live);
            replace_stale(&path).unwrap();
            assert!(!path.exists(), "a socket nothing listens on is replaced");
            fs::write(&path, "").unwrap();
            replace_stale(&path).unwrap();
            assert!(!path.exists());
            fs::remove_dir(&dir).unwrap();
        }

        #[test]
        fn a_stop_removes_only_the_socket_it_made() {
            let dir = fresh("short-made-only");
            fs::create_dir(&dir).unwrap();
            let path = dir.join("s.sock");
            let _first = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let made = fs::symlink_metadata(&path).unwrap();
            let made = (made.dev(), made.ino());
            // Moved aside, not removed, so its inode can't be reused for the next one.
            fs::rename(&path, dir.join("moved.sock")).unwrap();
            let _second = std::os::unix::net::UnixListener::bind(&path).unwrap();
            remove_if_made(&path, made);
            assert!(path.exists(), "another rlxd's socket stays");
            let now = fs::symlink_metadata(&path).unwrap();
            remove_if_made(&path, (now.dev(), now.ino()));
            assert!(!path.exists());
            fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn a_file_or_a_link_where_the_short_folder_goes_is_refused() {
            let file = fresh("short-file");
            fs::write(&file, "").unwrap();
            let refused = private_short_dir(&file).unwrap_err();
            assert!(refused.to_string().contains("not a folder"), "{refused}");
            fs::remove_file(&file).unwrap();

            let target = fresh("short-target");
            fs::create_dir(&target).unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
            let link = fresh("short-link");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let refused = private_short_dir(&link).unwrap_err();
            assert!(refused.to_string().contains("not a folder"), "{refused}");
            fs::remove_file(&link).unwrap();
            fs::remove_dir(&target).unwrap();
        }

        #[test]
        fn a_full_send_buffer_is_not_a_client_gone() {
            // Review #2068, r3 minor 1: replies a client has not read yet leave rlxd's
            // socket unwritable, which says nothing of whether the client is there.
            let (rlxd, client) = UnixStream::pair().unwrap();
            rlxd.set_nonblocking(true).unwrap();
            let mut writer = &rlxd;
            loop {
                match writer.write(&[b'x'; 512]) {
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => panic!("filling the send buffer: {e}"),
                }
            }
            let hangup = Hangup::of_socket(rlxd.as_fd());
            assert!(!hangup.gone(), "a full buffer, and the client still there");
            client.shutdown(Shutdown::Write).unwrap();
            assert!(!hangup.gone(), "a full buffer, and the client half-closed");
            drop(client);
            assert!(hangup.gone(), "the client closed both ways");
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    use std::io;
    use std::mem;
    use std::os::windows::io::{AsHandle, AsRawHandle};
    use std::ptr;

    use rlxd_client::Home;
    use rlxd_client::windows::Sid;
    use rlxd_proto::StatusData;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows_sys::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
        SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, PSECURITY_DESCRIPTOR,
        SECURITY_ATTRIBUTES,
    };

    use crate::peer::{self, Peer};

    /// A named pipe has no half-close: when reading ends, the client is gone. Unread, a
    /// pipe can't be told apart from a quiet client.
    pub struct Hangup;

    impl Hangup {
        /// Once reading has ended: the client is gone.
        pub fn gone(&self) -> bool {
            true
        }

        /// Never resolves: an unread pipe gives no sign that its client went.
        pub async fn closed(&self) {
            std::future::pending::<()>().await;
        }
    }

    /// `ACCESS_ALLOWED_ACE_TYPE`.
    const ACCESS_ALLOWED: u8 = 0;

    /// The pipe, with its next instance waiting for a client.
    pub struct Listener {
        name: String,
        security: Security,
        next: NamedPipeServer,
    }

    impl Listener {
        /// Makes `home`'s pipe: its first instance, admitting only this user, checked.
        pub fn bind(home: &Home) -> io::Result<Self> {
            let name = home.pipe_name();
            let me = Sid::current_user()?;
            let mut security = Security::for_only(&me)?;
            let first = security.create(&name, true).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot make the pipe {name}, which may be another process's: {error}"),
                )
            })?;
            if !dacl_admits_only(&first, &me)? {
                return Err(io::Error::other(format!(
                    "the pipe {name} admits more than this user; refusing to serve it"
                )));
            }
            Ok(Self {
                name,
                security,
                next: first,
            })
        }

        /// The next connection and its peer, with a fresh instance made for the one after.
        /// An instance whose client went before it was connected is dropped for a fresh
        /// one too. The peer is pinned before anything else, so its pid can't be reused
        /// meanwhile.
        pub async fn accept(&mut self) -> io::Result<(NamedPipeServer, Peer, Hangup)> {
            let connected = connect_stamped(&self.next).await;
            let peer = connected
                .as_ref()
                .ok()
                .map(|at| Peer::of_pipe(self.next.as_handle(), *at));
            let fresh = self.security.create(&self.name, false)?;
            let used = mem::replace(&mut self.next, fresh);
            connected?;
            let peer = peer.expect("pinned when connected");
            Ok((used, peer, Hangup))
        }

        /// What `status` says of the pipe's DACL: one entry, for this user, which
        /// [`Listener::bind`] read back from the live pipe.
        pub fn acl(&self) -> Option<&'static str> {
            Some(StatusData::PIPE_ACL_CURRENT_USER)
        }
    }

    /// Waits for a client to connect to `pipe`, and returns when it did, as a `FILETIME`
    /// count: when the connection's waker last fired, before this task ran again, or
    /// when it was found connected at once. The peer check refuses a process created
    /// after this time (a reused pid), so it is the latest wake: never before the
    /// connection, and as close to it as the runtime lets rlxd see.
    async fn connect_stamped(pipe: &NamedPipeServer) -> io::Result<u64> {
        use std::future::Future;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::task::{Context, Poll, Wake, Waker};

        struct Stamp {
            at: Arc<AtomicU64>,
            inner: Waker,
        }

        impl Wake for Stamp {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.at.store(peer::connected_now(), Ordering::SeqCst);
                self.inner.wake_by_ref();
            }
        }

        let at = Arc::new(AtomicU64::new(0));
        let connect = pipe.connect();
        tokio::pin!(connect);
        std::future::poll_fn(|cx| {
            let waker = Waker::from(Arc::new(Stamp {
                at: Arc::clone(&at),
                inner: cx.waker().clone(),
            }));
            match connect.as_mut().poll(&mut Context::from_waker(&waker)) {
                Poll::Ready(result) => {
                    let stamped = match at.load(Ordering::SeqCst) {
                        0 => peer::connected_now(),
                        stamped => stamped,
                    };
                    Poll::Ready(result.map(|()| stamped))
                }
                Poll::Pending => {
                    // A wake that left it pending was not the connection.
                    at.store(0, Ordering::SeqCst);
                    Poll::Pending
                }
            }
        })
        .await
    }

    /// A security descriptor whose DACL admits one user, and the attributes that carry
    /// it to `CreateNamedPipe`.
    struct Security {
        descriptor: PSECURITY_DESCRIPTOR,
        attributes: SECURITY_ATTRIBUTES,
    }

    impl Security {
        /// `D:P(A;;GA;;;<sid>)`: a protected DACL with one entry, all access for `sid`.
        fn for_only(sid: &Sid) -> io::Result<Self> {
            let sddl = format!("D:P(A;;GA;;;{})", sid.to_string_form()?);
            let wide: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
            let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
            // SAFETY: `wide` is NUL-terminated, and `descriptor` receives a LocalAlloc'ed
            // descriptor, freed in Drop.
            let made = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            };
            if made == 0 {
                return Err(io::Error::last_os_error());
            }
            let attributes = SECURITY_ATTRIBUTES {
                nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            Ok(Self {
                descriptor,
                attributes,
            })
        }

        fn create(&mut self, name: &str, first: bool) -> io::Result<NamedPipeServer> {
            let mut options = ServerOptions::new();
            options
                .first_pipe_instance(first)
                .reject_remote_clients(true);
            let attributes: *mut SECURITY_ATTRIBUTES = &mut self.attributes;
            // SAFETY: `attributes` points to valid SECURITY_ATTRIBUTES whose descriptor
            // lives as long as `self`, and the call only reads it.
            unsafe { options.create_with_security_attributes_raw(name, attributes.cast()) }
        }
    }

    impl Drop for Security {
        fn drop(&mut self) {
            // SAFETY: the descriptor was LocalAlloc'ed by the conversion and is ours.
            unsafe { LocalFree(self.descriptor as HLOCAL) };
        }
    }

    /// Whether the live pipe's DACL holds exactly one entry, allowing `sid`.
    fn dacl_admits_only(pipe: &NamedPipeServer, sid: &Sid) -> io::Result<bool> {
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: the handle is open while `pipe` is borrowed; the call fills `dacl` with
        // a pointer into `descriptor`, which it LocalAllocs and we free below.
        let status = unsafe {
            GetSecurityInfo(
                pipe.as_raw_handle() as HANDLE,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        // SAFETY: `dacl` points into `descriptor`, valid until it is freed below.
        let only = unsafe { dacl_is_one_allow_for(dacl, sid) };
        // SAFETY: `descriptor` was LocalAlloc'ed by GetSecurityInfo.
        unsafe { LocalFree(descriptor as HLOCAL) };
        Ok(only)
    }

    /// # Safety
    ///
    /// `dacl` must be null or point to a valid ACL.
    unsafe fn dacl_is_one_allow_for(dacl: *mut ACL, sid: &Sid) -> bool {
        if dacl.is_null() {
            // No DACL at all admits everyone.
            return false;
        }
        let mut size = ACL_SIZE_INFORMATION::default();
        // SAFETY: `dacl` is valid, and `size` is the struct the class asks for.
        let read = unsafe {
            GetAclInformation(
                dacl,
                (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        };
        if read == 0 || size.AceCount != 1 {
            return false;
        }
        let mut ace: *mut core::ffi::c_void = ptr::null_mut();
        // SAFETY: entry 0 exists, as the count says.
        if unsafe { GetAce(dacl, 0, &mut ace) } == 0 || ace.is_null() {
            return false;
        }
        let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
        // SAFETY: every entry starts with an ACE_HEADER; an allowed one is an
        // ACCESS_ALLOWED_ACE whose SID starts at `SidStart`.
        unsafe {
            (*ace).Header.AceType == ACCESS_ALLOWED
                && sid.equals(ptr::addr_of!((*ace).SidStart).cast_mut().cast())
        }
    }
}
