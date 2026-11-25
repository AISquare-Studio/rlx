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

`.github/workflows/rlxd.yml` runs all of these on ubuntu, macOS and Windows for any PR
that touches `rlxd/`, and for every push to `rc/**`.

## One version number

rlxd's version is rlx's. `[workspace.package] version` in `Cargo.toml` must equal
`version` in the repo's `pyproject.toml`, and every crate takes the workspace's.
`crates/rlxd/tests/version.rs` fails when they differ, so bump both together.

## Licence

MIT, as rlx is (`../LICENSE`).
