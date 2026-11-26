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

mod common;
mod macos_signing;

use std::path::{Path, PathBuf};

use common::TestHome;
use macos_signing::{INJECTION_ENTITLEMENTS, Signing, TEST_IDENTIFIER, helper, test_requirement};

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
