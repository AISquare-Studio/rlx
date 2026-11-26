# rlxd

rlx's engine: one long-running local daemon per home. It hosts processes (PTYs and
headless Claude Code runs), receives hooks, runs the user's scheduler, and pushes
events to its clients: the TUI, rlxapp and Remote. Python stays the orchestration
brain and the TUI. The plan is `docs/plans/0.10.3-handoff.md`, section 1.

This is the daemon's core (1.3, 7.2, 7.4), its event bus (1.7, 7.3) and the app gate's
authentication (5.1): one rlxd per home, on a local socket or a named pipe, a token
`hello`, the envelope on every reply, `status`, `events.subscribe`, and per-device gate
credentials with the peer identity check. The PTY host, the runner and the rest arrive
with later items, each a method on this socket that publishes on the bus. `rlxd-hook`
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
rlxd serve [--detach] [--gate-dev-mode]  # run rlxd for this home; --detach returns once it takes connections (--gate-dev-mode: dev builds only)
rlxd status [--json]    # what it is, who is connected, the gate's mode, and Claude Code's sign-in
rlxd stop               # stop it, and wait until it has
rlxd call [--as-app] [--role R] [--client NAME]
          [--gate-credential C | --gate-credential-file F] [--token T | --token-file F]
          [--protocol-version N] [--env JSON] [--socket PATH] [--raw] <method> [<params-json>]
rlxd events [--as-app] [--topics a,b] [--since SEQ] [--follow] [--until TYPE[,TYPE…]]
            [--limit N] [--timeout S] [--socket PATH] [--token T | --token-file F]
            [--gate-credential-file F]
rlxd here [--json] [--start] [--show-token] [--protocols VERSIONS]
                        # what is installed and running here: rlx.app's probe over SSH
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
  folder). For a home nested too deep for that path to fit the OS's socket path limit,
  the short socket `/tmp/rlxd-<uid>/<hash of home>.sock` instead: see
  [Long homes](#long-homes-the-short-socket). On Windows, the named pipe
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
    connection that used the old one: a last `not_gate` envelope with no `id`, which
    also ends an event stream the connection carries. A new device, once one is paired, waits up to
    120 s for an approval: a paired app answers `gate.pair_answer` (it lists them with
    `gate.pair_requests`), or the user does on the host. `rlxd gate pair` lists each
    waiting request with its id, device name, route and the process that asked (pid and
    binary), and approves only the one `--request-id` names. A request whose client
    leaves is withdrawn, and a credential whose reply can't be delivered is revoked, so
    no device is left paired with a credential nobody received; an undelivered
    re-pairing keeps the device's old credential. That holds when rlxd stops with the
    reply still unsent too. Device names are NFKC-normalized and trimmed, then may hold
    only spaces, and letters, digits, `.`, `_` and `-` that UTS #39 allows in
    identifiers, in one script, and must not look blank. So combining letters and
    fillers are refused, and so is every letter of the scripts UTS #39 marks Limited
    Use or Excluded, such as Cherokee, Inuktitut, Tifinagh or Javanese: a security
    trade-off, accepted for the gate, and the refusal says so. Names that only look
    alike (by their UTS #39 skeleton, with diacritics, case and spacing aside, so
    `laptọp` and `café` compare as `laptop` and `cafe`) count as one, so they are
    unique, and a new device may take the name of one the user unpaired, or a
    look-alike, only with `--force`.
    `rlxd gate unpair [<device>]` is the only reset; unpairing one device never touches
    another. A `hello {role: "app"}` without a paired device's credential (an empty one
    included) gets `not_gate`, and so does anything else the gate refuses. A client
    that half-closes its connection still gets every reply; one that goes away entirely
    withdraws its waiting request.
  - **The peer's identity**, checked for a device paired locally, on every gate hello
    and every `gate.pair`. macOS: the `LOCAL_PEERTOKEN` audit token, checked by
    Security.framework against a requirement compiled into rlxd (Apple's anchor, the
    identifier `studio.aisquare.rlx` and the team), plus the hardened runtime and no
    injection entitlements. Linux: `SO_PEERCRED`'s uid, the process pinned with
    `SO_PEERPIDFD`, and, where rlx's app is installed from a package, its binary.
    Windows: the pipe client's process, opened at once and held, its creation time,
    its image path, and the Authenticode signature (revocation checked, offline from
    the cache) of the file that image is mapped from, whose signer's full subject and
    issuer must be rlx's publisher's. **On every OS this is best-effort against code
    running as the same user, and the credential is the real boundary.** On macOS,
    `LOCAL_PEERTOKEN` names the last process to act on the client's socket, so same-user
    code can borrow a signed binary's write and pass as it; rlxd refuses a peer holding
    the connection on its fd 0 to 2, but not every borrowing. On Linux and Windows such
    code can hand the connection on and exec or inject into the app. **The exposure
    window is the first pairing,** trust on first use, before any credential exists.
    `crates/rlxd/src/peer.rs` says why, and holds the expected identity's constants.
  - **SSH.** Over SSH, a connection's peer is sshd, not the app, so a device paired
    inside an authenticated SSH session will be admitted by its credential alone: the
    SSH pairing plus the per-device credential is the boundary. SSH pairing is a later
    item (hub q1). Until it lands, rlxd mints no ssh-route credential, and refuses and
    logs any ssh-route device it finds in gate.json. A connection that only reaches the
    socket never pairs that route.
  - **Gate dev mode** (`--gate-dev-mode`, or `gate.dev_mode = true` in
    `<home>/rlxd/config.toml`) is for unsigned dev and CI builds of the app, and never
    the default. It exists only in an rlxd built with the cargo feature
    `gate-dev-mode`, which only rlxd's dev-dependency on itself turns on, as
    `debug-methods`: a release build has no `--gate-dev-mode` flag (a usage error),
    ignores `gate.dev_mode` in its config, with a warning, and says its version alone,
    while a feature build's `rlxd --version` ends in `(dev: gate-dev-mode)` and its
    `status` lists `dev_features: ["gate-dev-mode"]`. Otherwise any process of the
    user's could restart rlxd in dev mode and pair. In dev mode it skips only the peer
    identity check; the credential and every pairing rule still hold. rlxd logs it at
    start and on each gate hello, and `status` says `gate: "dev mode (unsigned app
    accepted)"` instead of `"signed app only"`. No config key, variable or flag changes
    the identity a release build expects.
