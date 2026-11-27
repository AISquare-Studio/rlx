//! What a release build leaves out. This builds rlxd as a release does (`cargo build
//! --release`, [`release_rlxd`]), without the features only rlxd's dev-dependency on
//! itself turns on, and checks against the test build the other tests run:
//!
//! - check 7 of task #22 (handoff 0.10.3, 7.3): no test-only methods, so `debug.emit` is
//!   `unknown_method`, and so is `debug.pty_panic` (#2479);
//! - handoff 0.10.3 rev 7, 5.1, "No dev mode in a release build": no `--gate-dev-mode`
//!   flag, `gate.dev_mode` in the config ignored, no marker in `rlxd --version` or in
//!   status, and not even the feature's name in the binary;
//! - no `fake-claude`: the runner tests' stand-in for Claude Code is a binary only the
//!   test-only `fake-claude` feature builds.

mod common;

use common::{TestHome, release_rlxd};

#[test]
fn check_7_a_release_build_has_no_debug_methods() {
    let release = TestHome::new().with_binary(release_rlxd());
    release.start();
    let run = release.rlxd(&["call", "debug.emit", r#"{"count":1}"#]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "unknown_method");
    // Nor the method that makes a PTY's screen model panic.
    let panic = release.rlxd(&["call", "debug.pty_panic", r#"{"pty_id":"0","at":"output"}"#]);
    assert_eq!(panic.error_code(), "unknown_method", "{panic:?}");
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
    let run = test_build.rlxd(&["call", "debug.pty_panic", r#"{"pty_id":"0","at":"output"}"#]);
    assert_eq!(
        run.error_code(),
        "unknown_pty",
        "the test build has it: {run:?}"
    );
}

/// The manager's decision on #2641's licence note: every rlxd binary carries tmux's ISC
/// notice for each file ported from it, and `rlxd licenses` prints it, a release
/// build's as a test build's. Each file that says it is ported from tmux is named in it.
#[test]
fn a_release_build_carries_tmuxs_notice_for_every_ported_file() {
    let notices = std::fs::read_to_string(common::workspace().join("THIRD_PARTY_NOTICES")).unwrap();
    for binary in [
        release_rlxd(),
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_rlxd")),
    ] {
        let run = TestHome::new()
            .with_binary(binary.clone())
            .rlxd(&["licenses"]);
        assert_eq!(run.code(), Some(0), "{binary:?}: {run:?}");
        assert_eq!(
            run.stdout, notices,
            "{binary:?} prints THIRD_PARTY_NOTICES as it is"
        );
    }
    let src = common::workspace().join("crates/rlxd/src");
    let mut ported = Vec::new();
    let mut dirs = vec![src.clone()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if std::fs::read_to_string(&path).is_ok_and(|text| {
                text.lines().take(3).any(|line| {
                    line.contains("from tmux 3.8")
                        || line.contains("ported from the capture loop of tmux")
                })
            }) {
                ported.push(path);
            }
        }
    }
    assert!(ported.len() >= 6, "{ported:?}");
    for path in ported {
        let name = path
            .strip_prefix(common::workspace())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        assert!(
            notices.contains(&format!("- rlxd/{name}:")),
            "{name} is not in THIRD_PARTY_NOTICES"
        );
    }
    for holder in [
        "Copyright (c) 2007 Nicholas Marriott",
        "Copyright (c) 2021 Will",
        "Copyright (c) 2022 Jeff Chiang",
        "Copyright (c) 2009 Jonathan Alvarado",
        "Copyright (c) 2023 Nicholas Marriott",
    ] {
        assert!(notices.contains(holder), "{holder}");
    }
    assert_eq!(
        notices.matches("Permission to use, copy, modify").count(),
        13
    );
}

#[test]
fn a_release_build_has_no_gate_dev_mode() {
    let version = env!("CARGO_PKG_VERSION");
    let binary = release_rlxd();
    let release = TestHome::new().with_binary(binary.clone());
    let run = release.rlxd(&["--version"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(run.stdout.trim_end(), format!("rlxd {version}"));

    // The flag is unknown: a usage error, and no rlxd was left running.
    let run = release.rlxd(&["serve", "--detach", "--gate-dev-mode"]);
    assert_eq!(run.code(), Some(2), "{run:?}");
    assert_eq!(release.rlxd(&["call", "status"]).code(), Some(3));

    // The config's key is ignored: the gate takes signed apps only, so this unsigned
    // client can't make even the first pairing.
    std::fs::create_dir_all(release.root().join("rlxd")).unwrap();
    std::fs::write(
        release.root().join("rlxd").join("config.toml"),
        "[gate]\ndev_mode = true\n",
    )
    .unwrap();
    release.start();
    let status = release.status();
    assert_eq!(status["gate"], "signed app only", "{status}");
    assert!(status.get("dev_features").is_none(), "{status}");
    assert!(release.log().contains("ignoring gate.dev_mode"));
    let run = release.rlxd(&["call", "gate.pair"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "not_gate");
    assert!(!release.gate_json().exists(), "nothing was paired");

    // Not even the feature's name is in the binary (`strings rlxd | grep -cF ...`).
    let bytes = std::fs::read(&binary).unwrap();
    let needle = b"gate-dev-mode";
    assert_eq!(
        bytes.windows(needle.len()).filter(|w| w == needle).count(),
        0
    );

    // The test build has it all, and says so.
    let test_build = TestHome::new();
    let run = test_build.rlxd(&["--version"]);
    assert_eq!(
        run.stdout.trim_end(),
        format!("rlxd {version} (dev: gate-dev-mode)")
    );
    test_build.start_dev();
    let status = test_build.status();
    assert_eq!(status["gate"], "dev mode (unsigned app accepted)");
    assert_eq!(status["dev_features"], serde_json::json!(["gate-dev-mode"]));
}

#[test]
fn a_release_build_has_no_fake_claude() {
    // Every binary of the package that a release build makes, as `cargo build --release
    // -p rlxd` makes them: fake-claude needs the test-only feature, so it is not one.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = std::process::Command::new(cargo)
        .args([
            "build",
            "--release",
            "--locked",
            "--package",
            "rlxd",
            "--bins",
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(common::workspace())
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .output()
        .expect("cargo runs");
    assert!(output.status.success(), "cargo build --release failed");
    let built: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact")
        .filter(|message| message["executable"].is_string())
        .filter_map(|message| message["target"]["name"].as_str().map(str::to_owned))
        .collect();
    assert!(built.iter().any(|name| name == "rlxd"), "{built:?}");
    assert!(!built.iter().any(|name| name == "fake-claude"), "{built:?}");
}
