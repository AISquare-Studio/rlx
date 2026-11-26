//! Signed helpers for the macOS gate peer tests: copies of the `rlxd` binary, whose
//! `rlxd call` is then a client with a code signature of the test's choosing.
//!
//! The tests sign with Apple's `codesign` tool. rlxd itself never runs it: the tests
//! assert that too.
//!
//! **The user's keychain search list is never written.** A self-signed identity lives
//! in a keychain file that each signing names with `--keychain`:
//!
//! - **`RLXD_GATE_TEST_KEYCHAIN`**, when set, with its password in
//!   `RLXD_GATE_TEST_KEYCHAIN_PASSWORD`: a keychain the caller made and listed. CI does,
//!   on its throwaway runner (rlxd.yml), because macOS 15's `codesign` finds identities
//!   only in listed keychains. The test imports its identity there and removes it after.
//! - **Otherwise** a throwaway keychain of the test's own, which it never lists. Some
//!   macOS releases (27, for one) sign from it; where `codesign` can't, or `security
//!   create-keychain` would list it, the test is skipped (and fails where
//!   `RLXD_GATE_PEER_REQUIRED` is set, as in CI).
//!
//! Every keychain step reads the list before and after, and fails if it changed. Ad-hoc
//! signing needs no keychain.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The identifier the test identity's requirement names.
pub const TEST_IDENTIFIER: &str = "studio.aisquare.rlx.gatetest";

/// The test identity's requirement: an ad-hoc signature carries an identifier, and
/// nothing else to check.
pub fn test_requirement() -> String {
    format!("identifier \"{TEST_IDENTIFIER}\"")
}

/// The injection entitlements rlxd refuses (5.1).
pub const INJECTION_ENTITLEMENTS: [&str; 3] = [
    "com.apple.security.get-task-allow",
    "com.apple.security.cs.disable-library-validation",
    "com.apple.security.cs.allow-dyld-environment-variables",
];

/// How to sign one helper.
pub struct Signing<'a> {
    /// The identifier.
    pub identifier: &'a str,
    /// With the hardened runtime (`--options runtime`).
    pub runtime: bool,
    /// Entitlements granted, each `true`.
    pub entitlements: &'a [&'a str],
    /// A self-signed identity to sign with, else ad hoc.
    pub identity: Option<&'a SelfSigned>,
}

/// A copy of `rlxd` at `dir/name`, signed as `signing` says, which must succeed.
pub fn helper(dir: &Path, name: &str, signing: &Signing<'_>) -> PathBuf {
    try_helper(dir, name, signing).unwrap_or_else(|why| panic!("{why}"))
}

