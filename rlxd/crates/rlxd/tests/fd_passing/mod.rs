//! Passing a connection's descriptor between processes (`SCM_RIGHTS`), and saying
//! `hello` on it: for the gate peer tests whose helper hands its connection on.
#![allow(dead_code, unsafe_code)]

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use serde_json::{Value, json};

/// Says `hello {role: "app"}` on `stream`, and reads the reply.
pub fn raw_hello(stream: &UnixStream, token: &str, credential: &str) -> Value {
    let hello = json!({"id": 1, "method": "hello", "params": {
        "token": token, "protocol_version": 1, "client": "gate-peer-test",
        "client_version": "0", "role": "app", "gate_credential": credential,
        "capabilities": [],
    }});
    let mut writer = stream.try_clone().unwrap();
    writer.write_all(format!("{hello}\n").as_bytes()).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"))
}

/// Sends `fd` over the Unix socket `over`, as `SCM_RIGHTS`.
pub fn send_fd(over: RawFd, fd: RawFd) {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    // SAFETY: a pure computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: an all-zero msghdr is a valid empty one.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = space as _;
    // SAFETY: `control` has room for one header carrying one descriptor, and `message`
    // points at live buffers.
    let sent = unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd);
        libc::sendmsg(over, &message, 0)
    };
    assert_eq!(sent, 1, "sendmsg: {}", std::io::Error::last_os_error());
}

/// Receives one descriptor sent with [`send_fd`].
pub fn receive_fd(over: RawFd) -> OwnedFd {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    // SAFETY: a pure computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: an all-zero msghdr is a valid empty one.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = space as _;
    // SAFETY: `message` points at live buffers of the sizes it says.
    let read = unsafe { libc::recvmsg(over, &mut message, 0) };
    assert_eq!(read, 1, "recvmsg: {}", std::io::Error::last_os_error());
    // SAFETY: the kernel filled one SCM_RIGHTS header, read here.
    let fd = unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        assert!(!header.is_null(), "no descriptor came");
        assert_eq!((*header).cmsg_type, libc::SCM_RIGHTS);
        std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>())
    };
    // SAFETY: the kernel just made this descriptor for us; nothing else owns it.
    unsafe { OwnedFd::from_raw_fd(fd) }
}
