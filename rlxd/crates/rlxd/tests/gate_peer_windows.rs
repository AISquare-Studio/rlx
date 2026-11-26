//! The Windows gate peer check (handoff 0.10.3 rev 6, 5.1 Accept, "Windows"; task
//! acceptance 4), with rlxd outside dev mode:
//!
//! ```text
//! cargo test -p rlxd --features gate-test-requirement --test gate_peer_windows
//! ```
//!
//! A test signer, a self-signed code-signing certificate whose root is put in
//! `Cert:\LocalMachine\Root`, signs each helper. `RLXD_TEST_GATE_REQUIREMENT` names that
//! signer and the helper's path as the expected publisher and binary. Trusting a root
//! needs an administrator, as CI's runner is: elsewhere these tests are skipped, with a
//! note, unless `RLXD_GATE_PEER_REQUIRED` is set, as in CI, where they fail instead.
//!
//! A pid reused under a connection, with a later creation time, is refused by the
//! comparison `peer::windows` unit-tests: a test can't make Windows reuse a pid on
//! demand. The held-handle test is the live form of it: its helper has exited.
#![cfg(windows)]
#![allow(unsafe_code)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use common::TestHome;
use rlxd_client::proto::{HelloParams, Role};
use rlxd_client::{Connection, Home};
use serde_json::{Value, json};

/// The test signer's subject name, the expected publisher.
const SIGNER: &str = "rlxd gate test";

/// Set in the helper process: the parent's pid, to hand the pipe to.
const HELPER_PARENT: &str = "RLXD_GATE_HELPER_PARENT";
/// Set in the helper process: the credential its control hello presents.
const HELPER_CREDENTIAL: &str = "RLXD_GATE_HELPER_CREDENTIAL";

/// The test signer: its certificate in the user's store, and its root trusted by the
/// machine. Dropping it removes both.
struct TestSigner {
    thumbprint: String,
}

impl TestSigner {
    /// The signer, or `None` (with a note) where no root can be trusted.
    fn new() -> Option<Self> {
        let made = powershell(&format!(
            "$c = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN={SIGNER}' \
             -CertStoreLocation Cert:\\CurrentUser\\My -NotAfter (Get-Date).AddDays(2); \
             $f = Join-Path ([IO.Path]::GetTempPath()) ('rlxd-gate-' + $c.Thumbprint + '.cer'); \
             Export-Certificate -Cert $c -FilePath $f | Out-Null; \
             try {{ Import-Certificate -FilePath $f -CertStoreLocation Cert:\\LocalMachine\\Root \
             -ErrorAction Stop | Out-Null }} catch {{ Remove-Item Cert:\\CurrentUser\\My\\$($c.Thumbprint); \
             Remove-Item $f; throw }}; Remove-Item $f; $c.Thumbprint"
        ));
        // PowerShell may print more before it; the thumbprint is the last line.
        let thumbprint = made.map(|out| {
            out.lines()
                .map(str::trim)
                .rfind(|line| !line.is_empty())
                .unwrap_or_default()
                .to_owned()
        });
        match thumbprint {
            Ok(thumbprint) => {
                assert!(
                    thumbprint.len() == 40 && thumbprint.bytes().all(|b| b.is_ascii_hexdigit()),
                    "not a thumbprint: {thumbprint:?}"
                );
                Some(Self { thumbprint })
            }
            Err(why) => {
                assert!(
                    std::env::var_os("RLXD_GATE_PEER_REQUIRED").is_none(),
                    "a test signer is required here: {why}"
                );
                eprintln!("skipped: no test signer's root can be trusted here: {why}");
                None
            }
        }
    }

    /// Signs `path` with Authenticode.
    fn sign(&self, path: &Path) {
        let signed = powershell(&format!(
            "$c = Get-Item Cert:\\CurrentUser\\My\\{}; \
             $r = Set-AuthenticodeSignature -FilePath '{}' -Certificate $c -HashAlgorithm SHA256; \
             if ($r.Status -ne 'Valid') {{ throw $r.StatusMessage }}",
            self.thumbprint,
            path.display()
        ));
        if let Err(why) = signed {
            panic!("signing {}: {why}", path.display());
        }
    }
}

impl Drop for TestSigner {
    fn drop(&mut self) {
        let _ = powershell(&format!(
            "Remove-Item Cert:\\LocalMachine\\Root\\{0} -ErrorAction SilentlyContinue; \
             Remove-Item Cert:\\CurrentUser\\My\\{0} -ErrorAction SilentlyContinue",
            self.thumbprint
        ));
    }
}