- **Logs** are `<home>/rlxd/logs/rlxd.log`, rotated at 5 MiB, three kept.
- **Detached** (`--detach`, and the rlxd `rlxd here --start` starts), rlxd holds nothing
  of the process that started it: its stdin, stdout and stderr are null (it logs to its
  file), and on macOS and Linux it first closes every other descriptor it inherited. So
  a caller that reads its starter's output to the end, such as `rlxd here --start | jq`,
  is never left waiting for rlxd to stop.
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
  envelope. `--as-app` presents the test gate credential that `rlxd call --as-app` kept
  when it paired, and never pairs itself.
- **The `daemon` topic:** `daemon.started {version, pid}`, `daemon.stopping {reason}`
  and `daemon.gap {from, to}`.
- **`debug.emit {count}`** publishes `count` events, for tests. It exists only with the
  cargo feature `debug-methods`, which only rlxd's dev-dependency on itself turns on:
  `cargo test` has it, `cargo build --release` never does, and
  `crates/rlxd/tests/release_build.rs` builds a release rlxd to check.
- **Packaging a release** builds `cargo build --release` (`-p rlxd` or `--workspace`),
  and nothing else. `--all-targets`, `--tests`, or a binary left behind by
  `cargo test --release` would carry `debug-methods` and `gate-dev-mode`, because cargo
  then builds the dev-dependency. `rlxd --version` says which: a release build's is
  `rlxd <version>` alone.

## Settings

App settings kept in the rlx home and served to every device (the owner's multi-device
brief, §4), the `crate::settings` module. They are not for secrets: every client with the
token reads every one.

- **The methods:** `settings.get {key}`, `settings.set {key, value, expected_version?}`,
  `settings.list {prefix?}` and `settings.delete {key, expected_version?}`, each through
  `rlxd call`. A stale `expected_version` gets `conflict`, with `detail.version`.
  Versions come from one count for the whole home, so a key's only rises, even across
  a delete and a re-create.
- **The keys:** `theme`, `layout`, `agent_sign_off`, `send_safety_mode`, and any dotted
  key, `<namespace>.<name>`, for later needs. Their shapes are in `schemas/v1/README.md`.
- **Protected keys:** `send_safety_mode` and every key under `policy.`. Only an app-role
  connection may set or delete them; any other gets `not_gate`. Every other key, agents
  and preferences included, any client may change, so a CLI or TUI user with no app can
  too.
