//! Where clients connect (handoff 0.10.3, 1.3):
//!
//! - **macOS and Linux:** the Unix socket `<home>/run/rlxd.sock`, mode 0600, in a 0700
//!   folder.
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
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
    use std::time::Duration;

    use rlxd_client::Home;
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
        /// A half-closed socket can still be written to; a closed one can't (`POLLOUT`
        /// is gone, `POLLHUP` is set), on macOS and Linux alike.
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
            ready < 0 || poll.revents & libc::POLLOUT == 0 || poll.revents & libc::POLLHUP != 0
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
    }

    impl Listener {
        /// Listens on `home`'s socket, replacing a stale one: the caller holds the lock,
        /// so no other rlxd serves it.
        pub fn bind(home: &Home) -> io::Result<Self> {
            let path = home.socket();
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let listener = UnixListener::bind(&path).map_err(|error| {
                let len = path.as_os_str().len();
                io::Error::new(
                    error.kind(),
                    format!("cannot listen on {} ({len} bytes): {error}", path.display()),
                )
            })?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            Ok(Self { listener, path })
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
            let _ = fs::remove_file(&self.path);
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
