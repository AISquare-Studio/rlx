//! The Linux gate peer check (handoff 0.10.3 rev 6, 5.1 Accept, "Linux"; task acceptance
//! 4), with rlxd outside dev mode:
//!
//! ```text
//! cargo test -p rlxd --features gate-test-requirement --test gate_peer_linux
//! ```
//!
//! The expected binary, which `RLXD_TEST_GATE_REQUIREMENT` names, is this test binary, so
//! a connection the test opens itself is the control. It fails, and doesn't skip, on a
//! kernel older than 6.5, where rlxd has no `SO_PEERPIDFD` to log.
//!
//! The other-uid check runs its client as root through `sudo -n`. Where that needs a
//! password it is skipped, with a note, unless `RLXD_GATE_PEER_REQUIRED` is set, as in
//! CI, where it fails instead.
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use common::TestHome;
use rlxd_client::proto::{HelloParams, Role};
use rlxd_client::{Connection, Home};
use serde_json::{Value, json};

/// Set in the helper process: the descriptor to pass its connection over.
const HELPER_FD: &str = "RLXD_GATE_HELPER_FD";
/// Set in the helper process: the credential its control hello presents.
const HELPER_CREDENTIAL: &str = "RLXD_GATE_HELPER_CREDENTIAL";

/// A fresh home whose rlxd, outside dev mode, expects this test binary as the app.
fn home() -> TestHome {
    let me = std::env::current_exe().unwrap();
    TestHome::new().with_env("RLXD_TEST_GATE_REQUIREMENT", me.to_str().unwrap())
}

/// A connection from this process, after a `hello` as `role`: its reply envelope too.
fn connect(root: &Path, role: Role, credential: Option<&str>) -> (Connection, Value) {
    let home = Home::at(root);
    let token = std::fs::read_to_string(home.token()).unwrap();
    let mut connection = Connection::open(&home).unwrap();
    let mut hello = HelloParams::new(token.trim(), "gate-peer-linux", "0", role).unwrap();
    if let Some(credential) = credential {
        hello = hello.with_gate_credential(credential).unwrap();
    }
    let reply = connection.hello(&hello).unwrap();
    (connection, serde_json::from_str(&reply.line).unwrap())
}

fn call(connection: &mut Connection, method: &str, params: Value) -> Value {
    let Value::Object(params) = params else {
        panic!("params must be an object")
    };
    let reply = connection.request(method, params).unwrap();
    serde_json::from_str(&reply.line).unwrap()
}

