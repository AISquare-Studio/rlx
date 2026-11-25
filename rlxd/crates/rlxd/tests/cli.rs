//! What `rlxd` answers before it does anything else (handoff 0.10.3, 1.1).

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rlxd"))
        .args(args)
        .output()
        .expect("rlxd runs")
}

#[test]
fn version_prints_the_name_and_rlxs_version() {
    for flag in ["--version", "-V"] {
        let output = run(&[flag]);
        assert!(output.status.success(), "{flag}: {output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("rlxd {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
}

#[test]
fn help_prints_the_usage() {
    let output = run(&["--help"]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .starts_with("usage: rlxd")
    );
}

#[test]
fn anything_else_is_a_usage_error() {
    for args in [&[][..], &["serve"], &["--version", "--help"]] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .starts_with("usage: rlxd")
        );
    }
}
