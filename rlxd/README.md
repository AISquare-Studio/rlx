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
rlxd here [--json] [--start] [--show-token] [--protocols VERSIONS]
                        # what is installed and running here: rlx.app's probe over SSH
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
  error, 3 when rlxd is not running for the home. `rlxd here` also exits 3, and 4, with
  an `ok` envelope: see below. `rlxd events` exits 4 when its `--timeout` runs out.

The `status` reply's shape is `schemas/v1/status-reply.json`. `RLXD_CLAUDE_BIN` names the
program rlxd runs as Claude Code; tests point it at `tests/fixtures/fake-claude/`.

## The event bus

One numbered stream of events per home (1.7, 7.3), the `crate::bus` module:

- **`seq`** numbers every event from 1, across topics and across restarts. rlxd keeps a
  mark at or above every seq it has given out in `<home>/rlxd/seq`. It is written 1,000
  seqs ahead, off the bus lock, before half of them are used, made durable (fsync, then
  rename, then the folder fsynced, or write-through on Windows), and written exactly on
  a clean stop. rlxd gives out no seq the mark on disk doesn't cover: when the mark can't
  be written, it retries every second, and once the mark is used up, a publish is
  refused rather than risk a repeat.
- **Gaps.** A client resuming from a seq the buffer no longer holds, a previous rlxd's
  included, first gets `daemon.gap {from, to}`, whose own seq is `to`. A client that reads
  so slowly that the buffer drops events it hasn't read gets one mid-stream. Gaps come
  whatever the topics.
- **Runs.** Run events also carry `run_seq`, per run. The bus indexes them by run, and
  a run's replay from the buffer says the home seq to follow on from.
- **The buffer** holds the last 10,000 events, at most 64 MiB of them, none older than
  24 hours. One event may be at most 16 MiB, the longest line a client reads.
- **`events.subscribe {topics?, since?, follow?}`** replays what the buffer holds after
  `since`, then, while `follow` (the default), live events. Without `follow` it ends at
  the last event published when it was asked. Its reply `{seq}` comes last and ends the
  stream. A connection carries one stream, so event frames carry no `id`. An unknown topic
  gets `unknown_topic`, and a `since` past the last event gets `since_ahead` with
  `detail.head`.
- **A slow client holds up no one.** A subscriber is a place in the buffer, not a
  queue. It is written at its own pace, at most 256 events or 1 MiB at a time, straight
  from the buffer, and when the buffer drops events it hasn't read, it gets a
  `daemon.gap` for them and goes on.
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
- **Packaging a release** builds `cargo build --release` (`-p rlxd` or `--workspace`),
  and nothing else. `--all-targets`, `--tests`, or a binary left behind by
  `cargo test --release` would carry `debug-methods`, because cargo then builds the
  dev-dependency.

## `rlxd here`: what rlx.app asks a host before it connects

