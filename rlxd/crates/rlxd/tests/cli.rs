//! What `rlxd` answers before it touches a home (handoff 0.10.3, 1.1 and 7.2): its
//! version, its help, and exit 2 on a usage error.

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rlxd"))
        .args(args)
        .env(
            "AISQUARE_HOME",
            std::env::temp_dir().join("rlxd-cli-test-no-home"),
        )
        .env_remove("RLX_HOME")
        .output()
        .expect("rlxd runs")
}

#[test]
fn version_prints_the_name_and_rlxs_version() {
    // The test build has the gate-dev-mode feature, and says so. A release build says
    // `rlxd <version>` alone: tests/release_build.rs checks that.
    for flag in ["--version", "-V"] {
        let output = run(&[flag]);
        assert!(output.status.success(), "{flag}: {output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("rlxd {} (dev: gate-dev-mode)\n", env!("CARGO_PKG_VERSION"))
        );
    }
}

#[test]
fn help_names_every_command() {
    let output = run(&["--help"]);
    assert!(output.status.success(), "{output:?}");
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("Usage: rlxd"), "{help}");
    for command in ["serve", "status", "stop", "call", "here"] {
        assert!(help.contains(&format!("  {command} ")), "{command}: {help}");
    }
}

#[test]
fn a_usage_error_exits_2_and_prints_nothing_on_stdout() {
    for args in [
        &[][..],
        &["frobnicate"],
        &["serve", "--bogus"],
        &["call"],
        &["call", "--raw", "status"],
        &["call", "--as-app", "--role", "tui", "status"],
        &["call", "status", "not json"],
        &["call", "status", "[1]"],
        &["call", "--env", "{", "status"],
        &["call", "--protocol-version", "one", "status"],
        &["status", "--bogus"],
        &["here", "--protocols", "0"],
        &["here", "--protocols", "1-"],
        &["here", "--bogus"],
    ] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
    }
}
