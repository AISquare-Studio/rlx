//! The Windows gate peer check (handoff 0.10.3 rev 6, 5.1 Accept, "Windows"; task
//! acceptance 4), with rlxd outside dev mode:
//!
//! ```text
//! cargo test -p rlxd --features gate-test-requirement --test gate_peer_windows
//! ```
//!
//! A test signer, a self-signed code-signing certificate whose root is put in
//! `Cert:\LocalMachine\Root`, signs each helper. There is one for the whole binary, made
//! by the first test that needs it, with a subject no other run's signer has, and it is
//! removed when the binary exits. `RLXD_TEST_GATE_REQUIREMENT` names that signer and the
//! helper's path as the expected publisher and binary. Trusting a root needs an
//! administrator, as CI's runner is: elsewhere these tests are skipped, with a note,
//! unless `RLXD_GATE_PEER_REQUIRED` is set, as in CI, where they fail instead.
//!
//! A pid reused under a connection, with a later creation time, is refused by the
//! comparison `peer::windows` unit-tests: a test can't make Windows reuse a pid on
//! demand. The held-handle test is the live form of it: its helper has exited.
#![cfg(windows)]
#![allow(unsafe_code)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::TestHome;
use rlxd_client::proto::{HelloParams, Role};
use rlxd_client::{Connection, Home};
use serde_json::{Value, json};

/// Every test signer's subject starts with this.
const SUBJECT_PREFIX: &str = "CN=rlxd gate test";

/// Set in the helper process: the parent's pid, to hand the pipe to.
const HELPER_PARENT: &str = "RLXD_GATE_HELPER_PARENT";
/// Set in the helper process: the credential its control hello presents.
const HELPER_CREDENTIAL: &str = "RLXD_GATE_HELPER_CREDENTIAL";

