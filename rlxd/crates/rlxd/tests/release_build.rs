//! Check 7 of task #22 (handoff 0.10.3, 7.3): the test-only methods are absent from a
//! release build. This builds rlxd as a release does (`cargo build --release`, which
//! leaves out the `debug-methods` feature that only rlxd's dev-dependency on itself turns
//! on), starts it in a fresh home and asks it for `debug.emit`: `unknown_method`. The
//! test build the other tests run answers the same call. The test build's stand-in gate
//! device (task #13's tests) is checked the same way.
//!
//! The build runs in the workspace's own target folder, so rlxd.yml's later
//! `cargo build --release` finds it done. Set `CARGO_BUILD_JOBS` to bound it.

mod common;

use std::path::PathBuf;
use std::process::{Command, Stdio};

use common::{TestHome, workspace};
use serde_json::Value;

/// rlxd, built for release: the executable `cargo build --release` reports.
fn release_rlxd() -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "build",
            "--release",
            "--locked",
            "--package",
            "rlxd",
            "--bin",
            "rlxd",
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(workspace())
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .expect("cargo runs");
    assert!(output.status.success(), "cargo build --release failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact")
        .filter(|message| message["target"]["name"] == "rlxd")
        .find_map(|message| message["executable"].as_str().map(PathBuf::from))
        .expect("cargo names the rlxd executable it built")
}

#[test]
fn check_7_a_release_build_has_no_debug_methods() {
    let binary = release_rlxd();
    let release = TestHome::new().with_binary(binary.clone());
    release.start();
    let run = release.rlxd(&["call", "debug.emit", r#"{"count":1}"#]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "unknown_method");
    let hint = run.json()["error"]["hint"].as_str().unwrap().to_owned();
    assert!(
        hint.contains("events.subscribe") && !hint.contains("debug"),
        "the methods it serves: {hint}"
    );
    assert_eq!(
        release.status()["seq"],
        1,
        "it published daemon.started only"
    );

    let test_build = TestHome::new();
    test_build.start();
    let run = test_build.rlxd(&["call", "debug.emit", r#"{"count":1}"#]);
    assert_eq!(run.code(), Some(0), "the test build has it: {run:?}");

    // Task #13: the test build's stand-in gate device is not in a release build either.
    // With RLXD_TEST_GATE_CREDENTIAL set and the credential sent, an app-role hello is
    // still not_gate there, and so send_safety_mode can't be set.
    let gated = TestHome::new().with_binary(binary).with_test_gate("d");
    gated.start();
    let run = gated.rlxd(&["call", "--as-app", "settings.list"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "not_gate");
    let test_gated = TestHome::new().with_test_gate("d");
    test_gated.start();
    let run = test_gated.rlxd(&["call", "--as-app", "settings.list"]);
    assert_eq!(run.code(), Some(0), "the test build takes it: {run:?}");
}
