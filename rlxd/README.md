# rlxd

rlx's engine: one long-running local daemon per home. It hosts processes (PTYs and
headless Claude Code runs), receives hooks, runs the user's scheduler, and pushes
events to its clients: the TUI, rlxapp and Remote. Python stays the orchestration
brain and the TUI. The plan is `docs/plans/0.10.3-handoff.md`, section 1.

This is the daemon's core (1.3, 7.2, 7.4): one rlxd per home, on a local socket or a
named pipe, a token `hello`, the envelope on every reply, and `status`, plus the app
gate's authentication (5.1): per-device gate credentials and the peer identity check.
The event bus, the PTY host, the runner and the rest arrive with later items, each a
method on this socket. `rlxd-hook` answers `--version` only for now.

```
rlxd/
  crates/rlxd-proto/   wire types, schema_version, JSON Schemas (schemars); no I/O
  crates/rlxd-client/  the library rlxapp depends on: the home's paths, connect, hello, call
  crates/rlxd/         the daemon: serve, status, stop, call
  crates/rlxd-hook/    the tiny hook client (`--version` only for now)
  schemas/v1/          generated from rlxd-proto and checked in; CI fails on drift
  tests/fixtures/      fake-claude scenarios
```

## The daemon

```sh
rlxd serve [--detach] [--gate-dev-mode]  # run rlxd for this home; --detach returns once it takes connections
rlxd status [--json]    # what it is, who is connected, the gate's mode, and Claude Code's sign-in
rlxd stop               # stop it, and wait until it has
rlxd call [--as-app] [--role R] [--client NAME] [--gate-credential C] [--token T]
          [--protocol-version N] [--env JSON] [--raw] <method> [<params-json>]
rlxd gate devices       # the devices paired as the app gate: route, created, last seen
rlxd gate pair [--request-id ID [--deny | --force]]  # list waiting pairings, or answer one
rlxd gate unpair [<device>]              # unpair one device, or every device
```

- **The home** is `RLX_HOME`, else `AISQUARE_HOME`, else `~/.aisquare`, as rlx resolves
  it. `rlxd-client`'s `Home` derives every path below from it, in one place.
- **One per home.** `<home>/run/rlxd.lock` is held while rlxd runs, with the lock
  primitive Python's `core.locking` takes: `flock` on POSIX, `LockFileEx` on byte 0 on
  Windows. A second `rlxd serve` exits 1 with `already_running`.
- **Where clients connect.** On macOS and Linux, `<home>/run/rlxd.sock` (0600, in a 0700
  folder). Its path must fit the OS's socket path limit, about 100 bytes, so a deeply
  nested home is refused with a message naming the path. On Windows, the named pipe
  `\\.\pipe\rlxd-<hash of home>`, whose DACL holds one entry, for the user rlxd runs as.
  A client checks that the pipe's server runs as its own user before it sends the token.
- **The token,** `<home>/rlxd/token` (0600), is made once. `hello` must carry it.
- **The registry.** While it runs, rlxd has a `kind: "rlxd"` record in `<home>/daemons/`
  (4.1), in the format of Python's `core.registry` and `core.daemons`: held by its
  `<id>.lock`, rewritten every 5 s, removed on stop. After a kill its lock is free, so it
  reads as dead.