/// [`helper`], or why `codesign` could not sign it.
pub fn try_helper(dir: &Path, name: &str, signing: &Signing<'_>) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::copy(env!("CARGO_BIN_EXE_rlxd"), &path).unwrap();
    let mut command = Command::new("codesign");
    command.args(["--force", "--identifier", signing.identifier]);
    match signing.identity {
        Some(identity) => {
            command
                .arg("--sign")
                .arg(&identity.sha1)
                .arg("--keychain")
                .arg(&identity.keychain);
        }
        None => {
            command.args(["--sign", "-"]);
        }
    }
    if signing.runtime {
        command.args(["--options", "runtime"]);
    }
    if !signing.entitlements.is_empty() {
        let plist = dir.join(format!("{name}.entitlements.plist"));
        let keys: String = signing
            .entitlements
            .iter()
            .map(|e| format!("<key>{e}</key><true/>"))
            .collect();
        std::fs::write(
            &plist,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \
                 \"-//Apple//DTD PLIST 1.0//EN\" \
                 \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist \
                 version=\"1.0\"><dict>{keys}</dict></plist>\n"
            ),
        )
        .unwrap();
        command.arg("--entitlements").arg(&plist);
    }
    command.arg(&path).stdin(Stdio::null());
    let before = search_list();
    let out = command.output().unwrap();
    assert_eq!(
        search_list(),
        before,
        "codesign changed the keychain search list"
    );
    if out.status.success() {
        Ok(path)
    } else {
        Err(format!(
            "codesign {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// The user's keychain search list, as `security` prints it: read only.
pub fn search_list() -> String {
    output("security", |c| c.args(["list-keychains", "-d", "user"]))
}

/// Skips the calling test with `why`, or fails it where the gate peer tests are
/// required (CI).
pub fn skip_or_fail(why: &str) {
    assert!(
        std::env::var_os("RLXD_GATE_PEER_REQUIRED").is_none(),
        "required here: {why}"
    );
    eprintln!("skipped: {why}");
}

/// A self-signed code-signing identity whose certificate's OU is `ou`, in a keychain
/// file that signing names (see the module's doc). Dropping it deletes a throwaway
/// keychain, or removes the identity from the given one.
pub struct SelfSigned {
    pub keychain: PathBuf,
    /// The certificate's SHA-1 fingerprint, which names the identity to `codesign`.
    pub sha1: String,
    /// Whether the keychain is the test's own, to delete.
    own: bool,
}

impl SelfSigned {
    /// The identity, or why this macOS can't have one without the search list.
    pub fn new(dir: &Path, ou: &str) -> Result<Self, String> {
        const NAME: &str = "rlxd gate test self-signed";
        std::fs::create_dir_all(dir).unwrap();
        let config = dir.join("cert.cnf");
        std::fs::write(
            &config,
            format!(
                "[req]\ndistinguished_name=dn\nx509_extensions=ext\nprompt=no\n[dn]\nCN={NAME}\n\
                 OU={ou}\nO=rlxd gate test\n[ext]\nbasicConstraints=critical,CA:false\n\
                 keyUsage=critical,digitalSignature\nextendedKeyUsage=critical,codeSigning\n"
            ),
        )
        .unwrap();
        let (key, cert, p12) = (dir.join("k.pem"), dir.join("c.pem"), dir.join("id.p12"));
        // macOS's own LibreSSL: its PKCS#12 is one `security import` reads.
        run("/usr/bin/openssl", |c| {
            c.args([
                "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2",
            ])
            .arg("-keyout")
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .arg("-config")
            .arg(&config)
        });
        run("/usr/bin/openssl", |c| {
            c.args(["pkcs12", "-export", "-passout", "pass:pw"])
                .arg("-inkey")
                .arg(&key)
                .arg("-in")
                .arg(&cert)
                .arg("-out")
                .arg(&p12)
        });
        let fingerprint = output("/usr/bin/openssl", |c| {
            c.args(["x509", "-noout", "-fingerprint", "-sha1", "-in"])
                .arg(&cert)
        });
        let sha1: String = fingerprint
            .split_once('=')
            .map(|(_, hex)| hex.trim().replace(':', ""))
            .unwrap_or_default();
        assert_eq!(sha1.len(), 40, "{fingerprint}");
        let before = search_list();
        let given = std::env::var_os("RLXD_GATE_TEST_KEYCHAIN").map(PathBuf::from);
        let (keychain, password) = match given {
            Some(keychain) => {
                let password = std::env::var("RLXD_GATE_TEST_KEYCHAIN_PASSWORD")
                    .map_err(|_| "RLXD_GATE_TEST_KEYCHAIN_PASSWORD is unset".to_owned())?;
                (keychain, password)
            }
            None => {
                let keychain = dir.join("gate-test.keychain-db");
                run("security", |c| {
                    c.args(["create-keychain", "-p", "kpw"]).arg(&keychain)
                });
                (keychain, "kpw".to_owned())
            }
        };
        let own = std::env::var_os("RLXD_GATE_TEST_KEYCHAIN").is_none();
        let made = Self {
            keychain: keychain.clone(),
            sha1,
            own,
        };
        if own && search_list() != before {
            // This macOS lists a new keychain: deleting it unlists it again.
            drop(made);
            assert_eq!(
                search_list(),
                before,
                "the keychain search list stayed changed"
            );
            return Err(
                "`security create-keychain` puts a new keychain on the user's \
                        search list here"
                    .to_owned(),
            );
        }
        run("security", |c| {
            c.args(["unlock-keychain", "-p", &password]).arg(&keychain)
        });
        run("security", |c| {
            c.arg("import").arg(&p12).arg("-k").arg(&keychain).args([
                "-P",
                "pw",
                "-T",
                "/usr/bin/codesign",
            ])
        });
        run("security", |c| {
            c.args([
                "set-key-partition-list",
                "-S",
                "apple-tool:,apple:,codesign:",
                "-s",
                "-k",
                &password,
            ])
            .arg(&keychain)
        });
        assert_eq!(search_list(), before, "the keychain search list changed");
        Ok(made)
    }
}

impl Drop for SelfSigned {
    fn drop(&mut self) {
        let mut command = Command::new("security");
        if self.own {
            command.arg("delete-keychain").arg(&self.keychain);
        } else {
            command
                .args(["delete-identity", "-Z", &self.sha1])
                .arg(&self.keychain);
        }
        let _ = command.stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
}

fn run(program: &str, args: impl FnOnce(&mut Command) -> &mut Command) {
    output(program, args);
}

/// Runs `program`, which must succeed: its stdout.
fn output(program: &str, args: impl FnOnce(&mut Command) -> &mut Command) -> String {
    let mut command = Command::new(program);
    args(&mut command);
    let out = command.stdin(Stdio::null()).output().unwrap();
    assert!(
        out.status.success(),
        "{program}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}
