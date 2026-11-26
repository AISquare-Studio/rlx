# rlxd

rlx's engine: one long-running local daemon per home. It hosts processes (PTYs and
headless Claude Code runs), receives hooks, runs the user's scheduler, and pushes
events to its clients: the TUI, rlxapp and Remote. Python stays the orchestration
brain and the TUI. The plan is `docs/plans/0.10.3-handoff.md`, section 1.

This is the workspace's foundation (1.1). Both binaries answer `--version` only for
now. The daemon's behaviour arrives with 1.3 and later items.

```
rlxd/
  crates/rlxd-proto/   wire types, schema_version, JSON Schemas (schemars); no I/O
  crates/rlxd-client/  the library rlxapp depends on: connect, call, subscribe (a stub for now)
  crates/rlxd/         the daemon (`--version` only for now)
  crates/rlxd-hook/    the tiny hook client (`--version` only for now)
  schemas/v1/          generated from rlxd-proto and checked in; CI fails on drift
  tests/fixtures/      fake-claude scenarios
```

## Building and checking

`rust-toolchain.toml` pins the toolchain. rustup reads it from the directory cargo
runs in, so run cargo from here:

```sh
cd rlxd
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo build --release
cargo run -p rlxd-proto --bin gen-schemas -- --check
cargo deny check            # licences and advisories (deny.toml)
```

`.github/workflows/rlxd.yml` runs all of these on ubuntu, macOS and Windows. It runs for any PR
that touches `rlxd/`, `pyproject.toml` or the workflow, for every push to `main` or
`rc/**`, and by hand.

## One version number

rlxd's version is rlx's. `[workspace.package] version` in `Cargo.toml` must equal
`version` in the repo's `pyproject.toml`, and every crate takes the workspace's.
`crates/rlxd/tests/version.rs` fails when they differ. A release bumps three files
together:

1. `pyproject.toml`'s `version`;
2. `rlxd/Cargo.toml`'s `[workspace.package] version`;
3. `rlxd/Cargo.lock`, by running `cargo update --workspace` in `rlxd/`. Every CI step
   runs with `--locked`, so a stale lock fails.

`.github/workflows/rlxd.yml` also runs for a PR that changes only `pyproject.toml`, so
a bump on one side alone fails before it merges.

## Release archives

rlxd ships with rlx releases (handoff 0.10.3, 1.2). `.github/workflows/rlxd-release.yml`
builds `rlxd` and `rlxd-hook` for five targets and packs each flat
(`../scripts/rlxd_archive.py`): `rlxd-<target>.tar.gz` for aarch64 and x86_64 macOS
and Linux (glibc), `rlxd-x86_64-pc-windows-msvc.zip` with the `.exe`s, and
`rlxd-SHA256SUMS` over them, which `shasum -a 256 -c` reads.

- **Every head.** rlxd.yml calls it, so each run uploads the artifact `rlxd-archives`,
  and installs it with `rlx daemon install` on ubuntu, macOS and Windows.
- **A release.** Publishing GitHub Release `vX.Y.Z` attaches the five archives and the
  sums to it. The tag must be `v` + `pyproject.toml`'s version.
- **By hand.** `gh workflow run rlxd.yml --ref <branch> -f release_tag=vX.Y.Z` does a
  dry run of that attach: every check, then the `gh release upload` it would run, printed.
  Add `-f dry_run=false` to attach for real, to re-attach after a failed release run.
- **Signing.** None yet: notarization and Authenticode are an owner step, wired in at
  the marked step of the build job.

`rlx daemon install` (`../src/aisquare/services/rlxd_install.py`) fetches
`rlxd-SHA256SUMS` and this machine's archive for `rlx --version` from
`https://github.com/AISquare-Studio/rlx/releases/download/v<version>/`, or from
`$AISQUARE_RELEASES_URL/v<version>/` when that is set. It checks the sha256 before it
writes anything, then installs both binaries into `uv tool dir --bin`, beside `rlx`.

## Licence

MIT, as rlx is (`../LICENSE`).