/// Runs `script` in Windows PowerShell: its stdout, or why it failed.
fn powershell(script: &str) -> Result<String, String> {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// A fresh home whose rlxd, outside dev mode, expects `binary` signed by the test signer.
fn home_expecting(binary: &Path) -> TestHome {
    TestHome::new().with_env(
        "RLXD_TEST_GATE_REQUIREMENT",
        &format!("{SIGNER}|{}", binary.display()),
    )
}

/// `binary`, copied to `dir/name`.
fn copy(binary: &Path, dir: &Path, name: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::copy(binary, &path).unwrap();
    path
}

#[test]
fn a_signed_helper_at_the_expected_path_passes_and_still_needs_the_credential() {
    let Some(signer) = TestSigner::new() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("rlxd-gate-win-{}", std::process::id()));
    let rlxd = Path::new(env!("CARGO_BIN_EXE_rlxd"));
    let helper = copy(rlxd, &dir, "rlx.exe");
    signer.sign(&helper);
    let home = home_expecting(&helper);
    home.start();
    let run = home.run_of(&helper, &["call", "gate.pair"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let credential = run.data()["gate_credential"].as_str().unwrap().to_owned();
    let app = [
        "call",
        "--role",
        "app",
        "--gate-credential",
        &credential,
        "status",
    ];
    let run = home.run_of(&helper, &app);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(run.data()["clients"]["app"], 1);
    assert!(
        home.log().contains("GetNamedPipeClientProcessId: pid"),
        "{}",
        home.log()
    );
    // A passing peer still needs the credential.
    let run = home.run_of(&helper, &["call", "gate.pair"]);
    assert_eq!(run.error_code(), "not_gate", "{run:?}");
    // Another binary, with the credential, is refused.
    let run = home.rlxd(&app);
    assert_eq!(run.error_code(), "not_gate", "{run:?}");
    // The path isn't enough: an unsigned copy at that same path is refused.
    std::fs::copy(rlxd, &helper).unwrap();
    let run = home.run_of(&helper, &app);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "not_gate");
    assert!(
        home.log().contains("signature does not verify"),
        "{}",
        home.log()
    );
    drop(home);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pipe_whose_client_has_exited_is_refused() {
    // "A held handle": the helper connects, hands the pipe to this process with
    // DuplicateHandle, and exits; a hello on that pipe is then refused.
    let Some(signer) = TestSigner::new() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("rlxd-gate-win-held-{}", std::process::id()));
    let helper = copy(&std::env::current_exe().unwrap(), &dir, "gate-helper.exe");
    signer.sign(&helper);
    let home = home_expecting(&helper);
    home.start_dev();
    let run = home.rlxd(&["call", "gate.pair"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let credential = run.data()["gate_credential"].as_str().unwrap().to_owned();
    home.stop();
    home.start();
    let out = home
        .command_of(
            &helper,
            &[
                "--exact",
                "helper_hands_its_pipe_to_its_parent",
                "--nocapture",
                "--test-threads=1",
            ],
        )
        .env(HELPER_PARENT, std::process::id().to_string())
        .env(HELPER_CREDENTIAL, &credential)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "the helper: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The test harness may print on the same line first.
    let handle: usize = stdout
        .lines()
        .find_map(|line| line.split_once("PIPE ").map(|(_, handle)| handle))
        .unwrap_or_else(|| panic!("the helper handed no pipe: {stdout}"))
        .trim()
        .parse()
        .unwrap();
    // SAFETY: the helper duplicated this handle into this process, for this test alone.
    let pipe = unsafe { std::fs::File::from_raw_handle(handle as _) };
    let hello = json!({"id": 1, "method": "hello", "params": {
        "token": home.token(), "protocol_version": 1, "client": "gate-peer-windows",
        "client_version": "0", "role": "app", "gate_credential": credential,
        "capabilities": [],
    }});
    (&pipe).write_all(format!("{hello}\n").as_bytes()).unwrap();
    let mut line = String::new();
    BufReader::new(&pipe).read_line(&mut line).unwrap();
    let reply: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"));
    assert_eq!(reply["error"]["code"], "not_gate", "{reply}");
    assert!(home.log().contains("has exited"), "{}", home.log());
    drop(pipe);
    drop(home);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Not a test of its own: the helper `a_pipe_whose_client_has_exited_is_refused` runs,
/// signed, in a process of its own. It proves it passes the peer check, connects, hands
/// that pipe to its parent with `DuplicateHandle`, prints the handle, and exits.
#[test]
fn helper_hands_its_pipe_to_its_parent() {
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE,
    };

    let Ok(parent) = std::env::var(HELPER_PARENT) else {
        return;
    };
    let parent: u32 = parent.parse().unwrap();
    let home = Home::from_env().unwrap();
    let token = std::fs::read_to_string(home.token()).unwrap();
    let credential = std::env::var(HELPER_CREDENTIAL).unwrap();
    let hello = HelloParams::new(token.trim(), "gate-helper", "0", Role::App)
        .unwrap()
        .with_gate_credential(credential)
        .unwrap();
    let reply = Connection::open(&home).unwrap().hello(&hello).unwrap();
    assert!(
        reply.is_ok(),
        "the helper passes while it runs: {}",
        reply.line
    );

    // Every instance can be busy for a moment, between rlxd's accept and its next one.
    let pipe = (0..500)
        .find_map(|_| {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(home.pipe_name())
            {
                Ok(pipe) => Some(pipe),
                Err(e) if e.raw_os_error() == Some(231) => {
                    std::thread::sleep(Duration::from_millis(10));
                    None
                }
                Err(e) => panic!("cannot open {}: {e}", home.pipe_name()),
            }
        })
        .expect("a pipe instance came free");
    // Long enough for rlxd to accept the connection and pin this process.
    std::thread::sleep(Duration::from_millis(300));
    // SAFETY: opens the parent for handle duplication; checked below.
    let target = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, parent) };
    assert!(
        !target.is_null(),
        "OpenProcess: {}",
        std::io::Error::last_os_error()
    );
    let mut duplicate: HANDLE = std::ptr::null_mut();
    // SAFETY: `pipe` is open in this process, `target` is the parent, and `duplicate`
    // receives the parent's new handle.
    let made = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            pipe.as_raw_handle() as HANDLE,
            target,
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    assert_ne!(
        made,
        0,
        "DuplicateHandle: {}",
        std::io::Error::last_os_error()
    );
    println!("\nPIPE {}", duplicate as usize);
}
