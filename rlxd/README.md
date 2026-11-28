# rlxd

rlx's engine: one long-running local daemon per home. It hosts processes (PTYs and
headless Claude Code runs), receives hooks, runs the user's scheduler, and pushes
events to its clients: the TUI, rlxapp and Remote. Python stays the orchestration
brain and the TUI. The plan is `docs/plans/0.10.3-handoff.md`, section 1.

This is the daemon's core (1.3, 7.2, 7.4), its event bus (1.7, 7.3) and the app gate's
authentication (5.1): one rlxd per home, on a local socket or a named pipe, a token
`hello`, the envelope on every reply, `status`, `events.subscribe`, and per-device gate
credentials with the peer identity check, and the headless runner with its spaces (2.1,
2.5, 7.5). The PTY host and the rest arrive with later items, each a method on this
socket that publishes on the bus. `rlxd-hook` answers `--version` only for now.

```
rlxd/
  crates/rlxd-proto/   wire types, schema_version, JSON Schemas (schemars); no I/O
  crates/rlxd-client/  the library rlxapp depends on: the home's paths, connect, hello,
                       call, subscribe
  crates/rlxd/         the daemon: serve, status, stop, call, events, and the bus
  crates/rlxd-hook/    the tiny hook client (`--version` only for now)
  crates/fake-claude/  the tests' stand-in for Claude Code (a test-only binary)
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
program rlxd runs as Claude Code; tests point it at `tests/fixtures/fake-claude/`. It is
read from rlxd's own environment only, and wins over the app's `PATH` (see [The app's
environment](#the-apps-environment-for-claude-code)).

`status.claude` (7.4) comes from a probe in the background, so `status` never waits on
Claude Code for long:

- **The probe** runs the `claude` rlxd resolved, with the app's environment: `claude
  --version` and `claude auth status --json`, at once, each given 5 s. `signed_in` is its
  `loggedIn`. `account` is its `email`, and `plan` its `subscriptionType`. `auth_method` is
  its `authMethod` (`claude.ai`, `api_key`, `oauth_token`), or for `third_party` the
  provider it names (`bedrock`, `vertex`, `foundry`, …).
- **When.** At rlxd's start, when the app's values or the `claude` they resolve to
  change (`env.set`, an app `hello`'s `env`), and when `status` finds the last answer
  older than 30 s. A probe for an older environment never overwrites a newer one's answer.
  An `env.set` or app `hello` with the values rlxd has, as on each reconnect, changes
  nothing: `status` keeps the answer and its `as_of`.
- **What `status` gets.** The last answer, with `as_of`, when its probe ended. A probe
  in flight is waited for at most 500 ms. Before the first one ends, and after a change
  until the new one ends, there is no `as_of`, and `signed_in` is false: no `as_of` means
  still checking, not signed out. A probe that ends however it ends, a panic included, no
  longer runs, so a later `status` probes again.
- **A probe that fails** (Claude Code won't start, doesn't answer within its time, prints
  no JSON) reads as signed out, with `error` saying why. `error` never holds what Claude
  Code printed.
- **Not installed** gives `installed: false`, with only `path_source` beside it.
- Only §7.4's fields of Claude Code's answer reach `status`: never its org, folders or
  sign-in expiry. The probe's output is never logged.

The rest of `status`: `runs` counts the runner's live and queued runs, `jobs` is the
scheduler's ([The scheduler](#the-scheduler)), and `capabilities` lists what this rlxd can do by 5.3's names
(`connectors.call`, `connectors.no_turn`, `connectors.gate_before_turn`), none of which
exist yet. `crates/rlxd/tests/status.rs` runs 7.4's Accept lines and these, on three
OSes, and `rlxd-proto`'s schema tests check `status` declares every field of 7.4's JSON.

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
  a run's replay from the buffer says the home seq to follow on from. The runner writes
  each run event to the run's log on disk before the bus publishes it: the seq is held
  meanwhile, and later publishers wait for it, never subscribers.
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

## The app's environment for Claude Code

A GUI app's `PATH` lacks the user's shell `PATH`, so rlx.app reads `PATH`,
`CLAUDE_CONFIG_DIR` and `CLAUDE_SECURESTORAGE_CONFIG_DIR` once from a login shell and
hands them to rlxd (handoff 0.10.3, 4.3). They decide which `claude` every gated run
starts and where it reads its settings, so only the gate holder sets them.

```sh
rlxd call --as-app env.set '{"CLAUDE_CONFIG_DIR":"/Users/u/.claude-work"}'
rlxd call --as-app --env '{"PATH":"/opt/homebrew/bin:/usr/bin:/bin"}' status
rlxd call env.get
```

- **`env.set {<key>: <value>, …}`**, and `env` in a `hello`, are taken only from an
  app-role connection that holds the gate (5.1). Anything else gets `not_gate`, and
  nothing changes: an `env.set` from a `cli` or `tui` client, a `hello` with `env` that
  doesn't take the app role, or an app `hello` the gate refuses, whatever keys its `env`
  holds. **`env.get`** is open to every client. Each replies with every value now: `schemas/v1/env.json`.
- **Keys.** Exactly the three. Any other key gets `bad_env_key`, and so does the whole
  request, valid keys included: nothing changes, and an app `hello` refused so takes no
  hold of the gate. A key the request doesn't name keeps
  its value, and an empty value removes its key.
- **Values** are strings of at most 64 KiB with no NUL (`invalid_params` otherwise),
  kept as sent: a relative `CLAUDE_CONFIG_DIR` stays relative.
- **`<home>/rlxd/env.json`** (0600) keeps them, written whole and durably (a 0600
  sibling, fsynced, renamed over it), so an rlxd started at login has them too. One that
  can't be read (not JSON, a key this rlxd doesn't know, a value too long) is moved
  aside whole to `env.json.unreadable-<time>`, logged as an error, and rlxd starts with
  no app environment, as the settings do. If it can't be moved either, every change is
  refused until it is fixed and rlxd restarted. A change that can't be written (that, or
  a full disk) refuses its request with why (`internal`), an app `hello` carrying `env`
  included: the app then holds no gate until the file is fixed, or it says hello
  without `env`.
- **`claude`** is resolved when the values are taken in (`env.set`, an app `hello`'s
  `env`) and when rlxd starts, never when a run starts: the first `claude` on the app's
  `PATH`, in order, else on rlxd's own `PATH` when the app set none. Empty and relative
  entries are skipped. The path is kept as found, an absolute path with links
  unresolved. `RLXD_CLAUDE_BIN`, from rlxd's own environment only, wins over both.
  `status.claude` reports it: `installed`; `path`, the program a run starts; and
  `path_source`, where rlxd looked: `app` (the app's `PATH`), `rlxd` (its own, as the
  app set none) or `rlxd_claude_bin`.
  - **Windows** tries `claude` with each of `PATHEXT`'s extensions, so `claude.exe`. A
    batch file found first on a `PATH`, such as npm's `claude.cmd` shim, isn't run: Rust
    starts one through `cmd`, whose rules no escaping makes safe for a prompt. `status`
    then says Claude Code isn't installed. `RLXD_CLAUDE_BIN`, the tests' override, is
    taken as it names, as the `auth-status.cmd` fixture needs.
- **Runs** start that `claude`, by its absolute path, with the values raw on top of
  rlxd's own environment. A run takes the values and the `claude` current when its
  attempt's process starts: one still queued picks up a change made before it starts,
  and one running keeps what it started with. `run.start` and `run.resume` refuse
  `claude_not_found` when no `claude` was found where rlxd looked, and
  `claude_not_runnable` when the one found is a batch file. The message says where it
  looked. Every other Claude Code rlxd starts does the same: a connector's login, a
  `connectors.call`, and the connectors' reads and one-turn probe, each on its account's
  environment ([Connectors](#connectors)).
- **The brain** (`rlx brain --stdio`) runs with the values raw on top of rlxd's own
  environment too, as they are when it starts. A change that alters them starts it again
  with the new values once it is idle: the request it runs ends first, and the next one,
  queued or not, runs on the new brain. It is stopped as rlxd's stop stops it and started
  at once, with no backoff and no `error`; `status.brain.restarts` counts it. A change to
  the values it already has doesn't restart it.
- **Keychain.** rlx keeps `/usr/bin/security` as the only Keychain reader, and rlxd
  links no Keychain crate. `deny.toml` bans `security-framework` and `keyring` for
  `rlxd.yml`'s `cargo deny`. `tests/env.rs` runs 4.3's line as written (`cargo tree
  --manifest-path rlxd/Cargo.toml --target all -e normal > tree.txt && …`) through `sh`
  on macOS and Linux, and its `cargo tree` and `grep` on every OS.

`crates/rlxd/tests/env.rs` runs 4.3's Accept lines that start no run, and
`crates/rlxd/tests/env_runs.rs` those that do, with fake-claude's `print-env`, on three
OSes.

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
- **A program's output can't take rlxd down.** The model parses what a program writes,
  so it takes hostile bytes too; a seeded random test feeds it such bytes, with resizes
  in the middle of sequences, and checks it never panics and its memory stays bounded.
  Should it panic anyway, that PTY alone ends: rlxd catches the panic, ends the
  process's group, and the PTY's screen (blank from then on), list entry and
  `pty.exited` say `error: "screen_failed"`. rlxd, its other PTYs and every connection
  go on. Unlike tmux, a hyperlink whose id is over 1 KiB is dropped, as one whose URI is:
  tmux keeps up to 5,000 ids of any length.
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
  detached rlxd's process group ignores it, and every process would inherit that.
  `pty.list`'s `pid` there is the shim's.
- **Who may do what:** `cli` and `tui` every method; `remote` the reads and
  `pty.write`; `app` the reads only. Any other call gets `role_not_allowed`.
- **Limits:** 64 PTYs a home, the exited ones included; 50,000 lines of history each;
  128 KiB a write, so the longest one fits a frame however it is escaped. A longer write
  gets `write_too_large`, and a frame over 1 MiB `frame_too_large`; the connection stays
  open for both. A paste over 128 KiB would need a paste in parts: §1.8's to decide.
- **The terminal** is `TERM=xterm-256color`, `COLORTERM=truecolor`, unless `env` says
  otherwise.
- **The environment** is rlxd's, less what the fleet's tmux server takes out of a
  window's (`core/spawn.py`'s `tmux_client_env`) and tmux's own `TMUX`, `TMUX_PANE`,
  `TERM_PROGRAM` and `TERM_PROGRAM_VERSION` (`src/pty/env.rs`). `tests/pty.rs` checks
  the lists against Python's through `uv run`, so they can't drift.
- **A process that ends** stays a zombie until `pty.kill` or `rlxd stop` reaps it, after
  signalling its group, so the group's id is never another's by then.

## The runner and spaces

rlxd runs Claude Code headless, with no tmux and no PTY (handoff 0.10.3, 2.1, 2.5 and
7.5), the `crate::runner` and `crate::spaces` modules.

- **`run.start`** (`schemas/v1/run-start.json`) takes the app's `run_id`, a lower-case
  UUID that is also Claude Code's `--session-id`, and refuses `run_exists`,
  `kind_mount_mismatch` (also `needs_connectors` with mount `no_tools`, which loads no
  connector) and `pinned_call_required`. Each kind has its own pool (tasks 2, refreshes and
  watches 1, the rest 2); over it, a run waits as `run.queued {position}`.
- **The command line** is 2.1's exactly: `-p --output-format stream-json --verbose
  --input-format stream-json --session-id <run_id> [--no-session-persistence] --tools ""
  [--strict-mcp-config] [--json-schema …] [--append-system-prompt …]
  [--include-partial-messages] --settings <home>/rlxd/runs/<run_id>/<attempt>/settings.json
  --setting-sources project --permission-mode dontAsk`, never `--allowedTools` or `--bare`.
  Each attempt starts the absolute `claude` resolved when the app last set its
  environment, or when rlxd started (4.3, never a lookup at the run's start), with the
  app's `PATH`, `CLAUDE_CONFIG_DIR` and `CLAUDE_SECURESTORAGE_CONFIG_DIR` on top of rlxd's
  own environment ([The app's environment](#the-apps-environment-for-claude-code)).
  `run.start`, `run.resume` and `connectors.call` refuse `claude_not_found` when that
  resolved to none, and `claude_not_runnable` when to a batch file.
- **Each attempt's settings** hold one PreToolUse hook (matcher `*`) running `rlxd-hook
  pre-tool-use --run <run_id> --attempt <n> --deadline-ms <deadline_ms>` by the absolute
  path of the `rlxd-hook` beside rlxd (quoted when it must be; on Windows in forward
  slashes, always quoted), with a timeout of `deadline_ms / 1000 + 10` s. When rlxd can't
  name that path, `run.start` refuses `hook_not_found`; it never falls back to a name on
  `PATH`. Its requests are decided by the tool-call gate below. rlxd-hook reads the
  attempt's key from its `RLXD_HOOK_KEY` and sends it as `hook`'s `hook_key` only (never
  with the hook's variables, never on its stdout or stderr), and rlxd binds the request to
  the run's live attempt by that key and by the process that connected. Its own timer is
  `deadline_ms` and 5 s, past the gate's deadline and below Claude Code's timeout for the
  hook. It prints rlxd's reply only when it is the gate's allow or deny; anything else (no
  run or key on its side, no rlxd, a refusal, a dropped connection, a reply it can't
  read, its timer) is a deny.
- **Stdin stays open**: the prompt is a stream-json user message, and stdin closes only
  after the `result`. Each stream-json line becomes `run.started`, `run.text` (and
  partial chunks under `partial`), `run.tool_result` (content up to 1 MiB, then
  `truncated`), `run.rate_limited`, `run.done` or `run.failed {reason}`. Types and fields
  rlxd doesn't know are ignored. A line over 16 MiB fails the run (`line_too_long`)
  without rlxd holding it, and a result or structured output over 1 MiB fails it
  (`output_too_large`), so a run always gets its end.
- **The process tree.** On macOS and Linux Claude Code runs under a guard, the hidden
  `rlxd __run-guard`, which leads a new session and process group (ids: its pid, which
  stays held while it is unreaped); Claude Code and what it starts run in that group,
  with no terminal. The guard kills the group when Claude Code exits (what it left
  running goes with it), and when rlxd dies (it checks its parent every 100 ms; on Linux
  its parent-death signal too, and Claude Code's from the guard). rlxd kills the group
  on a cancel or a timeout, and as soon as it sees the guard exit, before it reaps it,
  so a guard killed alone leaves nothing. The guard tells rlxd how Claude Code ended
  (`exit 1`, `signal 6`) over an inherited pipe, for `run.failed`'s message, and never
  re-raises the signal. On Windows Claude Code is started suspended inside a Job Object
  with `KILL_ON_JOB_CLOSE`. A clean stop ends every run in flight `failed {reason:
  "engine_restarted"}`.
- **The log** of each run is `<home>/rlxd/runs/<run_id>/`: `run.json`, rewritten whole and
  durably, and `events.jsonl`, each event as published. The runner numbers each run's
  events itself, under that run's own lock: an event that changes the run's state
  (`run.started`, `run.needs_you`, each end) is written into `run.json`, after the log so
  far is fsynced, before it is published; any other event is published, then appended,
  and fsynced with the run's next state event or every 64. No fsync happens under the
  bus's lock or the runner's, so a slow disk under one run holds up no other run and no
  other topic.
- **The lifecycle** (5.4). `run.get {run_id}` and `run.list {states?, job_id?, limit?}`
  read the runs rlxd knows, those from before a restart too: rlxd reads every run's record
  when it starts. `run.events {run_id, from_run_seq}` replays a run's log from disk,
  never from the bus's buffer, then follows a live run until it ends; `rlxd call` prints
  each event as a line and exits 0 when the stream closes.
- **Needs-you and resume.** A finished run whose output matches `needs_you_when` ends
  `run.needs_you {reason: "question", question}`, after `needs_you.raised` on the
  `needs_you` topic. `run.resume {run_id, reply?}` goes on with a run that has no live
  process as a new attempt (`--resume <run_id>`, its own settings file, the reply as its
  first message), and `needs_you.cleared` follows, after a restart too. It refuses
  `run_alive`, `not_resumable` (a `send_executor`) and `no_session`, also for a run that
  never reached `run.started` (cancelled while queued, failed before it started), which
  has no session to resume.
- **After a crash or a `kill -9`,** the guard has already ended the run's process group.
  Should the guard have died with rlxd (`killall -9 rlxd`), the next rlxd, in the same
  boot only, kills the guard's group if the guard still runs with the recorded pid and
  start time. Otherwise the group is the run's only when one of its processes, in the
  guard's session and started at or after the run, carries `RLXD_RUN_ID=<run_id>`, which
  every process of a run inherits (a group's id can't be taken while one of its
  processes lives); then each such process is killed. On macOS an Apple binary's
  environment is hidden, even from its own user, so a run whose leftovers are all
  Apple's (a Bash tool's `zsh`, `sleep` or `tail -f`, with Claude Code gone) can't be
  told from another group, and is left: the sweep never kills what it can't tell is the
  run's. On Linux Claude Code dies with the guard (its parent-death signal), and
  `/proc` shows the environment of each of the user's own processes. It cuts a torn last line from the run's log,
  puts back a state event its record holds that the log lost, and ends the run `failed
  {reason: "engine_restarted"}`, numbered on from its log.
- **`job_id`** is set by rlxd alone, for the scheduler (7.6). Test builds have
  `debug.run_start`, `run.start`'s params plus a `job_id`.
- **Spaces.** `space.ensure {name}` makes `<home>/spaces/<name>/` with an empty project
  `.claude/settings.json`, and returns its resolved path. It is not a project: no `.git`,
  not onboarded. Names are 1 to 64 lower-case letters, digits, `-`, `_` and `.`, with no
  separator, no `..` and no leading dot. `space.erase {name}` deletes the folder, the
  Claude Code session files of its runs, and rlxd's records of them. Session files are
  `projects/<encoded space dir>/<run_id>.jsonl` under `CLAUDE_CONFIG_DIR` and `~/.claude`
  (every account's config directory once the brain serves rlx's list), the space dir
  encoded as Claude Code does: no other project folder is touched.
  It refuses `runs_live` while a run in the space is queued, running, being started or
  being resumed, and `link_refused` (`detail.paths`) when a link, a junction or a mount
  point is below a root on the way: `spaces/`, the space itself, a config directory's
  `projects/`, or the space's folder there; nothing is removed then. `<home>` and each
  config directory are roots, taken as they resolve once (a `~/.claude` kept in dotfiles
  is fine), and the config directories are only gone into when the space has runs. Every folder on the way is
  opened once without following a link, and checked and removed through what was opened
  (`openat` and `unlinkat` on macOS and Linux; on Windows, handles that keep the folders
  from being renamed meanwhile), so a link put there after the check is never followed.
  A link inside the space goes as a link and is named in `links`, every one, under the
  space's resolved path. A mount point inside the space (another filesystem, or on Linux
  a bind mount of the same one) is never gone into, and a tree more than 256 folders
  deep is not removed past that: either fails the erase `erase_incomplete`, naming it.
  A second erase of a space being erased gets `erase_in_progress`. Paths it can't remove
  fail it `erase_incomplete` (`detail {removed, failed}`, each failure named under the
  space's resolved path), the runs concerned keep their records, and erasing again
  finishes.

## Connectors

The user's Claude Code connectors, the claude.ai MCP servers (handoff 0.10.3, 2.4, 5.3's
listing, 5.5 and 7.7): `crate::connectors`, and the gate in `crate::runner::connector_gate`.

- **The gate.** A run with `needs_connectors` makes up to 3 tries, each its own Claude
  Code, with backoffs of 2 s and 5 s, and starts no turn until each named connector is
  `connected`. On route 2, the default, a try holds stdin open for `connector_settle_s`
  (5 s, from the spawn plus half a second for Claude Code to come up), sends the prompt,
  and reads the `system/init` that follows; on a miss it is ended at once, and nothing
  it wrote reaches a client. Route 1 (capability `connectors.gate_before_turn`, which a
  release build doesn't announce until the probe finds it, and a test build with
  `RLXD_TEST_CONNECTOR_ROUTE=1` does) waits up to `connector_wait_s` (10 s) for the
  states before it sends the prompt. A `needs-auth` or `failed` connector stops the gate
  at once. When it gives up, the run ends `run.failed {reason: "connectors", connectors:
  [{id, status}]}`, after `needs_you.raised {reason: "auth"}` when a connector's sign-in
  lapsed. That needs-you is kept in the run's record, with the home the run's attempt
  ran on, and cleared once rlxd sees each connector it names `connected` again on that
  home's account (a login's end, or any later `system/init`), never another's, or by a
  resume or the space's erase. A run that never reached `run.started` has no
  session to resume (`no_session`): a new run is the way on.
- **What a missed try leaves.** Nothing of a try ended unconfirmed stays in the run's
  session: a miss, or any other end before its connectors are confirmed (a cancel, the
  timeout, rlxd stopping, a broken read, Claude Code's exit). On a first attempt (`--session-id`), the session file the try left is
  removed, so the next try starts it anew. On a resumed attempt (`--resume`), the file is
  the run's whole session: it is never removed, only cut back to its length before the
  attempt's first try, so a later resume goes on from what was delivered.
- **Accounts.** The map holds each Claude Code account rlx knows, by its slot (task #14,
  `crate::connector_map`). The brain says which there are (`accounts`, read-only: rlxd
  never writes the account registry or reads the Keychain): slot 1, the default account,
  the plain `claude`; and each managed slot, with its `CLAUDE_CONFIG_DIR` and
  `CLAUDE_CODE_TMPDIR`, and a mark of who is signed in there (a SHA-256 the brain makes of
  the account's id). rlxd keeps those homes and that mark in its own file, beside each
  account's connectors: the cache is keyed by each account's home, so a run's init is
  filed under the entry the brain gave for that home (its mark kept), and a new mark is a
  new sign-in, to read at once. The file is rlxd's alone (0600); the homes and the mark
  are never on the wire, in an event, or in rlxd's log. When the runs'
  `CLAUDE_CONFIG_DIR` names one (the app's when it set one, as runs get it, 4.3; else
  rlxd's own), or a run's init waits, rlxd asks the brain, at its start and after an
  `env.set` that leaves it unknown, so it knows which account each run used (with none
  named, the plain account). The ask runs on its own: an `env.set` made meanwhile is
  taken at once. Without a
  brain, there is the plain account alone; with the brain unavailable, the accounts stay
  as they were. A managed account is read only with both of its homes as rlx gave them:
  one whose `CLAUDE_CODE_TMPDIR` rlxd doesn't know (a restart with no brain to say it) is
  not read, `account_unknown`, rather than read with rlxd's own. An account gone from its
  list is dropped, with a `connector.removed {account, id}` for each of its connectors.
- **The cache.** Each `system/init` rlxd takes as a run's truth updates the account its
  own attempt ran on, as the brain gave it: by the `CLAUDE_CONFIG_DIR` that attempt
  started with (the app's then, when it set one, else rlxd's own), never the runs' home
  now, which an `env.set` may have changed since. That is the account whose
  `CLAUDE_CONFIG_DIR` it is, matched by its resolved path, or the plain account when it
  names none. Until rlx has said which accounts there are, rlxd holds the latest init
  with that home and files it by it then; one that is no account rlx knows is filed
  nowhere, never under a guess. That is an ungated run's (not `no_tools`, which lists none), a gated run's once
  confirmed, the last try's of one that failed, and a one-turn probe's. So does a
  `connectors.call` that finds its connector `needs-auth` or `failed`, and a login's
  end: each in the account of the home its own start read (the app's `CLAUDE_CONFIG_DIR`
  then, else rlxd's own), and nowhere when that is no account rlx knows. The runs'
  account now only says what a run, call or login started now runs on; it never files
  a result. A read that lists
  no server at all empties nothing, since a Claude Code that loaded none (E9's stdin at
  EOF) says the same. A server's `source` is its kind (`claude.ai`, or `mcp` for a local
  one), init's where it gives one (`claudeai` is a claude.ai connector), and its tools
  are those under `mcp__<name>__`, the name as Claude Code writes it (the tool-call
  gate's `claude_server_name`). Each connector has the `checked_at` of its last read, and
  an `identity`, `null` until rlxd reads one (below). The cache is kept in
  `<home>/rlxd/connectors.json` (0600), each account's entry by slot, written whole
  (`durable::write`) under the cache's lock before any change is published; a file from
  before accounts loads as the default account's. Each change is an event on the
  `connector` topic: `connector.added {account, id}`, `connector.removed {account, id}`,
  and `connector.changed {account, id, status, previous_status}` once for each real
  change.
- **`connectors.list {refresh?, allow_turn?, account?}`** gives the cache, to every
  role: `accounts`, each account's `{account, label, connectors, as_of, from, error}`
  (only `account`'s with one; a slot rlx doesn't know is `no_such_account`), and, at the
  top level as before, the plain account's (slot 1's) `connectors`, `as_of` and `from`.
  - With the capability `connectors.no_turn` (a test build with
    `RLXD_TEST_CONNECTORS_NO_TURN=1`; a release build only once the owner's probe,
    `rlxd/tests/probes/connector_map.sh`, confirms the route), `refresh` reads each account's connectors, or `account`'s, with
    no model turn (`runner/discover.rs`, spike #5's `mcp_status`), one Claude Code at a
    time: started on a control channel in `<home>/rlxd/connectors-read/<slot>/` (which
    must hold no Claude Code settings rlxd didn't write, as a `connectors.call`'s folder
    must not: else `untrusted_settings`), with no tools, no session and project settings
    only. It starts the `claude` the app's environment resolved to, on the account's
    environment: a run's (the app's values on top of rlxd's own), then a managed slot's
    `CLAUDE_CONFIG_DIR` and `CLAUDE_CODE_TMPDIR` from its record, or neither for the
    default account, even when the app's values carry `CLAUDE_CONFIG_DIR`. No account's
    read gets `CLAUDE_SECURESTORAGE_CONFIG_DIR`, the app's or rlxd's: it picks whose
    Keychain login Claude Code reads, which then keys on the account's own
    `CLAUDE_CONFIG_DIR` (the default item for the plain account). A managed account rlx
    gives its own (`secure_storage_dir`, once rlx pins one) is read with it. Runs keep the
    app's. It writes
    no user line and makes no call: `initialize`, then `mcp_status` every 250 ms until it
    lists a server and none is `pending`, and `model_turns: 0`, `from: "listing"`. An
    account that hasn't settled in 15 s keeps what was read before, with `error:
    {code: "settle_timeout"}`, and no event; one whose Claude Code refuses `mcp_status`,
    `no_route`. The refresh fails only when no account was read. A refresh of an
    account being read joins that read.
  - Without the capability, `refresh` is `no_route`, unless `allow_turn`, which `app`,
    `cli` and `tui` may pass (`remote` gets `role_not_allowed`): then rlxd runs its own
    one-turn probe of one account, `account` or the default one, never more: not a run,
    with the user's MCP servers loaded and every tool call it attempts denied by rlxd
    itself (its settings' hook is `rlxd __probe-deny`, which opens no connection), and
    reports `model_turns: 1`. Like a read, the probe starts the `claude` the app's
    environment resolved to, on the account's environment, and is refused
    `claude_not_found` or `claude_not_runnable` as `run.start` is. rlxd's log says when
    each probe and each read ran, of which slot, and holds neither a probe's prompt nor
    what either listed.
  - **Identity** (task #14, step 2; `crate::identity`): after an explicit refresh's reads
    by an `app`, `cli` or `tui` client (a `remote` refresh reads the statuses alone, as
    `connectors.call` refuses `remote`), and only with the capability `connectors.call`
    on and an app connected now, rlxd asks
    who each account is on each connector its identity table names: `{workspace?, user?,
    email?}`. One call per account and connector, at most, of the table's read tool with
    its fixed input, and only when that account's own listing holds the connector
    `connected` and the tool with `read_only_hint: true`. Each is a `connectors.call` run
    on that account's environment (as a read's: the app's `claude` and values, then the
    account's homes, and no Keychain login dir but its own), a gated `watch` run in
    `run.list` and the ledger
    started by `rlxd-identity`, and the app decides it. Its result is the identity
    reader's alone: the run's `run.tool_result` carries no content (`null`), so no event
    and nothing in the run's folder holds it; only the identity read from it, in the cache
    and `connectors.list`'s reply. A deny, a failure or an answer that says nothing
    changes nothing, and an identity read raises no needs-you: a connector it finds
    signed out is filed so under its account, and `connector.changed` tells the app; an identity stays as read until the account is signed in as someone
    else, and a new mark forgets it at once, before any read. Never from a tick, and
    never in rlxd's log. A release
    build's table is empty until the owner's probe (M4) names each connector's tool; a
    test build's holds the fixture's `claude.ai Fixture` `whoami`.
  - **In the background**, only with `connectors.no_turn` (no turn is ever spent unasked):
    every 10 minutes rlxd asks the brain for the accounts again and reads at once each
    new one and each signed in again (another mark); any other account is read again
    once its last read was tried over 24 h ago, whether that read succeeded or failed, so
    at most daily. A client that goes mid-read doesn't end the read: it runs on to its
    bound, and a later refresh of that account joins it.
- **`read_only_hint`** is each tool's MCP `readOnlyHint` when `system/init` lists the
  tool with annotations, else `null`: Claude Code 2.1 lists names only.
- **`connectors.login {id}`**, for `app`, `cli` and `tui` (`remote` gets
  `role_not_allowed`), runs `claude mcp login -- <id>` in an rlxd PTY, by Claude Code's
  absolute path, and returns `{login_id}`. An id that begins with `-` is refused
  (`invalid_params`). It reads the PTY's screen for the first URL:
  an `https://` one is `connector.login.open_url` then `connector.login.waiting`, and
  the login's exit 0 is `connector.login.done {status: "connected"}`; anything else is
  `connector.login.error {code}` (`not_https`, `no_url`, `failed`, `timed_out`,
  `claude_not_found`, `claude_not_runnable`). Its `claude` and environment are a run's:
  the app's (4.3), on top of rlxd's own, and its sign-in is filed in the account of the
  home it started on. No URL's query, where a login's secrets are, is
  ever in rlxd's log. The PTY is killed when the login ends.
