//! Where clients connect (handoff 0.10.3, 1.3):
//!
//! - **macOS and Linux:** the Unix socket `<home>/run/rlxd.sock`, mode 0600, in a 0700
//!   folder. When that path is too long for the OS, the short socket
//!   `/tmp/rlxd-<uid>/<hash of home>.sock` instead ([`Home::socket`]), whose folder must
//!   be this user's: one another user made first is refused, and a wider mode is narrowed
//!   to 0700.
//! - **Windows:** the named pipe `\\.\pipe\rlxd-<hash of home>`, whose DACL holds one
//!   entry, for the user rlxd runs as. rlxd makes its first instance, so a pipe someone
//!   else made first under that name fails the start, and it reads the DACL back from
//!   the live pipe before it takes a connection. Remote clients are refused.

#[cfg(unix)]
pub use unix::Listener;
#[cfg(windows)]
pub use windows::Listener;

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use rlxd_client::{Home, SOCKET_PATH_MAX, unix};
    use tokio::net::{UnixListener, UnixStream};

    /// The socket, listening.
    pub struct Listener {
        listener: UnixListener,
        path: PathBuf,
    }

    impl Listener {
        /// Listens on `home`'s socket, replacing a stale one: the caller holds the lock,
        /// so no other rlxd serves it. A short socket's folder is made first.
        pub fn bind(home: &Home) -> io::Result<Self> {
            let path = home.socket();
            if home.uses_short_socket() {
                let in_home = home.socket_in_home();
                let len = std::path::absolute(&in_home)
                    .unwrap_or(in_home)
                    .as_os_str()
                    .len();
                crate::log::info!(
                    "the home's socket path is {len} bytes, over this OS's {SOCKET_PATH_MAX}: \
                     listening on {} instead",
                    path.display()
                );
                private_short_dir(&Home::short_socket_dir())?;
            }
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

        /// The next connection.
        pub async fn accept(&mut self) -> io::Result<UnixStream> {
            self.listener.accept().await.map(|(stream, _)| stream)
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

    /// Makes `dir`, the short sockets' folder in `/tmp`, 0700. One that is there must be a
    /// folder, not a link, and this user's, else it is refused: another user who made it
    /// first could read or replace the socket. A wider mode is narrowed. The folder is
    /// left behind on stop, as other homes' sockets may be in it.
    fn private_short_dir(dir: &Path) -> io::Result<()> {
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
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
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
        fn the_short_folder_is_made_0700_and_a_wider_one_narrowed() {
            let dir = fresh("short-made");
            private_short_dir(&dir).unwrap();
            assert_eq!(mode(&dir), 0o700);
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            private_short_dir(&dir).unwrap();
            assert_eq!(mode(&dir), 0o700);
            fs::remove_dir(&dir).unwrap();
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
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    use std::io;
    use std::mem;
    use std::os::windows::io::AsRawHandle;
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

        /// The next connection, with a fresh instance made for the one after. An instance
        /// whose client went before it was connected is dropped for a fresh one too.
        pub async fn accept(&mut self) -> io::Result<NamedPipeServer> {
            let connected = self.next.connect().await;
            let fresh = self.security.create(&self.name, false)?;
            let used = mem::replace(&mut self.next, fresh);
            connected.map(|()| used)
        }

        /// What `status` says of the pipe's DACL: one entry, for this user, which
        /// [`Listener::bind`] read back from the live pipe.
        pub fn acl(&self) -> Option<&'static str> {
            Some(StatusData::PIPE_ACL_CURRENT_USER)
        }
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
