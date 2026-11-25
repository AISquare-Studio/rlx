//! One version number: rlxd's is rlx's (handoff 0.10.3, 1.2).
//!
//! The workspace's `version` must equal `version` in the repo's pyproject.toml, every
//! crate must take the workspace's, and rlxd/Cargo.lock must record it for each of them.
//! A release bumps three files together: pyproject.toml, rlxd/Cargo.toml, and
//! rlxd/Cargo.lock (regenerate it with `cargo update --workspace` from rlxd/).

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

/// Every crate of the workspace: each directory under rlxd/crates that holds a
/// Cargo.toml, with its manifest. A stray file there, such as a `.DS_Store`, is skipped.
fn crate_manifests() -> Vec<(PathBuf, toml::Table)> {
    let crates = repo().join("rlxd").join("crates");
    let mut manifests = Vec::new();
    for entry in fs::read_dir(&crates).unwrap() {
        let manifest = entry.unwrap().path().join("Cargo.toml");
        if manifest.is_file() {
            let table = read_toml(&manifest);
            manifests.push((manifest, table));
        }
    }
    assert!(
        manifests.len() >= 4,
        "found {} crates under {}",
        manifests.len(),
        crates.display()
    );
    manifests
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
        "rlxd/Cargo.toml's version differs from pyproject.toml's: bump pyproject.toml, \
         rlxd/Cargo.toml and rlxd/Cargo.lock together"
    );
}

#[test]
fn the_lock_records_rlxs_version_for_every_crate() {
    // `cargo test --locked` fails on a stale lock too, but only after a build; this
    // names the fix.
    let lock = read_toml(&repo().join("rlxd").join("Cargo.lock"));
    let packages = lock["package"]
        .as_array()
        .expect("Cargo.lock lists packages");
    for (manifest, table) in crate_manifests() {
        let name = table["package"]["name"]
            .as_str()
            .unwrap_or_else(|| panic!("{}: no [package] name", manifest.display()));
        let locked = packages
            .iter()
            .find(|p| p["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("Cargo.lock has no {name}"));
        assert_eq!(
            locked["version"].as_str(),
            Some(rlx_version().as_str()),
            "rlxd/Cargo.lock records {name} at another version: run `cargo update \
             --workspace` in rlxd/"
        );
    }
}

#[test]
fn this_binary_carries_rlxs_version() {
    assert_eq!(env!("CARGO_PKG_VERSION"), rlx_version());
}

#[test]
fn every_crate_takes_the_workspace_version() {
    for (manifest, table) in crate_manifests() {
        let package = &table["package"];
        assert_eq!(
            package.get("version").and_then(|v| v.get("workspace")),
            Some(&toml::Value::Boolean(true)),
            "{}: set `version.workspace = true`",
            manifest.display()
        );
    }
}
