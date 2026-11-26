# rlxd

rlx's engine: one long-running local daemon per home. It hosts processes (PTYs and
headless Claude Code runs), receives hooks, runs the user's scheduler, and pushes
events to its clients: the TUI, rlxapp and Remote. Python stays the orchestration
brain and the TUI. The plan is `docs/plans/0.10.3-handoff.md`, section 1.

This is the daemon's core (1.3, 7.2, 7.4) and its event bus (1.7, 7.3): one rlxd per
home, on a local socket or a named pipe, a token `hello`, the envelope on every reply,
`status`, and `events.subscribe`. The PTY host, the runner and the rest arrive with
later items, each a method on this socket that publishes on the bus. `rlxd-hook`
answers `--version` only for now.

```
rlxd/
  crates/rlxd-proto/   wire types, schema_version, JSON Schemas (schemars); no I/O
  crates/rlxd-client/  the library rlxapp depends on: the home's paths, connect, hello,
                       call, subscribe
  crates/rlxd/         the daemon: serve, status, stop, call, events, and the bus
  crates/rlxd-hook/    the tiny hook client (`--version` only for now)
  schemas/v1/          generated from rlxd-proto and checked in; CI fails on drift
  tests/fixtures/      fake-claude scenarios
```

## The daemon

```sh
rlxd serve [--detach]   # run rlxd for this home; --detach returns once it takes connections
rlxd status [--json]    # what it is, who is connected, and Claude Code's sign-in
rlxd stop               # stop it, and wait until it has
rlxd call [--as-app] [--role R] [--client NAME] [--gate-credential C] [--token T]
          [--protocol-version N] [--env JSON] [--raw] <method> [<params-json>]
rlxd events [--as-app] [--topics a,b] [--since SEQ] [--follow] [--until TYPE[,TYPE…]]
            [--limit N] [--timeout S]
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
- **The gate.** No connection may be the gate yet: every `hello {role: "app"}` gets
  `not_gate` until gate pairing lands (5.1).
- **Logs** are `<home>/rlxd/logs/rlxd.log`, rotated at 5 MiB, three kept.
- **Exit codes:** 0 when the envelope printed is `ok`, 1 when it is not, 2 on a usage
  error, 3 when rlxd is not running for the home. `rlxd events` exits 4 when its
  `--timeout` runs out.

## The event bus

One numbered stream of events per home (1.7, 7.3), the `crate::bus` module:

- **`seq`** numbers every event from 1, across topics and across restarts. rlxd keeps a
  mark at or above every seq it has given out in `<home>/rlxd/seq`: written 1,000 seqs
  ahead, and exactly on a clean stop. A client resuming from a seq the buffer no longer
  holds, a previous rlxd's included, first gets `daemon.gap {from, to}`, whose own seq
  is `to`. Run events also carry `run_seq`, per run, and the bus indexes them by run.
- **The buffer** holds the last 10,000 events, none older than 24 hours.
- **`events.subscribe {topics?, since?, follow?}`** replays what the buffer holds after
  `since`, then, while `follow` (the default), live events. Its reply `{seq}` comes last
  and ends the stream. A connection carries one stream, so event frames carry no `id`.
  An unknown topic gets `unknown_topic`.
- **A slow client holds up no one.** A subscriber is a place in the buffer, not a
  queue: it is written at its own pace, and when the buffer drops events it has not
  read, it gets a `daemon.gap` for them and goes on.
- **`rlxd events`** prints the stream as JSON lines, events only (`jq -s .` makes an
  `event-list.json`). It exits 0 at `--until` or `--limit`, or when the stream ends, and
  4 at `--timeout`. `--until` implies `--follow`. A refusal goes to stderr as its
  envelope.
- **The `daemon` topic:** `daemon.started {version, pid}`, `daemon.stopping {reason}`
  and `daemon.gap {from, to}`.
- **`debug.emit {count}`** publishes `count` events, for tests. It exists only with the
  cargo feature `debug-methods`, which only rlxd's dev-dependency on itself turns on:
  `cargo test` has it, `cargo build --release` never does, and
  `crates/rlxd/tests/release_build.rs` builds a release rlxd to check.

The `status` reply's shape is `schemas/v1/status-reply.json`. `RLXD_CLAUDE_BIN` names the
program rlxd runs as Claude Code; tests point it at `tests/fixtures/fake-claude/`.

## Building and checking

`rust-toolchain.toml` pins the toolchain. rustup reads it from the directory cargo
runs in, so run cargo from here:

```sh
cd rlxd
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked   # also builds a release rlxd (tests/release_build.rs)
cargo build --release
cargo run -p rlxd-proto --bin gen-schemas -- --check
cargo deny check            # licences and advisories (deny.toml)
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
