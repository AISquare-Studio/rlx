//! Signed helpers for the macOS gate peer tests: copies of the `rlxd` binary, whose
//! `rlxd call` is then a client with a code signature of the test's choosing.
//!
//! The tests sign with Apple's `codesign` tool. rlxd itself never runs it: the tests
//! assert that too.
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

/// A copy of `rlxd` at `dir/name`, signed as `signing` says.
pub fn helper(dir: &Path, name: &str, signing: &Signing<'_>) -> PathBuf {
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
    let mut out = command.output().unwrap();
    if let Some(identity) = signing.identity
        && !out.status.success()
        && String::from_utf8_lossy(&out.stderr).contains("no identity found")
    {
        // Some macOS releases search only the keychains on the user's search list, even
        // with --keychain: list it for this one signing, then put the list back.
        let _listed = identity.listed();
        out = command.output().unwrap();
    }
    assert!(
        out.status.success(),
        "codesign {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    path
}

/// A throwaway keychain holding a self-signed code-signing identity whose certificate's
/// OU is `ou`. Dropping it deletes the keychain. The user's keychain search list is not
/// touched: signing names this keychain.
pub struct SelfSigned {
    pub keychain: PathBuf,
    /// The certificate's SHA-1 fingerprint, which names the identity to `codesign`.
    pub sha1: String,
}

/// The user's keychain search list with a throwaway keychain added, until dropped.
pub struct Listed {
    before: Vec<String>,
}

impl SelfSigned {
    pub fn new(dir: &Path, ou: &str) -> Self {
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
        let keychain = dir.join("gate-test.keychain-db");
        run("security", |c| {
            c.args(["create-keychain", "-p", "kpw"]).arg(&keychain)
        });
        let made = Self {
            keychain: keychain.clone(),
            sha1,
        };
        run("security", |c| {
            c.args(["unlock-keychain", "-p", "kpw"]).arg(&keychain)
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
                "kpw",
            ])
            .arg(&keychain)
        });
        made
    }
}

impl SelfSigned {
    /// Adds this keychain to the user's search list until the guard drops.
    pub fn listed(&self) -> Listed {
        let before: Vec<String> = output("security", |c| c.args(["list-keychains", "-d", "user"]))
            .lines()
            .map(|line| line.trim().trim_matches('"').to_owned())
            .filter(|line| !line.is_empty())
            .collect();
        run("security", |c| {
            c.args(["list-keychains", "-d", "user", "-s"])
                .arg(&self.keychain)
                .args(&before)
        });
        Listed { before }
    }
}

impl Drop for Listed {
    fn drop(&mut self) {
        let _ = Command::new("security")
            .args(["list-keychains", "-d", "user", "-s"])
            .args(&self.before)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for SelfSigned {
    fn drop(&mut self) {
        let _ = Command::new("security")
            .arg("delete-keychain")
            .arg(&self.keychain)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
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