- **The gate** (5.1). Only rlx's app may hold the gate role, proven two ways:
  - **A per-device gate credential**, separate from the token. `gate.pair {device?,
    gate_credential?}` mints a named device's credential and returns it once; rlxd keeps
    only its SHA-256 in `<home>/rlxd/gate.json` (0600), with the device's route
    (`local`), when it was made and when it was last seen. The first pairing is trust
    on first use. Re-pairing a device needs its current credential, and drops every
    connection that used the old one. A new device, once one is paired, waits up to
    120 s for an approval: a paired app answers `gate.pair_answer` (it lists them with
    `gate.pair_requests`), or the user does on the host. `rlxd gate pair` lists each
    waiting request with its id, device name, route and the process that asked (pid and
    binary), and approves only the one `--request-id` names. A request whose client
    leaves is withdrawn, and a credential whose reply can't be delivered is revoked, so
    no device is left paired with a credential nobody received. Device names are unique;
    a new device may take the name of one the user unpaired only with `--force`.
    `rlxd gate unpair [<device>]` is the only reset; unpairing one device never touches
    another. A `hello {role: "app"}` without a paired device's credential (an empty one
    included) gets `not_gate`, and so does anything else the gate refuses.
  - **The peer's identity**, checked for a device paired locally, on every gate hello
    and every `gate.pair`. macOS: the `LOCAL_PEERTOKEN` audit token, checked by
    Security.framework against a requirement compiled into rlxd (Apple's anchor, the
    identifier `studio.aisquare.rlx` and the team), plus the hardened runtime and no
    injection entitlements. Linux: `SO_PEERCRED`'s uid, the process pinned with
    `SO_PEERPIDFD`, and, where rlx's app is installed from a package, its binary.
    Windows: the pipe client's process, opened at once and held, its creation time,
    its image path, and the Authenticode signature (revocation checked) of the file that
    image is mapped from, whose signer's full subject must be rlx's publisher. **On
    Linux and Windows this is
    best-effort against code running as the same user; there the credential is the
    real boundary.** `crates/rlxd/src/peer.rs` says why, and holds the expected
    identity's constants.
  - **SSH.** Over SSH, a connection's peer is sshd, not the app, so a device paired
    inside an authenticated SSH session will be admitted by its credential alone: the
    SSH pairing plus the per-device credential is the boundary. SSH pairing is a later
    item (hub q1). Until it lands, rlxd mints no ssh-route credential, and refuses and
    logs any ssh-route device it finds in gate.json. A connection that only reaches the
    socket never pairs that route.
  - **Gate dev mode** (`--gate-dev-mode`, or `gate.dev_mode = true` in
    `<home>/rlxd/config.toml`) is for unsigned dev and CI builds of the app, and never
    the default. It skips only the peer identity check; the credential and every
    pairing rule still hold. rlxd logs it at start and on each gate hello, and `status`
    says `gate: "dev mode (unsigned app accepted)"` instead of `"signed app only"`. No
    config key, variable or flag changes the identity a release build expects.
- **Logs** are `<home>/rlxd/logs/rlxd.log`, rotated at 5 MiB, three kept.
- **Exit codes:** 0 when the envelope printed is `ok`, 1 when it is not, 2 on a usage
  error, 3 when rlxd is not running for the home.

The `status` reply's shape is `schemas/v1/status-reply.json`. `RLXD_CLAUDE_BIN` names the
program rlxd runs as Claude Code; tests point it at `tests/fixtures/fake-claude/`.

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

The gate's peer checks sign or build helpers with a test identity, which only the
`gate-test-requirement` feature lets rlxd accept, so they are their own runs. Each test
compiles to nothing on another OS. The Linux other-uid check and the Windows tests need
`sudo -n` or an administrator: they skip with a note elsewhere, and
`RLXD_GATE_PEER_REQUIRED=1` (CI) makes them fail instead.

```sh
cargo test -p rlxd --features gate-test-requirement --test gate_peer_macos \
  --test gate_peer_linux --test gate_peer_windows
cargo test --release -p rlxd --test gate_peer_macos_release   # the compiled-in requirement
```

Two tests cross into Python's side of the daemon registry (4.1):
`crates/rlxd/tests/registry_interop.rs` (Python's `core.daemons` holds a record that
rlxd must read as live) and `../tests/test_daemon_registry.py` (Python reads the real
rlxd). They skip unless `RLXD_INTEROP_REQUIRED` or `RLXD_BIN` is set. With either set
they need `uv`, and the pytest needs `RLXD_BIN` to name a built rlxd:

```sh
RLXD_INTEROP_REQUIRED=1 cargo test --workspace --locked
cd .. && RLXD_BIN=rlxd/target/release/rlxd uv run --extra dev pytest tests/test_daemon_registry.py -q
```

`.github/workflows/rlxd.yml` runs all of these on ubuntu, macOS and Windows, with
`RLXD_INTEROP_REQUIRED=1`, so a missing uv or rlxd fails there. It runs for any PR
that touches `rlxd/`, `pyproject.toml`, the workflow, or the registry's Python side
(`src/aisquare/core/{daemons,registry,locking}.py`, `tests/test_daemon_registry.py`),
for every push to `main` or `rc/**`, and by hand.

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

## Licence

MIT, as rlx is (`../LICENSE`).
