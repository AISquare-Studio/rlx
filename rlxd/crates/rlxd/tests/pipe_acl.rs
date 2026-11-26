//! Windows: the live pipe's DACL holds one entry, for the current user's SID (handoff
//! 0.10.3, 1.3). It is read here through `GetSecurityInfo` on a client handle, apart from
//! the check rlxd makes on its own before it serves.
#![cfg(windows)]
#![allow(unsafe_code)]

mod common;

use std::os::windows::io::AsRawHandle;
use std::ptr;

use common::TestHome;
use rlxd_client::Home;
use rlxd_client::windows::Sid;
use windows_sys::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION,
    GetAce, GetAclInformation, PSECURITY_DESCRIPTOR,
};

#[test]
fn the_live_pipes_dacl_holds_one_entry_for_the_current_user() {
    let home = TestHome::new();
    home.start();
    let name = Home::at(home.root()).pipe_name();
    let pipe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&name)
        .unwrap_or_else(|e| panic!("{name}: {e}"));

    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: the handle is open while `pipe` lives; the call LocalAllocs `descriptor`,
    // into which `dacl` points, freed at the end.
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
    assert_eq!(status, 0, "GetSecurityInfo failed with {status}");
    assert!(!dacl.is_null(), "a pipe with no DACL admits everyone");

    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: `dacl` is valid while `descriptor` lives, and `size` is the struct
    // AclSizeInformation fills.
    let read = unsafe {
        GetAclInformation(
            dacl,
            (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    assert_ne!(read, 0, "GetAclInformation failed");
    assert_eq!(size.AceCount, 1, "the DACL holds one entry");

    let mut ace: *mut core::ffi::c_void = ptr::null_mut();
    // SAFETY: entry 0 exists, as the count says.
    assert_ne!(unsafe { GetAce(dacl, 0, &mut ace) }, 0, "GetAce failed");
    let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
    // SAFETY: every entry starts with its header.
    let kind = unsafe { (*ace).Header.AceType };
    assert_eq!(kind, 0, "the entry allows (ACCESS_ALLOWED_ACE_TYPE)");
    let me = Sid::current_user().unwrap();
    // SAFETY: an allowed entry's SID starts at `SidStart`, inside the descriptor.
    let ours = unsafe { me.equals(ptr::addr_of!((*ace).SidStart).cast_mut().cast()) };
    assert!(ours, "the entry is for the current user, {me:?}");

    // SAFETY: `descriptor` was LocalAlloc'ed by GetSecurityInfo.
    unsafe { LocalFree(descriptor as HLOCAL) };
    drop(pipe);
    assert_eq!(home.status()["pipe_acl"], "current_user");
}