/// Makes a test signer whose subject is `{subject}` and trusts its root, with `{extra}`
/// appended to New-SelfSignedCertificate's parameters (where a test injects a failure).
/// Its last line is `THUMBPRINT <thumbprint>`. Any error, or no certificate made, stops it,
/// and it exits 1 with the error on stderr, on one line. (In run 45852371615 attempt 1 its
/// predecessor exited 0 with nothing on stdout, and nothing said why.) First it removes
/// test signers that have expired, left by runs whose cleanup never ran
/// ([`TestSigner::remove_at_exit`]). Statements end in `;`, and there are no comments,
/// so that it means the same on one line.
const CREATE: &str = r"$ErrorActionPreference = 'Stop';
$ProgressPreference = 'SilentlyContinue';
try {
    try {
        Get-ChildItem Cert:\CurrentUser\My, Cert:\LocalMachine\Root |
            Where-Object { $_.Subject -like '{prefix}*' -and $_.NotAfter -lt (Get-Date) } |
            Remove-Item -ErrorAction SilentlyContinue;
    } catch {};
    $c = New-SelfSignedCertificate -Type CodeSigningCert -Subject '{subject}' -CertStoreLocation Cert:\CurrentUser\My -NotAfter (Get-Date).AddDays(1) {extra};
    if (-not $c -or -not $c.Thumbprint) { throw 'New-SelfSignedCertificate made no certificate' };
    $f = Join-Path ([IO.Path]::GetTempPath()) ('rlxd-gate-' + $c.Thumbprint + '.cer');
    try {
        Export-Certificate -Cert $c -FilePath $f | Out-Null;
        Import-Certificate -FilePath $f -CertStoreLocation Cert:\LocalMachine\Root | Out-Null;
    } catch {
        Remove-Item -LiteralPath ('Cert:\CurrentUser\My\' + $c.Thumbprint) -ErrorAction SilentlyContinue;
        throw;
    } finally {
        Remove-Item -LiteralPath $f -ErrorAction SilentlyContinue;
    };
    'THUMBPRINT ' + $c.Thumbprint;
} catch {
    [Console]::Error.WriteLine('rlxd-gate-signer: ' + $_.Exception.Message + ' (' + $_.FullyQualifiedErrorId + ')');
    exit 1;
}";

/// Signs `{path}` with Authenticode as the test signer `{thumbprint}`. It exits 1, with
/// the error on stderr, unless the signature is valid.
const SIGN: &str = r"$ErrorActionPreference = 'Stop';
$ProgressPreference = 'SilentlyContinue';
try {
    $c = Get-Item -LiteralPath 'Cert:\CurrentUser\My\{thumbprint}';
    $r = Set-AuthenticodeSignature -FilePath '{path}' -Certificate $c -HashAlgorithm SHA256;
    if ($r.Status -ne 'Valid') { throw ('the signature is ' + $r.Status + ': ' + $r.StatusMessage) };
} catch {
    [Console]::Error.WriteLine('rlxd-gate-signer: ' + $_.Exception.Message + ' (' + $_.FullyQualifiedErrorId + ')');
    exit 1;
}";

/// Held while a PowerShell script runs, so that this binary's tests make, sign with and
/// remove certificates one at a time.
static POWERSHELL: Mutex<()> = Mutex::new(());

/// The test signer: a self-signed code-signing certificate in the user's store, whose
/// root the machine trusts. There is one for the binary ([`TestSigner::shared`]), removed
/// when it exits ([`TestSigner::remove_at_exit`]).
struct TestSigner {
    /// Its full subject and, self-signed, its issuer: this run's alone.
    subject: String,
    /// Its SHA-1 thumbprint: 40 hex digits.
    thumbprint: String,
}

/// The binary's test signer, or why it has none: made once, by the first test to ask.
static SHARED: OnceLock<Result<TestSigner, String>> = OnceLock::new();

impl TestSigner {
    /// The binary's test signer, or `None` (with a note) where no root can be trusted.
    ///
    /// One for every test: run 45852371615 attempt 1 failed while three tests each made
    /// one, with the same subject, at the same moment on a cold runner.
    fn shared() -> Option<&'static Self> {
        let made = SHARED.get_or_init(|| {
            let made = Self::create(&unique_subject("signer"), "");
            // Past the harness's capture, so that every run's log shows the one signer.
            let _ = match &made {
                Ok(signer) => writeln!(
                    std::io::stderr(),
                    "gate_peer_windows: made the test signer {:?} ({}), for all of this \
                     binary's tests; it is removed when the binary exits",
                    signer.subject,
                    signer.thumbprint
                ),
                Err(why) => writeln!(std::io::stderr(), "gate_peer_windows: {why}"),
            };
            made
        });
        match made {
            Ok(signer) => Some(signer),
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

    /// Makes a signer whose subject is `subject`, and trusts its root, with `extra`
    /// appended to New-SelfSignedCertificate's parameters ([`CREATE`]). Its error carries
    /// PowerShell's exit status, stderr and stdout.
    fn create(subject: &str, extra: &str) -> Result<Self, String> {
        let ran = powershell(
            &CREATE
                .replace("{prefix}", SUBJECT_PREFIX)
                .replace("{subject}", subject)
                .replace("{extra}", extra),
        )
        .map_err(|why| format!("making the test signer {subject:?}: {why}"))?;
        let thumbprint = ran
            .stdout
            .lines()
            .find_map(|line| line.trim().strip_prefix("THUMBPRINT "))
            .map(str::trim)
            .filter(|t| t.len() == 40 && t.bytes().all(|b| b.is_ascii_hexdigit()));
        let (true, Some(thumbprint)) = (ran.status.success(), thumbprint) else {
            return Err(format!("making the test signer {subject:?}: {ran}"));
        };
        let signer = Self {
            subject: subject.to_owned(),
            thumbprint: thumbprint.to_owned(),
        };
        signer.remove_at_exit()?;
        Ok(signer)
    }

    /// Removes the signer, and its trusted root, once this process has exited, however it
    /// exits: a static ([`SHARED`]) is never dropped, and a failed run ends with
    /// `process::exit`. A PowerShell of its own waits for this process, then removes them;
    /// where it can't start, they are removed now, and this is an error.
    fn remove_at_exit(&self) -> Result<(), String> {
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

        let script = format!(
            "Wait-Process -Id {} -ErrorAction SilentlyContinue; {}",
            std::process::id(),
            self.removal()
        );
        let mut command = Command::new("powershell");
        command
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // A console of its own, so that a Ctrl+C that ends the tests doesn't end it.
            .creation_flags(CREATE_NO_WINDOW);
        match common::launch(&mut command) {
            // It outlives this process; dropping it leaves it running.
            Ok(_waiting) => Ok(()),
            Err(e) => {
                let removed = match powershell(&self.removal()) {
                    Ok(ran) => ran.to_string(),
                    Err(why) => why,
                };
                Err(format!(
                    "cannot start the test signer's removal ({e}), so it was removed now: \
                     {removed}"
                ))
            }
        }
    }

    /// A script that removes the signer and its trusted root.
    fn removal(&self) -> String {
        format!(
            "Remove-Item -LiteralPath 'Cert:\\LocalMachine\\Root\\{0}' -ErrorAction SilentlyContinue; \
             Remove-Item -LiteralPath 'Cert:\\CurrentUser\\My\\{0}' -ErrorAction SilentlyContinue",
            self.thumbprint
        )
    }

    /// Signs `path` with Authenticode.
    fn sign(&self, path: &Path) {
        let script = SIGN
            .replace("{thumbprint}", &self.thumbprint)
            .replace("{path}", &path.display().to_string());
        match powershell(&script) {
            Ok(ran) if ran.status.success() => {}
            Ok(ran) => panic!("signing {}: {ran}", path.display()),
            Err(why) => panic!("signing {}: {why}", path.display()),
        }
    }
}

/// A subject no other run's test signer has: [`SUBJECT_PREFIX`], then `what`, this
/// process and the time.
fn unique_subject(what: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    format!("{SUBJECT_PREFIX} {what} {} {millis}", std::process::id())
}

/// What a PowerShell script did.
struct Ran {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl std::fmt::Display for Ran {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PowerShell's {}, stderr {:?}, stdout {:?}",
            self.status,
            self.stderr.trim(),
            self.stdout.trim()
        )
    }
}

/// Runs `script` in Windows PowerShell, as no other test runs one ([`POWERSHELL`]): what it
/// did, or why it couldn't run.
fn powershell(script: &str) -> Result<Ran, String> {
    let _alone = POWERSHELL.lock().unwrap_or_else(PoisonError::into_inner);
    let mut command = Command::new("powershell");
    command
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = common::launch(&mut command)
        .and_then(Child::wait_with_output)
        .map_err(|e| format!("cannot run PowerShell: {e}"))?;
    Ok(Ran {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// A fresh home whose rlxd, outside dev mode, expects `binary` signed by `signer`.
fn home_expecting(signer: &TestSigner, binary: &Path) -> TestHome {
    TestHome::new().with_env(
        "RLXD_TEST_GATE_REQUIREMENT",
        &format!("{0}|{0}|{1}", signer.subject, binary.display()),
    )
}

/// `binary`, copied to `dir/name`.
fn copy(binary: &Path, dir: &Path, name: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::copy(binary, &path).unwrap();
    path
}

/// A test signer that can't be made is an error carrying PowerShell's exit status and
/// stderr. Run 45852371615 attempt 1 panicked with `not a thumbprint: ""` instead, its
/// stderr thrown away. Neither failure here makes a certificate, so this runs anywhere,
/// administrator or not.
#[test]
fn a_test_signer_that_cannot_be_made_is_an_error_with_powershells_stderr() {
    // An invalid parameter: New-SelfSignedCertificate refuses to run.
    let Err(why) = TestSigner::create(&unique_subject("invalid"), "-NoSuchParameter") else {
        panic!("made a test signer with an invalid parameter");
    };
    assert!(why.contains("exit code: 1"), "{why}");
    assert!(why.contains("rlxd-gate-signer: "), "{why}");
    assert!(why.contains("NoSuchParameter"), "{why}");
    // -WhatIf: no error, and no certificate. The script must not go on without one.
    let Err(why) = TestSigner::create(&unique_subject("what-if"), "-WhatIf") else {
        panic!("made a test signer with -WhatIf");
    };
    assert!(why.contains("exit code: 1"), "{why}");
    assert!(
        why.contains("rlxd-gate-signer: New-SelfSignedCertificate made no certificate"),
        "{why}"
    );
}

#[test]
fn a_signed_helper_at_the_expected_path_passes_and_still_needs_the_credential() {
    let Some(signer) = TestSigner::shared() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("rlxd-gate-win-{}", std::process::id()));
    let rlxd = Path::new(env!("CARGO_BIN_EXE_rlxd"));
    let helper = copy(rlxd, &dir, "rlx.exe");
    signer.sign(&helper);
    let home = home_expecting(signer, &helper);
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
fn an_image_renamed_away_while_it_runs_is_refused() {
    // Review #1991, item 4: an unsigned image runs from the expected path, is renamed
    // away, and the signed file is put back there before the check. The check reads the
    // file the process runs, not the one now at its path.
    let Some(signer) = TestSigner::shared() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("rlxd-gate-win-swap-{}", std::process::id()));
    let rlxd = Path::new(env!("CARGO_BIN_EXE_rlxd"));
    let helper = copy(rlxd, &dir, "rlx.exe");
    signer.sign(&helper);
    let home = home_expecting(signer, &helper);
    home.start();
    let run = home.run_of(&helper, &["call", "gate.pair"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let credential = run.data()["gate_credential"].as_str().unwrap().to_owned();

    let signed = dir.join("rlx-signed.exe");
    std::fs::rename(&helper, &signed).unwrap();
    std::fs::copy(rlxd, &helper).unwrap();
    let mut unsigned = home
        .command_of(&helper, &["call", "--raw"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    std::fs::rename(&helper, dir.join("rlx-unsigned-running.exe")).unwrap();
    std::fs::rename(&signed, &helper).unwrap();
    let hello = json!({"id": 1, "method": "hello", "params": {
        "token": home.token(), "protocol_version": 1, "client": "gate-peer-windows",
        "client_version": "0", "role": "app", "gate_credential": credential,
        "capabilities": [],
    }});
    let mut stdin = unsigned.stdin.take().unwrap();
    stdin.write_all(format!("{hello}\n").as_bytes()).unwrap();
    drop(stdin);
    let out = unsigned.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let reply: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("{e}: {stdout}{}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(reply["error"]["code"], "not_gate", "{reply}");
    // Refused for the file it runs, wherever that now is: the image's name follows the
    // rename on current Windows, so the path check refuses it, and the file binding is
    // the check behind that one.
    let log = home.log();
    let refusal = log
        .lines()
        .find(|l| l.contains("hello refused: role app"))
        .unwrap_or_else(|| panic!("{log}"));
    assert!(
        refusal.contains("rlx-unsigned-running.exe")
            || refusal.contains("its image was moved or replaced"),
        "{refusal}"
    );
    // The control: the signed file, now back at the path, still passes when it runs.
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
    drop(home);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pipe_whose_client_has_exited_is_refused() {
    // "A held handle": the helper connects, hands the pipe to this process with
    // DuplicateHandle, and exits; a hello on that pipe is then refused.
    let Some(signer) = TestSigner::shared() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("rlxd-gate-win-held-{}", std::process::id()));
    let helper = copy(&std::env::current_exe().unwrap(), &dir, "gate-helper.exe");
    signer.sign(&helper);
    let home = home_expecting(signer, &helper);
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
