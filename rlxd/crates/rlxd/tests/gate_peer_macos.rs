//! The macOS gate peer check (handoff 0.10.3 rev 6, 5.1 Accept, "macOS, signed"), with
//! rlxd outside dev mode and the test identity in force:
//!
//! ```text
//! cargo test -p rlxd --features gate-test-requirement --test gate_peer_macos
//! ```
//!
//! Each helper is a copy of `rlxd`, signed before rlxd starts, whose `rlxd call` is the
//! client. The test identity is an ad-hoc signature with the identifier
//! `studio.aisquare.rlx.gatetest`, named by `RLXD_TEST_GATE_REQUIREMENT`, which only a
//! build with the `gate-test-requirement` feature reads. gate_peer_macos_release checks
//! the compiled-in requirement.
#![cfg(target_os = "macos")]
#![allow(unsafe_code)]

mod common;
mod fd_passing;
mod macos_signing;

use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use common::{TestHome, eventually};
use fd_passing::{raw_hello, receive_fd, send_fd};
use macos_signing::{INJECTION_ENTITLEMENTS, Signing, TEST_IDENTIFIER, helper, test_requirement};
use rlxd_client::Home;

/// A fresh home whose rlxd, outside dev mode, takes the test identity's requirement.
fn home() -> TestHome {
    TestHome::new().with_env("RLXD_TEST_GATE_REQUIREMENT", &test_requirement())
}

fn helpers(home: &TestHome) -> PathBuf {
    home.root().join("helpers")
}

/// The passing helper: the test identifier, the hardened runtime, no entitlements.
fn good(home: &TestHome) -> PathBuf {
    helper(
        &helpers(home),
        "good",
        &Signing {
            identifier: TEST_IDENTIFIER,
            runtime: true,
            entitlements: &[],
            identity: None,
        },
    )
}

/// `helper call args…`: its exit code and error code, if any.
fn call(home: &TestHome, helper: &Path, args: &[&str]) -> common::Run {
    let mut all = vec!["call"];
    all.extend_from_slice(args);
    home.run_of(helper, &all)
}