/// Pairs this process (the expected binary): its credential.
fn pair(root: &Path) -> String {
    let (mut connection, hello) = connect(root, Role::Cli, None);
    assert_eq!(hello["ok"], true, "{hello}");
    let reply = call(&mut connection, "gate.pair", json!({}));
    assert_eq!(reply["ok"], true, "{reply}");
    reply["data"]["gate_credential"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn the_expected_binary_passes_on_so_peerpidfd_and_still_needs_the_credential() {
    let home = home();
    home.start();
    let credential = pair(home.root());
    let (mut app, hello) = connect(home.root(), Role::App, Some(&credential));
    assert_eq!(hello["ok"], true, "{hello}");
    assert_eq!(
        call(&mut app, "status", json!({}))["data"]["clients"]["app"],
        1
    );
    let log = home.log();
    let pinned = format!("SO_PEERPIDFD: pid {} runs", std::process::id());
    assert!(
        log.lines().filter(|l| l.contains(&pinned)).count() >= 2,
        "the pairing and the hello were each checked through SO_PEERPIDFD \
         (Linux 6.5 or later):\n{log}"
    );
    // A passing peer still needs the credential.
    let (_, hello) = connect(home.root(), Role::App, None);
    assert_eq!(hello["error"]["code"], "not_gate", "{hello}");
    let (mut cli, _) = connect(home.root(), Role::Cli, None);
    let reply = call(&mut cli, "gate.pair", json!({}));
    assert_eq!(reply["error"]["code"], "not_gate", "{reply}");
}

#[test]
fn another_binary_is_refused() {
    let home = home();
    home.start();
    let credential = pair(home.root());
    // rlxd's own `call` is not the expected binary.
    let run = home.rlxd(&[
        "call",
        "--role",
        "app",
        "--gate-credential",
        &credential,
        "status",
    ]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "not_gate");
    assert!(
        home.log().contains("which is not rlx's app"),
        "{}",
        home.log()
    );
    assert_eq!(home.rlxd(&["gate", "unpair"]).code(), Some(0));
    let run = home.rlxd(&["call", "gate.pair"]);
    assert_eq!(
        run.error_code(),
        "not_gate",
        "no first pairing either: {run:?}"
    );
}

#[test]
fn without_a_packaged_install_the_uid_and_the_binarys_name_are_checked() {
    // The hub's rule for an AppImage: Linux's weaker guarantee, not dev mode. No
    // RLXD_TEST_GATE_REQUIREMENT here, so the compiled-in identity holds, and this host
    // has no packaged rlx app.
    for packaged in rlxd::peer::LINUX_PACKAGED_BINARIES {
        assert!(
            !Path::new(packaged).exists(),
            "{packaged} is installed here"
        );
    }
    let home = TestHome::new();
    let dir = home.root().join("helpers");
    std::fs::create_dir_all(&dir).unwrap();
    let app = dir.join(rlxd::peer::LINUX_APP_NAME);
    std::fs::copy(env!("CARGO_BIN_EXE_rlxd"), &app).unwrap();
    home.start();
    let run = home.rlxd(&["call", "gate.pair"]);
    assert_eq!(run.error_code(), "not_gate", "rlxd's own client: {run:?}");
    let run = home.run_of(&app, &["call", "gate.pair"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let credential = run.data()["gate_credential"].as_str().unwrap().to_owned();
    let app_hello = [
        "call",
        "--role",
        "app",
        "--gate-credential",
        &credential,
        "status",
    ];
    assert_eq!(home.run_of(&app, &app_hello).code(), Some(0));
    assert_eq!(home.rlxd(&app_hello).error_code(), "not_gate");
    assert!(
        home.log().contains("Linux's weaker guarantee"),
        "{}",
        home.log()
    );
}

#[test]
fn a_dead_peer_is_refused() {
    let home = home();
    home.start();
    let credential = pair(home.root());
    let (mine, theirs) = UnixStream::pair().unwrap();
    // The helper inherits its end.
    // SAFETY: clears FD_CLOEXEC on a descriptor this test owns.
    assert_eq!(
        unsafe { libc::fcntl(theirs.as_raw_fd(), libc::F_SETFD, 0) },
        0
    );
    let me = std::env::current_exe().unwrap();
    let out = home
        .command_of(
            &me,
            &[
                "--exact",
                "helper_connects_and_passes_its_socket",
                "--nocapture",
                "--test-threads=1",
            ],
        )
        .env(HELPER_FD, theirs.as_raw_fd().to_string())
        .env(HELPER_CREDENTIAL, &credential)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    // `output` waited for the helper, so it has exited and been reaped.
    assert!(
        out.status.success(),
        "the helper: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    drop(theirs);
    let passed = UnixStream::from(receive_fd(mine.as_raw_fd()));
    let reply = raw_hello(&passed, &home.token(), &credential);
    assert_eq!(reply["error"]["code"], "not_gate", "{reply}");
    let log = home.log();
    assert!(
        log.contains("the peer has exited") || log.contains("no longer the peer that connected"),
        "{log}"
    );
}

/// Not a test of its own: the helper `a_dead_peer_is_refused` runs, in a process of its
/// own. It proves it passes the peer check, connects, hands that connection to its
/// parent over `SCM_RIGHTS`, and exits.
#[test]
fn helper_connects_and_passes_its_socket() {
    let Ok(fd) = std::env::var(HELPER_FD) else {
        return;
    };
    let fd: RawFd = fd.parse().unwrap();
    let home = Home::from_env().unwrap();
    let credential = std::env::var(HELPER_CREDENTIAL).unwrap();
    let (_, hello) = connect(home.root(), Role::App, Some(&credential));
    assert_eq!(
        hello["ok"], true,
        "the helper passes while it runs: {hello}"
    );
    let stream = UnixStream::connect(home.socket()).unwrap();
    // Long enough for rlxd to accept the connection and pin this process.
    std::thread::sleep(Duration::from_millis(300));
    send_fd(fd, stream.as_raw_fd());
}

#[test]
fn a_client_of_another_uid_is_refused() {
    let required = std::env::var_os("RLXD_GATE_PEER_REQUIRED").is_some();
    let sudo = std::process::Command::new("sudo")
        .args(["-n", "true"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !sudo {
        assert!(
            !required,
            "sudo -n is required here, to run a client as root"
        );
        eprintln!("skipped: sudo -n needs a password here, so no client can run as root");
        return;
    }
    let home = home();
    home.start();
    let credential = pair(home.root());
    let rlxd = env!("CARGO_BIN_EXE_rlxd");
    let aisquare_home = format!("AISQUARE_HOME={}", home.root().display());
    let mut command = std::process::Command::new("sudo");
    command.args([
        "-n",
        "env",
        &aisquare_home,
        "RLX_HOME=",
        rlxd,
        "call",
        "--role",
        "app",
        "--gate-credential",
        &credential,
        "status",
    ]);
    let run = common::run(command, None);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "not_gate");
    let log = home.log();
    assert!(log.contains("runs as uid 0, not rlxd's"), "{log}");
}

/// Says `hello {role: "app"}` on `stream`, and reads the reply.
fn raw_hello(stream: &UnixStream, token: &str, credential: &str) -> Value {
    let hello = json!({"id": 1, "method": "hello", "params": {
        "token": token, "protocol_version": 1, "client": "gate-peer-linux",
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
fn send_fd(over: RawFd, fd: RawFd) {
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
fn receive_fd(over: RawFd) -> OwnedFd {
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