- **`connectors.call {run_id, connector, tool, input}`** (5.3 phase 2; spike #5, route a)
  makes exactly one call of the connector's tool through Claude Code's own MCP client,
  with no model turn, for `app`, `cli` and `tui` (`remote` gets `role_not_allowed`). It
  needs the capability `connectors.call`, which a release build doesn't announce until
  the owner-run probe confirms the route (test builds: `RLXD_TEST_CONNECTORS_CALL=1`);
  without it the call is `no_route`. The call is a run: kind `watch`, mounting the one
  connector, `pinned_call` its tool and its input's RFC 8785 SHA-256, never resumed
  (`not_resumable`). Its Claude Code is started on a control channel (`--permission-mode
  manual --permission-prompts host --permission-prompt-tool stdio`, no `--settings`, no
  session), the `claude` and environment a run's (4.3), and is never written a user line: `initialize`, whose capabilities must list
  `mcp_call_check_permissions_v1` (else the run fails `error`, naming it, and the call is
  `no_route`: an older Claude Code would run it unchecked); `mcp_status`, polled until the
  connector is `connected`, whose tools (named as Claude Code names them) and
  `readOnly: true` hints are what the gate's rules decide by. **rlxd's rules run before
  anything is sent:** the mount and the tool's one claude.ai server, kind `watch` (so the
  tool must be read-only) and the pinned call; one that denies is a deny `by: "rlxd"` in
  the ledger (`call_id: "rlxd_mcp_call"`) and the call is never sent, so nothing that might
  let Claude Code skip its ask can run a tool rlxd's rules deny. A tool the connectors'
  cache knows is not read-only is denied so before Claude Code starts. Then one
  `mcp_call`, always with `check_permissions: true`. Claude Code's `can_use_tool` takes
  the gate's own path (rlxd's rules again, then the app, then the ledger), and an allow
  answers with the pinned input as `updatedInput`, never the request's. A second
  `can_use_tool` (one rlxd can't read counts as the first) is a second call, which a
  `connectors.call` never makes: it is denied `by: "rlxd"` at once (never the app's), the
  tree is killed, and the call is `call_failed`, the first call's content dropped. **No settings but rlxd's:** Claude Code
  decides a `check_permissions` call by its settings' rules and hooks before it asks
  `can_use_tool`, and a rule that allowed the call would skip every rule of rlxd's. So it
  loads no user or local settings (`--setting-sources project`) and runs not in the space
  but in an empty, private folder of the run's own (`runs/<run_id>/call/`), which rlxd
  writes nothing into; before Claude Code starts and again before the call is sent, rlxd
  checks it holds no `.claude/settings.json` or `.claude/settings.local.json`, failing
  closed (anything there, or an error reading it, fails the run with no call made:
  `untrusted_settings`, `detail.path`). The checks are of a moment; an organisation's
  managed settings, which no flag turns off, are its own control. The run
  emits `run.started`,
  `run.tool_call`, `run.tool_decided` and `run.tool_result`, and ends `run.done
  {num_turns: 0}`, or `run.failed {reason: "denied"}` on a deny. The reply is `{run_id,
  decision, content?, is_error?, model_turns: 0}`; a deny is `denied` (`detail.by`,
  `detail.reason`, `detail.call_id`); any other end is `call_failed` (`detail.reason`,
  the run's). A call that comes back with no `can_use_tool` first (or after a deny) fails
  all the same, `call_failed` (never `denied`: it ran): its content is dropped, the ledger keeps it (`by: "rlxd"`, `unknown`, or
  `error`), and rlxd's log names what can skip the ask. Every read is bounded: a 4 MiB
  line, a wait for `initialize`'s reply (15 s), the connector (30 s), the `can_use_tool`
  (30 s, and at least the app's `deadline_ms` and 5 s), and the result (60 s from the
  moment any answer, allow or deny, is given), and the run's `timeout_s` (their sum and
  `deadline_ms`); any of them, or a model turn's line, kills the tree, and after a kill
  Claude Code's stderr is read for 5 s at most (a process that left the tree may hold
  it). A deny, however
  long the app took or by `timeout`, ends `run.failed {reason: "denied"}`.
- **The probe.** `rlxd/tests/probes/connectors_route.sh` is the owner-run check of what
  only the real Claude Code can say: the route, whether `system/init` carries
  annotations, `claude mcp login`'s output, and the handoff's `[real]` lines.
## The tool-call gate

Every tool call of every run is decided by rlxd (handoff 0.10.3, 5.1), the
`crate::runner::gate` module, and it fails closed.

- **A hook request is bound to its run.** It must name the run's running attempt, carry
  that attempt's key, and come from a process inside the run. The key is 256 random bits
  made at each attempt's start (an attempt with no randomness doesn't start) and given to
  Claude Code as `RLXD_HOOK_KEY`, so its hooks inherit it; it is compared in constant
  time, and never logged, published or written (not in `run.json`, the run's log, its
  settings, or rlxd's log). Any other request is denied at once, with no event and no
  ledger entry.
- **Inside the run** is by ancestry, not process group: Claude Code starts each hook
  detached, in a session and process group of its own. On macOS and Linux the asking
  process must descend from the run's guard by its parent chain, each step up checked
  to be the real parent (a parent never started after its child, so a pid given to
  another process since fails), and the asking process itself is the one pinned when
  the connection was accepted: Linux holds its pidfd across the check, macOS requires
  the process at its pid to have started before the connection was accepted. macOS has
  no public handle on a process, so its pin is a pid and a start time, with two windows
  left open: a peer that exits between its `connect` and rlxd's `accept`, its pid reused
  in that instant, and a process started after the wall clock was stepped back past the
  accept. Neither is in reach: macOS hands out pids in order, and a pid that passes must
  also be inside the run, whose processes hold the key already. On Windows it must be in
  the run's Job Object, checked through the process handle pinned when the pipe
  connected. fake-claude starts its hooks detached the same way.
- **What the key does not stop.** Every process Claude Code starts inherits the key, not
  only its hooks, so a process inside a run (a tool's shell, say) can send a request for
  its own run. It only ever gets the app's answer to its own request: each call is its
  `tool_use_id`, decided once, so a forged request can't get an allow for Claude Code's
  real call (a real call whose id was decided already is denied), and it can't reach
  another run, whose key it doesn't have and whose guard (or job) it isn't under.
- **rlxd's own rules come first**, before the app is looked for, and deny `by: "rlxd"`:
  the mount (`no_tools`, or a tool of a connector the mount doesn't name), the kind
  (`check`, `parse_instruction` and `widget_author` make no calls; `widget_refresh` and
  `watch` make read calls only), and the pinned call (`send_executor` and `watch`: the pinned tool with the pinned
  `input_sha256` only, and once: then every call is denied `pinned_call_used`, which
  `run.json` keeps across attempts and restarts).
- **A connectors run's calls are to tools its init listed.** A call reaches the app only
  when its tool's exact name is in the attempt's `system/init` tools, and the tool is
  one mounted server's; otherwise it is denied `by: "rlxd"`, a tool the init didn't list
  with the reason `tool_not_in_init`. A connector still connecting when the init is
  written has no tools in it, so its calls are denied: that is what 5.5's
  `needs_connectors` is for, since a run that names a connector there starts its first
  turn only once it is connected, so its tools are in the init. A tool's server is the
  one MCP server of the attempt's `system/init` whose name, as Claude Code writes it in
  tool names (each UTF-16 unit but a letter, a digit, `_` or `-` as `_`; for a
  `claude.ai ` connector, runs of `_` as one and none at the ends), begins the tool's
  name before a `__`. When none does, or more than one does (a local server named
  `claude_ai_Slack` or `claude_ai_Slack__x` beside the `claude.ai Slack` connector), the
  call is denied. A mount's `claude.ai ` connector is only a server whose init `source`
  is `claudeai`: a local, user, project or plugin server named exactly `claude.ai Slack`
  never passes as it.
- **Read-only.** A tool is read-only only when its `system/init` entry says `annotations.readOnlyHint: true`; a hint that isn't there
  (`null`) counts as a write, in one function, `read_only`, until the hub decides
  otherwise (#2718).
- **Every other call goes to the app** as `run.tool_call {call_id, tool, input,
  input_sha256, deadline_ms}`, `input_sha256` being the SHA-256 of the input as RFC 8785
  canonical JSON. Only `run.decide {run_id, call_id, decision, reason, draft_id?, stop?}`
  from an app-role connection holding the gate answers it (any other gets `not_gate`);
  the first answer wins, a later one gets `already_decided`, one after the deadline
  `too_late`, one for a call the run never made `unknown_call`. `allow` lets the tool run;
  `deny` and `draft` are a deny to Claude Code, with the app's reason. `stop: true` on a
  deny also ends the run `run.needs_you {reason: "denied", call_id}`, which `run.resume`
  goes on with as a new attempt, gated as the first was. Each decision is published as
  `run.tool_decided {call_id, decision, by, reason}`, and `run.decide`'s reply says the
  decision taken (rlxd's deny, when it overrode an allow). Once the app stops a run, its
  other calls, waiting or to come, are denied by rlxd. Nothing a run writes (its text, a
  tool's result) decides a call.
- **No app, a lost app.** With no app connected, a call is denied `by: "unreachable"` at
  once, a read too. A call pending when the app leaves is held until its deadline, for
  the app to decide when it reconnects; then it is denied `by: "timeout"`. The hook's
  timeout in the run's settings is the deadline plus 10 s, so rlxd always answers first.
  A hook connection that ends before its call is decided (the hook killed, rlxd
  stopping) denies it `by: "rlxd"`, and a hook that can't reach rlxd denies.
- **The ledger**, `run.json`'s `calls` and `run.get`'s, holds each call: `decided`, or
  `executing` for an allow, written and fsynced before the hook gets its answer, and an
  allow whose entry can't be written is a deny `by: "rlxd"` instead (a pinned call stays
  used); `ok` or `error` from the call's `tool_result`; `unknown` when the run ends, or
  rlxd restarts, with it still executing (it may have sent). A call still waiting as its
  run ends is denied `by: "rlxd"` first. The gate's writes of `run.json` keep the run's
  last state event in it, as every record write does.
- **Out of the gate:** PTYs (`pty.*`) are the user's own terminals, and are not gated.
- **Test builds** have `debug.pre_tool_use {run_id, attempt, key, stdin}`, a hook request
  from the connection's own process, and `debug.drop_hook_connections`, which ends every
  hook connection waiting on the gate. With `RLXD_TEST_HOOK` set, each run's settings name
  rlxd itself as the hook (the hidden `rlxd __test-hook pre-tool-use`, which asks
  `debug.pre_tool_use`); without it, they name the real rlxd-hook beside rlxd
  (`tests/hook_gate.rs`).
  `RLXD_TEST_FAIL_LEDGER=<space>` fails the ledger writes of that space's runs. A
  release build has none of these, nor their names.

## The scheduler

rlxapp's jobs (handoff 0.10.3, 2.3 and 7.6). The app owns their definitions and pushes
them by `job_id`; rlxd keeps a definition only to fire it, and records what each fire did.

- **Methods.** `job.put` creates or replaces a job, idempotently: `{job_id, type: "run",
  schedule, run, catch_up, enabled?, loud?}`. `schedule` is `{"every_minutes": N}` or
  `{"cron": "*/15 * * * *", "tz": "Europe/London"}`. `run` is a `run.start` template
  without `run_id`, checked as `run.start` checks its params, so a template it would
  refuse is refused at `job.put`, with the same code. The job's own unknown fields are
  refused; the template's follow `run.start`'s rule, which ignores them. The same
  definition put again (the app pushes its jobs on each connect) changes nothing. A
  replace starts the schedule again from now only when its `schedule` changes or it is
  enabled again; otherwise it keeps where the schedule is, a fire it owes, and a missed
  fire waiting for an app. A replace keeps the job's record and whether it is paused. `job.list`, `job.delete`, `job.pause`, `job.resume`, and `job.run_now`, which
  fires the job now. `job.run_now` needs an app gate client (`app_absent` otherwise);
  `paused` doesn't stop it, a live run of the job (`job_running`) or its rate limit
  (`rate_limited`, `detail.resets_at`) do. `job.list`'s `last_state` is the job's last run's state as the runner has it now.
- **When.** `every_minutes` fires on a grid from `job.put`, so a late fire never drifts
  it. Cron matches civil minutes in its IANA zone (jiff's bundled tz database, the same
  on every OS), each once: a time a DST gap skips fires once at the gap's end, and a time
  a fold repeats fires once, at its first occurrence.
- **Whether.** One function decides each fire, in this order: a paused job's due fire is
  skipped `paused`; with no app gate client connected, `app_absent` (2.3); while the
  job's last run is queued or running, `overlap`; while a run of the job hit its rate
  limit (a `run.rate_limited` whose `status` isn't `allowed` or `allowed_warning`, until
  its `resets_at`, else 5 minutes; a warning holds nothing), `rate_limited`. A fire
  rlxd couldn't make on time (it was down, or the clock jumped) is one decision however
  many it passed: `catch_up: "once"` fires once, `"skip"` records `missed`. A fire missed
  while rlxd was down waits 10 s after rlxd starts for an app to connect, then is
  `app_absent`. Each `app_absent` skip is owed: when an app connects, `once` fires it
  once, and `skip` waits for the next time.
- **A fire** is a run of the template with a fresh `run_id`, the job's `job_id` on all
  its events, and the job's `loud`, which rlxd only echoes, on `job.fired` and on the
  run's `run.done` and `run.needs_you`. Events on topic `job`: `job.put`, `job.deleted`,
  `job.paused`, `job.resumed`, `job.fired {job_id, run_id, scheduled_for, fired_at,
  cause, loud}`, `job.skipped {job_id, scheduled_for, reason, code?}` and
  `job.watch_result`. `job.fired`
  goes out once the run has started, so its first run events may come before it. A run
  that can't start is `job.skipped {reason: "start_failed", code}`, `code` being
  `run.start`'s refusal.
- **Watch jobs** (`"type": "watch"`, with `connector`, `tool` and an object `input`) make
  one pinned call per tick, with no prompt of the job's; the tool must read as one of the
  connector's (`mcp__<connector as Claude Code writes it>__…`), else `invalid_params` at
  `job.put`. Every fire is a `watch` run with an rlxd-picked `run_id`, gated like any run
  (5.1): an app must be connected (else `app_absent`, as for any job) and must allow the
  call. With the capability `connectors.call`, each fire is a `connectors.call` run (the
  job's one call with no model turn, its `job_id` on its events, its allowed result
  `job.watch_result` as below); when that run finds no route (a Claude Code without
  `mcp_call_check_permissions_v1`), the scheduler makes the same fire again (the same
  `scheduled_for`, a second `job.fired`) by the fallback. Without the capability, the run
  is the fallback: Claude Code with rlxd's fixed prompt, "Call this
  tool once with exactly this input; do nothing else", as its appended system prompt (so
  its command line carries it) and as its message, which quotes the connector, the tool
  and the input as canonical JSON in a fence, as data, and nothing of the job's as text.
  Its mount is the connector, and its `pinned_call` is the tool and the input's RFC 8785
  SHA-256, so a connector's text can't steer the call, and a second call, even the
  identical one, is denied `pinned_call_used`. The allowed call's raw result is
  `job.watch_result {job_id, run_id, tool, input_sha256, content, is_error, truncated}`
  (`content` as `run.tool_result` carries it, up to 1 MiB): the app evaluates `match`
  itself, never on an error or a cut result. A denied call gives none, and so does a run
  job, even one whose template is a pinned `watch` run.
- **Once.** Before a fire's run starts, the job (its schedule moved past the fire) and a
  `starting` record of the fire, with its `run_id`, are written in one transaction. So
  a crash while the run starts never fires that time again: the next rlxd settles the
  fire as `job.fired` when the run was made, or `job.skipped start_failed
  (engine_restarted)` when it wasn't.
- **Storage.** `<home>/rlxd/rlxd.db`, SQLite (built in): one writer, WAL, `synchronous =
  FULL`, migrations numbered in `schema_migrations`. The folder is 0700 and the database,
  its `-wal` and its `-shm` 0600 on macOS and Linux (the templates hold prompts). One that
  can't be read (corrupt, or from a newer rlxd) is moved to `rlxd.db.unreadable-<time>`,
  logged, and reported in `status` (`jobs.db: "set_aside"`, `jobs.set_aside`), and a fresh
  one begun; it is never reset in place. Another failure to open it (busy, locked, no
  access) is tried three times, then the scheduler doesn't start (`jobs.db:
  "unavailable"`, and why) and the file is left as it is. Each job keeps its last 1000
  fire records.
- **Tests.** Test builds have a test clock: `RLXD_TEST_CLOCK=<RFC 3339>` starts the
  scheduler there, and `debug.clock_advance {seconds}` and `debug.clock_set {at}` move it,
  answering once every decision due by then is made (`tests/scheduler.rs`). A release
  build has neither. 7.6's Accept lines run on real one-minute schedules in
  `tests/scheduler_accept.rs`, `#[ignore]`d in the test job and run by rlxd.yml's
  `scheduler accept` job: `cargo test -p rlxd --test scheduler_accept -- --ignored`.

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
  failed`. Run `rlxd here --start` first, and again when a connection closes. An sshd
  that refuses socket forwarding closes the channel the same way. So when a connection
  through `--socket` closes without a whole reply to `hello`, `rlxd call` and `rlxd
  events` say `disconnected` (exit 1), with a hint naming both likely causes: the host's
  rlxd isn't running, or its sshd doesn't allow socket forwarding. A close after
  `hello`'s reply is rlxd's own, and a plain `disconnected` with no such hint. When
  nothing listens at `--socket` at all, as when the forward is down, it is
  `not_running` (exit 3), with a hint to check the forward.
- **A missing token.** With `--socket` or `--token-file`, a token file that can't be
  read is `unauthorized` (exit 1), from `rlxd call` and `rlxd events` alike, whatever
  runs in this home: each reads the token before it connects. Only the home's own token
  file, missing where no rlxd runs, reads as `not_running`.
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

Parts of `crates/rlxd` are ported from tmux 3.8 and stay under tmux's ISC licence, so
the crate's licence is `MIT AND ISC`:

- `src/vt/` (its grid, screen writes, input parser, UTF-8 handling, hyperlinks and
  `capture-pane`'s loop: grid.c, grid-view.c, screen.c, screen-write.c, input.c,
  utf8.c, utf8-combined.c, hyperlinks.c and cmd-capture-pane.c);
- `src/pty/keys.rs` (key names, the bytes for keys, and `paste-buffer -p`:
  key-string.c, input-keys.c, cmd-send-keys.c and cmd-paste-buffer.c).

Each of those files starts with the copyright lines and the permission notice of the
tmux files it comes from.

Every rlxd binary carries those notices too: `THIRD_PARTY_NOTICES` here holds tmux's
copyright and permission notice for each file ported from it, rlxd embeds it when it is
built, and `rlxd licenses` prints it. The release archives hold just `rlxd` and
`rlxd-hook`, so the notice travels inside the binary.
