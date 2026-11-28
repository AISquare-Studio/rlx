//! The macOS gate peer check against the compiled-in requirement (handoff 0.10.3 rev 6,
//! 5.1 Accept, "macOS, signed" and "The shipped binary"; task acceptance 3 (iv) and (v)):
//!
//! ```text
//! cargo test --release -p rlxd --test gate_peer_macos_release
//! ```
//!
//! Built without the `gate-test-requirement` feature, so nothing can replace the
//! requirement: not `RLXD_TEST_GATE_REQUIREMENT`, and not `gate.requirement` in rlxd's
//! config. A build with the feature skips this file.
//!
//! The rlxd under test is a release build ([`release_rlxd`]), which has no gate dev mode
//! (handoff rev 7): the test build every `cargo test` makes has it, through rlxd's
//! dev-dependency on itself. So no credential here is minted in dev mode; each is planted
//! in gate.json, as any process of the user's could.
#![cfg(all(target_os = "macos", not(feature = "gate-test-requirement")))]

mod common;
mod macos_signing;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use common::{TestHome, release_rlxd};
use macos_signing::{SelfSigned, Signing, TEST_IDENTIFIER, helper, test_requirement, try_helper};
use rlxd::peer::{MACOS_REQUIREMENT, MACOS_TEAM_ID, static_code_satisfies};

/// The release build, built once. Every test asks for it first: under `cargo test
/// --release` the build replaces `CARGO_BIN_EXE_rlxd`, which the helpers are copies of.
fn release() -> PathBuf {
    static RELEASE: OnceLock<PathBuf> = OnceLock::new();
    RELEASE.get_or_init(release_rlxd).clone()
}

/// A fresh home that the release build serves.
fn release_home() -> TestHome {
    TestHome::new().with_binary(release())
}

/// A credential paired in gate.json, so a hello carrying it is refused only by the peer
/// check.
fn planted_credential(home: &TestHome) -> String {
    let credential = "a".repeat(64);
    home.plant("local", &credential);
    credential
}

/// Asserts `helper` can neither say `hello {role: "app"}` with a valid credential, nor,
/// once the user unpaired it, make the gate's first pairing.
fn refused(home: &TestHome, helper: &Path, credential: &str, why: &str) {
    let run = home.run_of(
        helper,
        &[
            "call",
            "--role",
            "app",
            "--gate-credential",
            credential,
            "status",
        ],
    );
    assert_eq!(run.code(), Some(1), "{why}: {run:?}");
    assert_eq!(run.error_code(), "not_gate", "{why}");
    assert!(
        home.log()
            .contains("does not satisfy the gate's requirement"),
        "{why}: the peer check refused it, not the credential:\n{}",
        home.log()
    );
    let run = home.rlxd(&["gate", "unpair"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let run = home.run_of(helper, &["call", "gate.pair"]);
    assert_eq!(run.code(), Some(1), "{why}: {run:?}");
    assert_eq!(run.error_code(), "not_gate", "{why}");
    assert!(!home.gate_json().exists(), "{why}: nothing was paired");
}

#[test]
fn a_self_signed_helper_with_the_identifier_and_team_is_refused_for_want_of_apples_anchor() {
    let home = release_home();
    let dir = home.root().join("helpers");
    let list = macos_signing::search_list();
    let identity = match SelfSigned::new(&dir.join("keychain"), MACOS_TEAM_ID) {
        Ok(identity) => identity,
        Err(why) => return macos_signing::skip_or_fail(&why),
    };
    let signed = try_helper(
        &dir,
        "self-signed",
        &Signing {
            identifier: "studio.aisquare.rlx",
            runtime: true,
            entitlements: &[],
            identity: Some(&identity),
        },
    );
    let self_signed = match signed {
        Ok(path) => path,
        Err(why) => {
            return macos_signing::skip_or_fail(&format!(
                "codesign can't sign from a keychain off the search list here: {why}"
            ));
        }
    };
    // The control: it satisfies the very same requirement without the anchor, so the
    // anchor alone refuses it.
    let without_anchor = MACOS_REQUIREMENT
        .strip_prefix("anchor apple generic and ")
        .expect("the requirement starts with the anchor");
    static_code_satisfies(&self_signed, without_anchor)
        .unwrap_or_else(|e| panic!("the control failed: {e}"));
    assert!(static_code_satisfies(&self_signed, MACOS_REQUIREMENT).is_err());

    // Refused on the gate's first pairing and on a hello with a valid credential.
    let credential = planted_credential(&home);
    home.start();
    refused(&home, &self_signed, &credential, "self-signed");

    // Rev 7: gate.dev_mode in the config changes nothing in a release build.
    home.stop();
    std::fs::write(
        home.root().join("rlxd").join("config.toml"),
        "[gate]\ndev_mode = true\n",
    )
    .unwrap();
    home.start();
    assert_eq!(home.rlxd(&["gate", "unpair"]).code(), Some(0));
    let run = home.run_of(&self_signed, &["call", "gate.pair"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "not_gate");
    assert!(home.log().contains("ignoring gate.dev_mode"));
    drop(identity);
    assert_eq!(
        macos_signing::search_list(),
        list,
        "the keychain search list is never written"
    );
}

#[test]
fn neither_the_test_variable_nor_the_config_changes_the_requirement() {
    let home = release_home().with_env("RLXD_TEST_GATE_REQUIREMENT", &test_requirement());
    let test_identity = helper(
        &home.root().join("helpers"),
        "test-identity",
        &Signing {
            identifier: TEST_IDENTIFIER,
            runtime: true,
            entitlements: &[],
            identity: None,
        },
    );
    let credential = planted_credential(&home);
    std::fs::write(
        home.root().join("rlxd").join("config.toml"),
        format!("[gate]\nrequirement = '{}'\n", test_requirement()),
    )
    .unwrap();
    home.start();
    refused(&home, &test_identity, &credential, "the test identity");
    assert!(home.log().contains("ignoring gate.requirement"));
}

#[test]
fn the_shipped_binary_holds_the_requirement_and_no_test_override() {
    let binary = std::fs::read(release()).unwrap();
    let holds = |needle: &[u8]| {
        binary
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count()
    };
    assert!(holds(br#"anchor apple generic and identifier "studio.aisquare.rlx""#) >= 1);
    assert_eq!(holds(b"RLXD_TEST_GATE_REQUIREMENT"), 0);
    assert_eq!(holds(b"gate-dev-mode"), 0);
}
