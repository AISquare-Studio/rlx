//! What a release build leaves out. This builds rlxd as a release does (`cargo build
//! --release`, [`release_rlxd`]), without the features only rlxd's dev-dependency on
//! itself turns on, and checks against the test build the other tests run:
//!
//! - check 7 of task #22 (handoff 0.10.3, 7.3): no test-only methods, so `debug.emit` is
//!   `unknown_method`;
//! - handoff 0.10.3 rev 7, 5.1, "No dev mode in a release build": no `--gate-dev-mode`
//!   flag, `gate.dev_mode` in the config ignored, no marker in `rlxd --version` or in
//!   status, and not even the feature's name in the binary.

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

    let test_build = TestHome::new();
    test_build.start();
    let run = test_build.rlxd(&["call", "debug.emit", r#"{"count":1}"#]);
    assert_eq!(run.code(), Some(0), "the test build has it: {run:?}");
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