- **The log.** Every change is logged with who made it. A `send_safety_mode` change that
  lowers send safety for any connector is logged as a warning. It needs no confirm in
  rlxd yet; that is with the hub.
- **Events:** each change, once on disk, is `settings.changed` or `settings.deleted
  {key, version, by_device?}` on the `settings` topic.
- **Limits:**
  - a value is at most 64 KiB of JSON and 32 arrays or objects deep;
  - a home holds at most 512 settings and 8 MiB of keys and values, so
    `settings.list` always fits the 16 MiB line a client reads.
- **The file** is `<home>/rlxd/settings.json`: 0600, even over a stale temporary file
  left by a crash, compact JSON, written whole and durably. A change rewrites it as
  blocking work, so no other connection waits on the disk.
  - It is read key by key. A key rlxd can't hold, such as a value nested too deep or
    over 64 KiB, is set aside alone, to a new `settings.json.unreadable-<time>`, and the
    rest are kept. If that file can't be written, the key stays as it was, unserved.
  - A key a newer rlxd wrote is kept as it is, and not served.
  - A file rlxd can't read, or one that isn't a settings document at all, is moved to
    such a file whole and logged as an error, and rlxd starts with no settings. If it
    can't be moved either, rlxd refuses every change until a restart, so it never
    writes over a file it could not read.
- **An app-role connection** is one that holds the gate (5.1): a device `gate.pair`
  paired, whose credential its `hello` carries. One whose device is unpaired, or whose
  credential is replaced, can no longer change a protected key.
  `crates/rlxd/tests/settings.rs` pairs a device for real, in gate dev mode.

## PTYs

rlxd holds processes on pseudo-terminals (handoff 1.4), as tmux held the fleet's panes:
ConPTY on Windows. The `crate::pty` module is the host, and `crate::vt` is the screen.

- **The methods:** `pty.spawn {argv, cols, rows, cwd?, env?, label?}`, `pty.write
  {pty_id, text | keys | paste}`, `pty.resize`, `pty.screen {pty_id, scrollback?}`,
  `pty.subscribe {pty_id, read_only}`, `pty.kill` and `pty.list`, each through `rlxd
  call` but the stream. Their shapes are in `schemas/v1/README.md`.
- **The stream:** `pty.subscribe` sends the screen (`pty.snapshot`), then the PTY's
  `pty.output` and `pty.exited`. A client too slow to keep up gets a fresh snapshot
  instead of what it missed; the PTY never waits for it. `pty.output` has a partition of
  the bus to itself (20,000 events, 32 MiB), so it never pushes another topic's events
  out.
- **The screen is tmux's.** `pty.screen`'s `text` is what `tmux capture-pane -e -p`
  prints for the same screen, byte for byte. `crate::vt` is tmux 3.8's own grid, screen
  writes and input parser, ported. Each ported file keeps tmux's ISC notice.
  `tests/vt_tmux.rs` checks it against tmux itself on 41 screens, and `tests/pty.rs`
  runs the handoff's own `cmp`.
- **A PTY outlives every client,** and one any connection spawned is in every
  connection's `pty.list`. A process that ends keeps its screen and exit status until
  `pty.kill`, as tmux's `remain-on-exit on` keeps a pane. `rlxd stop` ends them all.
- **`rlxd attach <pty_id> [--read-only]`** puts a PTY in this terminal, as `tmux -L asq
  attach` did: its screen, then its output; this terminal's keys sent by name and its
  pastes as pastes, so rlxd sends them as the program's modes need; the PTY sized to
  this terminal. Ctrl-] detaches. `--read-only` sends nothing and leaves the size alone.
- **`pty.kill`** ends the process's whole group: SIGHUP and SIGTERM, then SIGKILL after
  two seconds. It replies once the group is gone.
- **On Windows** each PTY's processes are in a Job Object that rlxd makes, named with
  128 random bits, with `KILL_ON_JOB_CLOSE`; `pty.kill` ends the job. The ConPTY's
  process is rlxd itself in a hidden mode, `rlxd __pty-shim`, which joins the job before
  it starts the program, so nothing the program starts escapes it, and exits with the
  program's exit code, all 32 bits. The shim lives through Ctrl-C and Ctrl-Break, which
  the console sends the program too, and turns Ctrl-C back on for the program first: a
  detached rlxd's process group ignores it, and every process would inherit that. `pty.list`'s `pid` there is the shim's.
