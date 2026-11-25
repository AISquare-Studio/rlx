//! One version number: rlxd's is rlx's (handoff 0.10.3, 1.2).
//!
//! The workspace's `version` must equal `version` in the repo's pyproject.toml, and every
//! crate must take the workspace's. A release bumps both files together.

use std::fs;
use std::path::{Path, PathBuf};

/// The repo's root: rlxd/crates/rlxd, three levels up.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("rlxd lives at <repo>/rlxd/crates/rlxd")
        .to_path_buf()
}

fn read_toml(path: &Path) -> toml::Table {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.parse()
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn rlx_version() -> String {
    let pyproject = read_toml(&repo().join("pyproject.toml"));
    pyproject["project"]["version"]
        .as_str()
        .expect("pyproject.toml has [project] version")
        .to_owned()
}

#[test]
fn the_workspace_version_is_rlxs_version() {
    let workspace = read_toml(&repo().join("rlxd").join("Cargo.toml"));
    let version = workspace["workspace"]["package"]["version"]
        .as_str()
        .expect("rlxd/Cargo.toml has [workspace.package] version");
    assert_eq!(
        version,
        rlx_version(),
        "rlxd/Cargo.toml's version differs from pyproject.toml's: bump both together"
    );
}

#[test]
fn this_binary_carries_rlxs_version() {
    assert_eq!(env!("CARGO_PKG_VERSION"), rlx_version());
}

#[test]
fn every_crate_takes_the_workspace_version() {
    let crates = repo().join("rlxd").join("crates");
    let mut seen = 0;
    for entry in fs::read_dir(&crates).unwrap() {
        let manifest = entry.unwrap().path().join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let package = &read_toml(&manifest)["package"];
        assert_eq!(
            package.get("version").and_then(|v| v.get("workspace")),
            Some(&toml::Value::Boolean(true)),
            "{}: set `version.workspace = true`",
            manifest.display()
        );
        seen += 1;
    }
    assert!(seen >= 4, "found {seen} crates under {}", crates.display());
}