rlx.app runs `rlxd here --json --start` on a host over SSH, inside the authenticated
session, before it connects (the owner's remote brief, §3). One envelope answers: rlx's
and rlxd's versions, the protocol versions rlxd serves, whether it runs, where clients
connect and how they get in. With `--start`, an rlxd that is installed but not running is
started first.

**Why `rlxd here` and not `rlx here`:**

- rlxd is the program that serves the protocol, so the versions it reports are compiled
  into it, not looked up.
- It is one native binary, with nothing to resolve on `PATH` or in a virtualenv, so it
  runs as it is from a session that has read no profile, even `env -i`. A probe still
  takes about 0.4 s when rlx is installed: nearly all of it is `rlx --version`, run for
  `rlx.version`.
- `--start` is `rlxd serve --detach`, run by the same binary.
- Its answer is an rlxd-proto type, with its schema in `schemas/v1/` like every wire
  type.

rlx's own version is in the answer too (`rlx.version`), from `rlx --version`.

### Finding it with no profile

A non-login SSH session hasn't read the user's shell profile, so its `PATH` usually lacks
the folder rlx is installed in. Run rlxd by its absolute path, trying these folders in
order (`rlxd_client::install::dirs()` is the same list):

1. uv's tool bin folder, where `uv tool install` puts `rlx`: `$UV_TOOL_BIN_DIR`, else
   `$XDG_BIN_HOME`, else `$XDG_DATA_HOME/../bin`, else `~/.local/bin`. rlxd goes there
   too, beside rlx: it ships with 0.10.3's release binaries, which `rlx daemon install`
   (#21, PR #593) puts in place. The installer and `rlx upgrade` must put it there as
   well;
2. `~/.local/bin`, uv's default, in case a variable moved it only in the profile;
3. Homebrew's `/opt/homebrew/bin` and `/home/linuxbrew/.linuxbrew/bin`;
4. `/usr/local/bin`.

The command an app sends. ssh hands it to the user's login shell, which may be fish or
csh, so it runs under `sh -c`. It runs the first rlxd it finds. When there is none, it
looks for rlx in the same folders:

```sh
sh -c 'set -- "${UV_TOOL_BIN_DIR:-}" "${XDG_BIN_HOME:-}" "${XDG_DATA_HOME:+$XDG_DATA_HOME/../bin}" "$HOME/.local/bin" /opt/homebrew/bin /home/linuxbrew/.linuxbrew/bin /usr/local/bin; for d do if [ -n "$d" ] && [ -x "$d/rlxd" ]; then exec "$d/rlxd" here --json --start; fi; done; for d do if [ -n "$d" ] && [ -x "$d/rlx" ]; then "$d/rlx" --version; exit 5; fi; done; exit 127'
```

- **rlxd found:** its exit code and envelope, in the table below.
- **Exit 5: rlx, but no rlxd.** It is an rlx from before 0.10.3, or one whose rlxd was
  never installed, so it is case 2: "This rlx (vX) isn't supported. Upgrade rlx on the
  host." stdout is what `rlx --version` printed, `rlx 0.10.1`, so its last word is the vX.
- **Exit 127: neither.** rlx is not installed, case 4: "No active rlx instances found on
  <host>."

### Options and exit codes

| Option | |
|---|---|
| `--json` | Print the envelope. Without it, a description for a person, with the same exit code. |
| `--start` | Start rlxd if it is installed, not running, and supported. `data.started` says whether this call started it. |
| `--show-token` | Put the shared client token in `data.auth.token`. It is a secret. |
| `--protocols V` | The protocol versions the caller speaks: `1`, `1-3` or `1,2`. Default: the versions this rlxd speaks. |

| Exit | Case (brief §3) | Envelope |
|---|---|---|
| 0 | 1: rlxd runs and speaks one of the caller's protocol versions | `ok`, `running: true`, `supported: true` |
| 3 | 3: installed, not running (without `--start`) | `ok`, `running: false` |
| 4 | 2: unsupported. rlxd serves none of the caller's versions; `--start` starts nothing | `ok`, `supported: false` |
| 1 | the probe failed: an rlxd that holds its lock but takes no connections, a start that failed, or a home whose path is not UTF-8 | `ok: false` (`disconnected`, `start_failed`, …) |
| 2 | a usage error | none; the message is on stderr |
| 5 | 2: rlx with no rlxd. The command above says it, not rlxd | none; stdout is `rlx --version`'s line |
| 127 | 4: nothing installed. The command above, or the shell, says it | none |

**Exit 3 and 4 come with an `ok` envelope.** Everywhere else, `ok: true` means exit 0
(handoff 7.2, and `envelope.json`'s text). `rlxd here` is the one exception: its answer is
`ok` whenever the probe itself worked, and the exit code says which case it found. So
read `data.running` and `data.supported`, or the exit code, not `ok` alone. (`rlxd status`
also exits 3, with `ok: false` and `not_running`.)

### The answer

`data` is `schemas/v1/here-reply.json`:

```json
{"installed": true, "running": true, "started": false, "supported": true,
 "version": "0.10.3", "protocol_versions": [1], "pid": 4242,
 "socket": "/home/u/.aisquare/run/rlxd.sock", "home": "/home/u/.aisquare",
 "platform": "linux", "auth": {"token_file": "/home/u/.aisquare/rlxd/token"},
 "rlxd": {"version": "0.10.3", "path": "/home/u/.local/bin/rlxd", "protocol_versions": [1]},
 "rlx": {"installed": true, "version": "0.10.3", "path": "/home/u/.local/bin/rlx"}}
```

- **`version`, `protocol_versions`, `pid` and `socket`** are the running rlxd's, as it
  reported them, when it runs. Otherwise they are this rlxd's, which it serves once
  started, and there is no `pid`. A running rlxd that refuses this one's protocol is
  still `running`. Its versions come from its `incompatible_protocol` refusal, and its pid
  and version from the daemon registry when it is registered.
- **Paths** (`home`, `socket`, `auth.token_file`) are absolute, as the app can use them:
  a relative `RLX_HOME` is read from the directory the probe runs in. While rlxd's lock is
  free, it is not running, whatever sits at the socket's path, and `--start` replaces it.
- **`rlxd`** is the program that answered. It can differ from the running one after an
  upgrade that has not restarted it.
- **`socket`** is where to connect. Over SSH, the app forwards a direct-streamlocal
  channel to it.
- **`auth`**: `hello`'s `token` is the contents of `auth.token_file`, which the app reads
  inside the authenticated SSH session, or takes from `auth.token` with `--show-token`.
  The per-device gate credential (5.1) will be added here.
- **`platform`** is the OS as Rust names it (`std::env::consts::OS`): `macos`, `linux` or
  `windows` where rlxd ships, and another name, such as `freebsd`, elsewhere.
- **The protocol range.** This rlxd serves protocol version 1, and `hello`'s reply lists
  the same.

### The shared token over SSH

SSH user authentication is the boundary for the shared token (brief §5). Whoever can run
`rlxd here --show-token` as the host's user can already read `auth.token_file`, and the
exec channel is encrypted, so `--show-token` exposes nothing new. rlxd never writes the
token to its log or into an error. For the app:

- **Never log it.** Keep `data.auth.token` out of logs, error reports and crash dumps.
- **Fetch it again for each session.** Read it inside each authenticated SSH session and
  keep it in memory only. A shared token stored on a device can't be revoked for that
  device alone by `rlxd gate unpair`. Whether the app may store it is a decision the hub
  has not made yet, and until it does, the answer is no.

### The rlxd `--start` starts

It gets a `PATH` it can find Claude Code and rlx on: the session's own first, then this
rlxd's folder, the folders above and the system's (`/usr/bin`, `/bin`, `/usr/sbin`,
`/sbin`), each one only if it exists. It does not get the SSH session's own variables:
`SSH_AUTH_SOCK`, `SSH_CONNECTION`, `SSH_CLIENT`, `SSH_TTY` and `SSH_ORIGINAL_COMMAND` are
removed, so the Claude Code it runs can't use an agent the session forwarded. It runs in a
process group of its own with no terminal, so it outlives the SSH session, unless the
host's systemd-logind has `KillUserProcesses=yes` (no distribution's default), which ends
every process of a closed session. There, rlxd has to run as a user service instead
(`systemd-run --user`, with `loginctl enable-linger` so the user's services outlive the
session).

`.github/workflows/rlxd.yml`'s `ssh` job runs the probe through a real `ssh localhost`
on ubuntu, with the host key pinned and public-key auth only. It checks that each answer
matches the local one and that the started rlxd has no `SSH_*` variables. It also runs the
command above in all three of its cases: rlxd, rlx only (exit 5) and nothing (exit 127).

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
