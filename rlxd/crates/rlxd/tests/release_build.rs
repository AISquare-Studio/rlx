//! What a release build leaves out. This builds rlxd as a release does (`cargo build
//! --release`, [`release_rlxd`]), without the features only rlxd's dev-dependency on
//! itself turns on, and checks against the test build the other tests run:
//!
//! - check 7 of task #22 (handoff 0.10.3, 7.3): no test-only methods, so `debug.emit` is
//!   `unknown_method`;
//! - handoff 0.10.3 rev 7, 5.1, "No dev mode in a release build": no `--gate-dev-mode`
//!   flag, `gate.dev_mode` in the config ignored, no marker in `rlxd --version` or in
//!   status, and not even the feature's name in the binary;
//! - no `fake-claude`: the runner tests' stand-in for Claude Code is a binary only the
//!   test-only `fake-claude` feature builds;
//! - no scheduler test clock: no `debug.clock_advance` or `debug.clock_set`, and
//!   `RLXD_TEST_CLOCK` is not read, so its jobs run on the system's clock.

mod common;

use common::{TestHome, release_rlxd};

#[test]
fn check_7_a_release_build_has_no_debug_methods() {
    let release = TestHome::new().with_binary(release_rlxd());
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

    // Nor the runner's test-only start with a scheduler job_id, nor the scheduler's
    // test clock.
    let run = release.rlxd(&["call", "debug.run_start", r#"{"job_id":"j"}"#]);
    assert_eq!(run.error_code(), "unknown_method", "{run:?}");
    for (method, params) in [
        ("debug.clock_advance", r#"{"seconds":60}"#),
        ("debug.clock_set", r#"{"at":"2030-01-01T00:00:00Z"}"#),
    ] {
        let run = release.rlxd(&["call", method, params]);
        assert_eq!(run.error_code(), "unknown_method", "{method}: {run:?}");
    }

    let test_build = TestHome::new();
    test_build.start();
    let run = test_build.rlxd(&["call", "debug.emit", r#"{"count":1}"#]);
    assert_eq!(run.code(), Some(0), "the test build has it: {run:?}");
}

#[test]
fn a_release_builds_scheduler_ignores_a_test_clock() {
    let release = TestHome::new()
        .with_binary(release_rlxd())
        .with_env("RLXD_TEST_CLOCK", "2030-01-01T00:00:00Z");
    release.start();
    let job = r#"{"job_id":"t1","type":"run","schedule":{"every_minutes":1},
        "run":{"kind":"task","prompt":"hi","mount":{"kind":"no_tools"}},"catch_up":"skip"}"#;
    let put = release.rlxd(&["call", "job.put", job]);
    assert_eq!(put.code(), Some(0), "{put:?}");
    let list = release.rlxd(&["call", "job.list", "{}"]);
    let next = list.data()["jobs"][0]["next_fire_at"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!next.starts_with("2030"), "the system's clock: {next}");
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