- **Who may do what:** `cli` and `tui` every method; `remote` the reads and
  `pty.write`; `app` the reads only. Any other call gets `role_not_allowed`.
- **Limits:** 64 PTYs a home, the exited ones included; 50,000 lines of history each;
  1 MiB a write.
- **The terminal** is `TERM=xterm-256color`, `COLORTERM=truecolor`, unless `env` says
  otherwise.

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
- **`socket`** is where to connect: the home's `run/rlxd.sock`, or its short socket in
  `/tmp/rlxd-<uid>/` for a deep home. Over SSH, the app forwards a direct-streamlocal
  channel to it ([Over SSH](#over-ssh-the-socket-through-a-direct-streamlocal-channel)).
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

## Over SSH: the socket through a direct-streamlocal channel

rlx.app reaches a host's rlxd as VS Code Remote-SSH reaches its server: through the SSH
connection itself, on a direct-streamlocal channel to rlxd's socket (the owner's remote
brief, §3). Nothing listens on a TCP port, on the host or on the device.

1. Inside the authenticated session, run the probe: `rlxd here --json --start`, with
   `--show-token`, or read `auth.token_file` over the session. Its `socket` is the path to
   open the channel to, and the token is `hello`'s.
2. Open a direct-streamlocal channel to `socket`. sshd connects to the socket as the user
   who logged in, whom its 0600 mode admits. rlx.app's SSH client opens the channel
   itself. With OpenSSH, `ssh -N -L <local socket>:<socket> host` makes a local socket
   whose every connection is one such channel.
3. Speak the protocol on the channel as on the socket: `hello` with the token, then
   requests and event streams. From a shell, with OpenSSH:

   ```sh
   dir=$(mktemp -d)   # 0700: no one else can bind the forward's path first or read the token
   here=$(ssh host '<rlxd> here --json --start --show-token')
   (umask 077 && printf '%s' "$here" | jq -r .data.auth.token > "$dir/token")
   ssh -f -N -o ExitOnForwardFailure=yes \
     -L "$dir/rlxd.sock:$(printf '%s' "$here" | jq -r .data.socket)" host
   rlxd call --socket "$dir/rlxd.sock" --token-file "$dir/token" status
   rlxd events --socket "$dir/rlxd.sock" --token-file "$dir/token" --follow
   ```

   `<rlxd>` is rlxd's absolute path on the host ([Finding it with no
   profile](#finding-it-with-no-profile)). The forward's local socket goes in a folder
   only you may enter, so no other local user can take its path before ssh does.
   `ExitOnForwardFailure=yes` makes ssh exit rather than run on without the forward when
   it can't, and with it `-f` puts ssh in the background only once the forward is up, so
   the next command finds the socket. `--token-file` keeps the host's token out of the command line, which other
   local users can read (`ps`); `--token` works too, but shows it.

   `--socket` connects there instead of to the home's socket. It sends nothing of the
   local home's, and keeps nothing there:
   - the token must be given, with `--token-file` or `--token`: without either,
     `--socket` is a usage error (exit 2), and the home's token file is never read;
   - `--as-app` needs `--gate-credential-file <file>` (or `--gate-credential`), where
     the test client keeps its credential for the rlxd at `--socket`: the home's
     `test-gate-credential` is never sent there, nor that rlxd's credential kept here;
   - the socket must be a socket, not a link, that this user owns, as ssh makes it, and
     the process listening on it must run as this user, read from the connection
     itself (`getpeereid`, `SO_PEERCRED`): otherwise nothing is sent. A connection to
     the home's own socket is checked the same way;
   - when nothing listens there, the hint is the forward and `rlxd here --start` on
     the host, never this home's `rlxd serve`.

- **sshd must allow it,** as OpenSSH's defaults do. The channel needs local forwarding
  allowed twice: `AllowStreamLocalForwarding` `yes` or `local`, and
  `AllowTcpForwarding` `yes` or `local` with `PermitOpen any` (the default), because
  OpenSSH applies its local TCP forwarding permission to direct-streamlocal channels too
  (`session.c` disables all local forwarding for `AllowTcpForwarding no`, and
  `channel_connect_to_path` checks it). So a host hardened with `AllowTcpForwarding no`,
  a `PermitOpen` list, `AllowStreamLocalForwarding no` or `remote`, or
  `DisableForwarding yes` refuses the channel, and ssh says `open failed`. The narrowest
  setting that works is `AllowStreamLocalForwarding local` with `AllowTcpForwarding
  local`: no remote forwards of either kind, and no listening.
- **An rlxd that does not run** shows through the channel as a connection that closes
  before `hello`'s reply: sshd could not connect to the socket, and ssh says `connect
  failed`. Run `rlxd here --start` first, and again when a connection closes.
- **The gate** checks a forwarded connection as it checks any other, on the short socket
  too: the token, then for the app role the credential and the peer. A forwarded
  connection's peer is sshd, not rlx's app, so outside gate dev mode it can neither pair
  nor be the gate, until SSH pairing (hub q1) gives SSH devices a credential of their own
  (see the gate's **SSH** note above). `rlxd call --socket … --as-app
  --gate-credential-file <file>` pairs with the rlxd at `--socket`, never with the local
  home's.
- **Windows hosts** are an open question for the hub (q2): rlxd serves a named pipe there,
  not a socket. This is built and checked for macOS and Linux hosts.

`.github/workflows/rlxd.yml`'s `ssh` job runs it on ubuntu through a real `ssh -N -L` to
localhost, for `~/.aisquare` and for a home too deep for its own socket. Its sshd is that
narrowest setting, `AllowStreamLocalForwarding local`, `AllowTcpForwarding local` and
`PermitListen none`, with no agent, X11 or tunnel forwarding, which `sshd -T` must show
in effect for the job's own connection (`-C user=…,host=localhost,addr=127.0.0.1`, so a
`Match` block counts). A remote (`-R`) forward of a Unix socket and of a TCP port must
each be refused. A second sshd with `AllowTcpForwarding no` must refuse the very `-L` to a
Unix socket that the first one serves, which shows the constraint above and fails if
OpenSSH ever drops it. The client side is a home with no rlxd and no token, and the
host's token goes in a 0600 file for `--token-file`. Through the forward:

- `--socket` with no token is a usage error;
- `status` must answer, naming the host's socket and home;
- the gate must refuse the unsigned test client: `--as-app` (with a credential file), a
  made-up credential and `gate.pair` each get `not_gate`, and nothing is paired or kept;
- an `rlxd events --since 0 --until daemon.stopping` stream must replay this rlxd's
  `daemon.started`, stay open through 60 quiet seconds, and take `daemon.stopping` live
  when rlxd stops over ssh;
- neither rlxd nor ssh may listen on TCP or UDP;
- once the forward is gone, the same call exits 3.

### Long homes: the short socket

A socket's path must fit `sockaddr_un`'s `sun_path`: 107 bytes on Linux and 103 on macOS
(`rlxd_client::SOCKET_PATH_MAX`). `rlxd_client::Home::socket` decides where it is, so
rlxd and every client agree, however each spells the home:

- the home's `run/rlxd.sock`, by its **canonical** path (links, `.` and `..` resolved;
  macOS's `/var` is `/private/var`), when that fits. rlxd binds it, and every client
  connects to it, by that path, so a home reached through a link, or as `/var` and
  `/private/var`, is one socket;
- else `/tmp/rlxd-<uid>/<hash>.sock`, about 40 bytes. `<hash>` is the first 16 hex
  digits of the SHA-256 of the home's canonical path, its bytes as they are, so two
  homes whose paths aren't UTF-8 never share one.

`status`'s `paths.socket` and `rlxd here`'s `socket` report the path in use, and rlxd's
log says why it uses the short one.

- **Why `/tmp`.** Every process of the user must name the same folder, whatever its
  environment holds: a desktop session, a non-login SSH session, `env -i`. `$TMPDIR`
  differs between them on macOS. logind removes `$XDG_RUNTIME_DIR` at the user's last
  logout, which an rlxd started over SSH outlives.
- **Only its user's.** `/tmp` is shared, so another user can make `/tmp/rlxd-<uid>/`
  first. rlxd makes the folder 0700, and refuses to start when that path is not a folder
  (a link included), is another user's, or has any mode wider than 0700. It never
  changes a folder it finds into shape: while it was wider, another user could have left
  entries in it. The error names the folder; remove it once no rlxd uses it, and rlxd
  makes it again. A client sends the token only to a socket its user owns, in a folder
  its user owns that no one else may enter. Otherwise it fails before it sends anything
  (`ConnectError::NotOurs`).
- **A sticky `/tmp`.** rlxd and its clients check the folder, then use it by path. That
  is safe because `/tmp` is root's with the sticky bit set, so no other user can rename
  or replace our folder in between. Both check that it is, and refuse otherwise. rlxd
  also looks at the folder again after it binds, and refuses one that changed.
- **Replacing a socket.** A socket rlxd finds at its path is replaced only when nothing
  listens on it, such as one left by an rlxd that was killed. One that still takes a
  connection is someone else's, and rlxd refuses to start rather than take it over.
- **A stop** removes the socket only if it is still the one this rlxd made, and leaves
  the folder, which other homes' sockets may share.

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

Every `cargo test` builds rlxd with the dev-only cargo features `debug-methods` and
`gate-dev-mode`, through rlxd's dev-dependency on itself, so the tests can start it with
`--gate-dev-mode`; nothing else is needed. To run such an rlxd by hand, as the test
client of the handoff's checks or an unsigned app build wants, build it with the
feature, and check that its version says so:

```sh
cargo build -p rlxd --features gate-dev-mode
target/debug/rlxd --version   # rlxd <version> (dev: gate-dev-mode)
target/debug/rlxd serve --detach --gate-dev-mode
```

A plain `cargo build` or `cargo build --release` has no dev mode: `--gate-dev-mode`
is then a usage error, and `tests/release_build.rs` checks a release build for it.

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

`tests/stress-here.sh [RUNS]`, which CI does not run, runs `cargo test -p rlxd --test
here` RUNS times (30 by default), each within a time limit, and fails if a run fails,
hangs, or leaves an rlxd running. It builds and tests, so on a shared machine run it
through the memory gate: `rlx fleet gate -- tests/stress-here.sh 30`.

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
`rlxd-SHA256SUMS` over them, which `shasum -a 256 -c` reads. The Linux binaries are
built in Ubuntu 22.04, so they run on glibc 2.35 or newer (Ubuntu 22.04, Debian 12 and
later), and CI runs them there before packing them.

- **Every head.** rlxd.yml calls it, so each run uploads the artifact `rlxd-archives`,
  and installs it with `rlx daemon install` on ubuntu, macOS and Windows.
- **A release.** Publishing GitHub Release `vX.Y.Z` attaches the five archives and the
  sums to it. The tag must be `v` + `pyproject.toml`'s version.
- **By hand.** `gh workflow run rlxd.yml --ref vX.Y.Z -f release_tag=vX.Y.Z` does a
  dry run of that attach: every check, then the `gh release upload` it would run, printed.
  Add `-f dry_run=false` to attach for real. It runs on the tag only: any other `--ref` is
  refused (`ref_not_tag`), and so is a tag that has moved since (`tag_moved`), so what is
  attached is always built from the tag. After a failed release run, re-run its failed
  jobs instead: that run is the tag's.
- **No test or dev features.** The build names its cargo features (`RELEASE_FEATURES`,
  none) with `--no-default-features`, never `--all-features`, and refuses a list naming
  a test or dev one. Gate dev mode is a cargo feature releases never enable (#24), and
  `package` fails if any binary's `--version` carries its marker, `(dev: …)`, or is
  anything but `<binary> <version>`.
- **Signing.** None yet: notarization and Authenticode are an owner step, wired in at
  the marked step of the build job.

`rlx daemon install` (`../src/aisquare/services/rlxd_install.py`) fetches
`rlxd-SHA256SUMS` and this machine's archive for `rlx --version` from
`https://github.com/AISquare-Studio/rlx/releases/download/v<version>/`, or from
`$AISQUARE_RELEASES_URL/v<version>/` when that is set, which must be `https://`, or
`http://` to this machine (127.0.0.1, ::1, localhost) for a test server. A redirect is
followed only while it stays `https`. It checks the sha256 before it writes anything,
then installs both binaries into `uv tool dir --bin`, beside `rlx`: both or neither, so
`rlxd` and `rlxd-hook` are never from different versions. On Windows a running `rlxd.exe`
is renamed aside, and the next install removes it.

## Licence

MIT, as rlx is (`../LICENSE`).

`crates/rlxd/src/vt/` is ported from tmux 3.8 (its grid, screen writes, input parser
and UTF-8 handling), and stays under tmux's ISC licence: its copyright and permission
notice heads each of those files.