/// Pairs the passing helper (a first pairing): its credential.
fn pair(home: &TestHome, good: &Path) -> String {
    let run = call(home, good, &["gate.pair"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    run.data()["gate_credential"].as_str().unwrap().to_owned()
}

#[test]
fn a_signed_helper_passes_on_its_audit_token_and_still_needs_the_credential() {
    let home = home();
    let good = good(&home);
    home.start();
    assert_eq!(home.status()["gate"], "signed app only");
    let credential = pair(&home, &good);
    let run = call(
        &home,
        &good,
        &["--role", "app", "--gate-credential", &credential, "status"],
    );
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(run.data()["clients"]["app"], 1);
    let log = home.log();
    assert!(
        log.lines()
            .filter(|l| l.contains("LOCAL_PEERTOKEN audit token of pid"))
            .count()
            >= 2,
        "the pairing and the hello were each checked on the audit token:\n{log}"
    );
    // A verified peer still needs the credential.
    for args in [&["gate.pair"][..], &["--role", "app", "status"]] {
        let run = call(&home, &good, args);
        assert_eq!(run.code(), Some(1), "{args:?}: {run:?}");
        assert_eq!(run.error_code(), "not_gate", "{args:?}");
    }
    // The unsigned test client, with the right credential, is refused.
    let run = home.rlxd(&[
        "call",
        "--role",
        "app",
        "--gate-credential",
        &credential,
        "status",
    ]);
    assert_eq!(run.error_code(), "not_gate", "{run:?}");
    assert!(
        home.log()
            .contains("does not satisfy the gate's requirement")
    );
}

#[test]
fn a_helper_with_another_identifier_is_refused() {
    let home = home();
    let other = helper(
        &helpers(&home),
        "other",
        &Signing {
            identifier: "studio.aisquare.rlx.other",
            runtime: true,
            entitlements: &[],
            identity: None,
        },
    );
    home.start();
    let run = call(&home, &other, &["gate.pair"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "not_gate");
    assert!(
        !home.gate_json().exists(),
        "a first pairing it could not make"
    );
}

#[test]
fn without_the_hardened_runtime_or_with_an_injection_entitlement_a_helper_is_refused() {
    // Task acceptance 3 (vi), and the injection entitlements.
    let home = home();
    let good = good(&home);
    let mut refused = vec![(
        "no runtime",
        helper(
            &helpers(&home),
            "no-runtime",
            &Signing {
                identifier: TEST_IDENTIFIER,
                runtime: false,
                entitlements: &[],
                identity: None,
            },
        ),
    )];
    for (n, entitlement) in INJECTION_ENTITLEMENTS.iter().enumerate() {
        let path = helper(
            &helpers(&home),
            &format!("entitled-{n}"),
            &Signing {
                identifier: TEST_IDENTIFIER,
                runtime: true,
                entitlements: &[entitlement],
                identity: None,
            },
        );
        refused.push((entitlement, path));
    }
    home.start();
    // Each refused helper fails a first pairing; then, with the passing helper's valid
    // credential, its hello and a re-pairing fail too.
    for (why, path) in &refused {
        let run = call(&home, path, &["gate.pair"]);
        assert_eq!(run.error_code(), "not_gate", "{why}: {run:?}");
    }
    let credential = pair(&home, &good);
    for (why, path) in &refused {
        let pair_again = format!(r#"{{"gate_credential":"{credential}"}}"#);
        for args in [
            &["--role", "app", "--gate-credential", &credential, "status"][..],
            &["gate.pair", &pair_again],
        ] {
            let run = call(&home, path, args);
            assert_eq!(run.code(), Some(1), "{why} {args:?}: {run:?}");
            assert_eq!(run.error_code(), "not_gate", "{why} {args:?}");
        }
    }
    let log = home.log();
    assert!(
        log.contains("does not run with the hardened runtime"),
        "{log}"
    );
    for entitlement in INJECTION_ENTITLEMENTS {
        assert!(
            log.contains(&format!("holds the entitlement {entitlement}")),
            "{entitlement}:\n{log}"
        );
    }
    let run = call(
        &home,
        &good,
        &["--role", "app", "--gate-credential", &credential, "status"],
    );
    assert_eq!(
        run.code(),
        Some(0),
        "the passing helper still passes: {run:?}"
    );
}

#[test]
fn nothing_executes_codesign() {
    // The execve probe: rlxd runs in a sandbox that denies executing any `codesign`, and
    // with a `codesign` first on its PATH that records being run. The passing helper
    // must still pass, so the check never needed it.
    let shim_dir = std::env::temp_dir().join(format!("rlxd-codesign-shim-{}", std::process::id()));
    std::fs::create_dir_all(&shim_dir).unwrap();
    let marker = shim_dir.join("ran");
    let shim = shim_dir.join("codesign");
    std::fs::write(
        &shim,
        format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        shim_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let home = home().with_env("PATH", &path);
    let good = good(&home);
    let profile = "(version 1)(allow default)(deny process-exec (regex #\"/codesign$\"))";
    let rlxd = env!("CARGO_BIN_EXE_rlxd");
    let run = home.run_of(
        Path::new("/usr/bin/sandbox-exec"),
        &["-p", profile, rlxd, "serve", "--detach"],
    );
    assert_eq!(run.code(), Some(0), "{run:?}");
    let credential = pair(&home, &good);
    let run = call(
        &home,
        &good,
        &["--role", "app", "--gate-credential", &credential, "status"],
    );
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert!(
        !marker.exists(),
        "rlxd ran codesign: {:?}",
        std::fs::read_to_string(&marker)
    );
    let binary = std::fs::read(rlxd).unwrap();
    assert!(
        !binary.windows(b"codesign".len()).any(|w| w == b"codesign"),
        "rlxd's binary names codesign"
    );
    let _ = std::fs::remove_dir_all(&shim_dir);
}

/// Set in the helper process: the descriptor to hand its connection on over.
const HELPER_FD: &str = "RLXD_GATE_HELPER_FD";
/// Set in the helper process: the signed binary it then becomes.
const HELPER_EXEC: &str = "RLXD_GATE_HELPER_EXEC";

/// The binary `pid` runs now.
fn pid_path(pid: u32) -> Option<PathBuf> {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buffer` holds as many bytes as the call is told.
    let length = unsafe {
        libc::proc_pidpath(
            i32::try_from(pid).ok()?,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
        )
    };
    let length = usize::try_from(length).ok().filter(|n| *n > 0)?;
    Some(PathBuf::from(
        String::from_utf8_lossy(&buffer[..length]).into_owned(),
    ))
}

#[test]
fn a_connection_handed_on_by_a_process_that_then_execs_the_app_is_refused() {
    // Review #1991, item 3: an unsigned process connects, hands its connection to its
    // parent, and execs the signed app in the same pid. The parent's hello on that
    // connection, with a valid credential, must not pass as the app.
    let home = home();
    let good = good(&home);
    home.start();
    let credential = pair(&home, &good);
    let (mine, theirs) = UnixStream::pair().unwrap();
    // SAFETY: clears FD_CLOEXEC on a descriptor this test owns, for the helper.
    assert_eq!(
        unsafe { libc::fcntl(theirs.as_raw_fd(), libc::F_SETFD, 0) },
        0
    );
    let me = std::env::current_exe().unwrap();
    let mut helper = home
        .command_of(
            &me,
            &[
                "--exact",
                "helper_connects_hands_on_and_execs",
                "--nocapture",
                "--test-threads=1",
            ],
        )
        .env(HELPER_FD, theirs.as_raw_fd().to_string())
        .env(HELPER_EXEC, &good)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(theirs);
    let passed = UnixStream::from(receive_fd(mine.as_raw_fd()));
    let good_path = std::fs::canonicalize(&good).unwrap();
    eventually("the helper runs the signed app", || {
        pid_path(helper.id()).is_some_and(|p| p == good_path)
    });
    let reply = raw_hello(&passed, &home.token(), &credential);
    assert_eq!(reply["error"]["code"], "not_gate", "{reply}");
    // The signed app, now in the helper's pid, reads its stdin to the end and exits.
    drop(helper.stdin.take());
    let _ = helper.wait();
    let log = home.log();
    let refusal = log
        .lines()
        .find(|l| l.contains("hello refused: role app") && l.contains("LOCAL_PEERTOKEN"))
        .unwrap_or_else(|| panic!("refused on the audit token taken at connect:\n{log}"));
    eprintln!("{refusal}");
}

/// Not a test of its own: the helper of the test above, in a process of its own. It
/// connects, hands that connection to its parent over `SCM_RIGHTS`, and execs the signed
/// app, which keeps its pid.
#[test]
fn helper_connects_hands_on_and_execs() {
    let Ok(fd) = std::env::var(HELPER_FD) else {
        return;
    };
    let fd: RawFd = fd.parse().unwrap();
    let exec = std::env::var(HELPER_EXEC).unwrap();
    let home = Home::from_env().unwrap();
    let stream = UnixStream::connect(home.socket()).unwrap();
    // Long enough for rlxd to accept the connection and pin this process.
    std::thread::sleep(Duration::from_millis(300));
    send_fd(fd, stream.as_raw_fd());
    let error = Command::new(exec).args(["call", "--raw"]).exec();
    panic!("exec: {error}");
}
