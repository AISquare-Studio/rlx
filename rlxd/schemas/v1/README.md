# rlxd schemas, version 1

> **v1 is a draft until rlx 0.10.3 ships.** Until that release, v1 can still change in
> ways the versioning rules below would otherwise forbid, as the handoff settles. From
> 0.10.3 on, v1 changes only additively.

These are JSON Schemas (draft 2020-12) for rlxd's wire format, following handoff 0.10.3
rev 7: 7.2, 7.3, 7.4, 5.1, 4.1 and 1.4. They are generated from the types in
`rlxd/crates/rlxd-proto`.

**Don't edit them by hand.** Change the types, then regenerate and commit:

```sh
cd rlxd
cargo run -p rlxd-proto --bin gen-schemas            # regenerate
cargo run -p rlxd-proto --bin gen-schemas -- --check # what CI runs: exit 1 on drift
```

The types hold every document to the same rules as its schema. A document the schema
refuses also fails to parse, with an error naming the field. The types are stricter in
three places JSON Schema can't reach:

- A float with a zero fraction, such as `1.0` where an integer belongs, passes the
  schema and fails to parse.
- A `pty.write`'s 1 MiB of `text`, `paste` or key names is counted in bytes. JSON Schema
  counts a string's length in characters, so only the types check it.
- An event's `type` must start with its `topic`. The schema can't compare one field
  with another, so it checks that only for the topics v1 lists: 7.3's, plus 7.8's
  `install` and `signin`. The types check it for every topic.

Every unsigned integer carries its Rust type's `maximum`, so the two agree on range
too. A `time` with second `60` is refused: RFC 3339 allows a leap second, but
check-jsonschema refuses every one and the jsonschema crate accepts some, so `59` is the
bound both validators agree on.

| File | What it describes |
|---|---|
| `daemon-record.json` | One record in `<home>/daemons/` (4.1): `{id, kind, pid, host, version, started_at, heartbeat_at, argv, restartable, cwd?}`, which Python's registry and rlxd share. `pid` is 1 to 2^31 - 1, Python's `core.registry.PID_MAX` |
| `envelope.json` | Every response: `{schema_version, ok: true, data}` or `{schema_version, ok: false, error{code, message, hint?, detail?}}`, never both, plus `id` on the socket. `detail` is a string or an object. `incompatible_protocol` must carry `detail.supported`, and `since_ahead` `detail.head`. `ok: true` means exit 0, except `rlxd here`'s exits 3 and 4 |
| `event.json` | One event: `{schema_version, seq, time, topic, type, data}`. `topic` is snake_case. `type` is `<topic>.<event>`, the event one or more snake_case names joined by `.` (`run.text`, `connector.login.open_url`). On the `run` topic, `data` also carries `run_id`, `run_seq`, `kind`, `attempt` and `job_id?` (7.5) |
| `event-list.json` | A JSON array of events, as `jq -s .` makes of `rlxd events`' lines |
| `events-subscribe.json` | `events.subscribe`'s params: `{topics?, since?, follow?}`. `topics` is a non-empty list, absent for every topic; `since` replays the events after that seq first; `follow` (default `true`) goes on with live events |
| `request.json` | One socket request frame: `{id, method, params?}`. `id` is at most 2^53 - 1 |
| `hello.json` | `hello`'s params: `{token, protocol_version, client, client_version, role, gate_credential?, capabilities, env?}`, where `role` is `app`, `tui`, `remote` or `cli` |
| `hello-reply.json` | `hello`'s reply data: `{protocol_version, protocol_versions, rlxd_version, capabilities}` |
| `here-reply.json` | `rlxd here`'s data, the probe rlx.app runs over SSH: `{installed: true, running, started, supported, version?, protocol_versions, pid?, socket, home, platform, auth{token_file, token?}, rlxd{version, path, protocol_versions}, rlx{installed, version?, path?}}` |
| `status-reply.json` | `status`'s reply data (7.4): `{version, pid, platform, protocol_version, capabilities, seq, clients, gate, brain, claude, runs, jobs, paths, pipe_acl?}`. `clients` counts open connections by `hello` role, and `pipe_acl` is Windows' |
| `settings-get.json` | `settings.get`'s params: `{key}`. A key is a known one (`theme`, `layout`, `agent_sign_off`, `send_safety_mode`) or dotted snake_case names, such as `rlxapp.sidebar` |
| `settings-set.json` | `settings.set`'s params: `{key, value, expected_version?}`. A known key's value has that key's shape; a dotted key's is any JSON but `null`. `expected_version` 0 means "only if never set" |
| `settings-list.json` | `settings.list`'s params: `{prefix?}` |
| `setting.json` | One setting, `settings.get`'s and `settings.set`'s reply: `{key, version, value?, updated_at?, by_device?}`. Version 0, with nothing else, is a key never set |
| `settings-list-reply.json` | `settings.list`'s reply: `{settings: [setting]}`, in key order |
| `settings-changed.json` | `settings.changed`'s and `settings.deleted`'s data, on the `settings` topic: `{key, version, by_device?}` |
| `settings-delete.json` | `settings.delete`'s params: `{key, expected_version?}`. The reply is the setting at version 0 |
| `pty-spawn.json` | `pty.spawn`'s params (1.4): `{argv, cols, rows, cwd?, env?, label?}`. `env` sets a variable, or removes it with `null`. Sizes are 1 to 1000 cells |
| `pty-ref.json` | One PTY, `{pty_id}`: the reply of `pty.spawn`, `pty.write`, `pty.resize` and `pty.kill`. A `pty_id` is 128 random bits, as 32 lowercase hex digits |
| `pty-write.json` | `pty.write`'s params: `{pty_id}` and exactly one of `text`, `keys` (tmux's key names) and `paste` (a bracketed paste) |
| `pty-resize.json` | `pty.resize`'s params: `{pty_id, cols, rows}` |
| `pty-screen.json` | `pty.screen`'s params: `{pty_id, scrollback?}`, at most 50,000 lines of history |
| `pty-screen-reply.json` | `pty.screen`'s reply, and a `pty.snapshot`'s data: `{pty_id, text, cursor{x, y, visible}, cols, rows, alternate, history, exited, status?, signal?, seq}`. `text` is byte for byte what `tmux capture-pane -e -p` prints. `status` comes exactly when `exited` |
| `pty-subscribe.json` | `pty.subscribe`'s params: `{pty_id, read_only}` |
| `pty-kill.json` | `pty.kill`'s params: `{pty_id}` |
| `pty-list-reply.json` | `pty.list`'s reply: `{ptys: [{pty_id, label?, argv, cwd, pid, cols, rows, started_at, exited, status?, signal?}]}`, in the order they started |
| `pty-output.json` | `pty.output`'s data, on the `pty` topic: `{pty_id, bytes_b64}`, the process's bytes in standard base64 |
| `pty-exited.json` | `pty.exited`'s data, on the `pty` topic: `{pty_id, status, signal?}` |

Validate a document with `uvx check-jsonschema --schemafile rlxd/schemas/v1/<schema> <file>`.

## The handshake

The first frame is `hello`, carrying the client's `protocol_version`. The reply names
the version the connection speaks, and every version rlxd serves. A version rlxd doesn't
serve gets the error `incompatible_protocol`, whose `detail.supported` lists the ones it
does, and never a silent misparse. The protocol version is the `schema_version` of every
envelope and event.

### The gate (5.1)

Only rlxapp may be the tool-call gate. The shared token lets a client in, but it doesn't
make the connection the gate.

- **The credential.** A `hello {role: "app"}` also carries `gate_credential`. `gate.pair`
  mints it once, for a peer that passes rlxd's identity check. Other roles have none to
  send.
- **The identity check.** Where the OS allows it, rlxd also checks the peer's identity.
  Gate dev mode is for unsigned dev and CI builds and is never the default. It relaxes
  only this check, so the credential is still required.
- **Refusals.** A `role: app` hello without a valid credential gets the error
  `not_gate`, and so does one from a peer that fails the check. A refused hello never
  counts as an app client. A tool-call decision from a connection that isn't the gate
  also gets `not_gate`.

## Two sequences

- `seq`, on every event: strictly increasing per home. `events.subscribe {since}`
  resumes after it.
- `data.run_seq`, on every `run`-topic event: strictly increasing per run, from 1, so one
  run can be replayed from a known point.

  It comes with the run's other fields: `run_id`, `kind`, and `attempt` (from 1, one up
  with each `run.resume`). `job_id` is added when a scheduler job fired the run.

## The stream

`events.subscribe` makes its connection carry the event stream: the events, then the
subscribe's reply `{seq}`, which ends it. A connection carries one stream, so event frames
carry no `id`: a frame with `ok` is a reply, any other an event. Replies to requests sent
meanwhile come between the events, with their `id`.

For a client:

- **There is no "subscribed" reply first.** To miss no event of something you start,
  take `since` from `status`'s `seq` before you start it. Or send the start on the same
  connection after `events.subscribe`.
- **Keep the reply's `seq`** as the `since` to resume from. It is the last seq the stream
  covered, whatever its topics.
- **`follow: false`** replays once, up to the last event published when rlxd took the
  subscribe.
- **A `daemon.gap {from, to}`** says the events numbered `from` to `to` are gone. Re-read
  what you show (`run.list`, `run.events`, `job.list`). It comes first when `since` is
  older than the oldest event rlxd holds. It can also come anywhere later, when a client
  reads so slowly that rlxd drops events it hasn't read. It comes whatever the topics.
  Its own `seq` is `to`, so seqs still strictly increase and resuming from it is right.
- **After a restart:**
  - resuming from the last reply's `seq` after a clean stop gives no gap;
  - from an older seq, or after a crash, expect one gap;
  - a crash's gap also covers up to 1,000 seqs that were never used.
- **`since_ahead`**, with `detail.head`, means `since` comes from another numbering of the
  home's events. Re-read what you show, then subscribe from `status`'s `seq`, or without
  `since`.
- **Other codes:** `unknown_topic`, and `invalid_params` for params of the wrong shape.
  A second `events.subscribe` on a streaming connection gets `invalid_request`.
  `rlxd events` adds `timed_out` (exit 4) and `stream_ended` on stderr.
- **A request sent mid-stream** is answered between the events. Until it is, that
  connection's events wait; a slow one, such as a `status` that asks Claude Code, delays
  them.
- **`run.events`** isn't served yet (`unknown_method`): the runner adds it. Catch up on
  a run older than the buffer through it, not `events.subscribe`.

`data` of the `daemon` events isn't checked by `event.json` yet.

## Settings

App settings live in the rlx home, so every device that connects sees the same values
(the owner's multi-device brief, §4). They are not for secrets: every client with the
token reads every one. Device-local state stays on the device: SSH profiles, the window's
size and position, per-device notifications, caches.

- **The keys.** rlxd knows four, each with a shape:
  - `theme`, a name of 1 to 64 characters;
  - `layout`, an object;
  - `agent_sign_off`, a string of at most 512 characters;
  - `send_safety_mode`.

  Any dotted key, `<namespace>.<name>`, holds any JSON value but `null`. Another key
  without a dot gets `unknown_setting`.
- **Limits.** Every value is at most 64 KiB of JSON and 32 arrays or objects deep; the
  schemas can't count depth, so only the types check it. A home holds at most 512 settings
  and 8 MiB of keys and values.
- **Protected keys:** `send_safety_mode` and every key under `policy.`, such as the
  app-absent policy or a connector's write policy.
  - Only an app-role connection holding the gate, a paired device, may set or delete
    them; any other gets `not_gate`. Every change is logged, and its event names the
    device.
  - Any client may change every other key, agents included.
- **`send_safety_mode`** is `{default, connectors?}`.
  - It is a policy, so it is strict for a writer: `settings.set` with a field rlxd
    doesn't know is refused, by `settings-set.json` and by rlxd, rather than ignored.
  - A reader takes such a field, which a later rlxd may add: `setting.json` and
    `settings-list-reply.json` allow one, so it never costs a client its
    `settings.list`.
  - Each mode is `draft`, `standing_approval` or `auto`, safest first, and `connectors`
    maps a connector's name to its mode.
  - A key never set means `draft`.
  - A change lowers send safety when any connector's mode, its own or else the default,
    becomes less safe.
- **Versions** come from one count for the whole home: each set takes the next number,
  so no two sets share one, and a key's version only rises.
  - `settings.set` and `settings.delete` with `expected_version` act only when that is
    the version now, else they get `conflict` with `detail.version`; read again and act
    from that.
  - Without `expected_version`, the last change wins.
  - A deleted key is back to never set, at version 0. Set again, it goes on from the
    home's count, so a write made from its version before the delete gets `conflict`.
- **Events.** Every change, once on disk, is a `settings.changed {key, version,
  by_device?}`, or a `settings.deleted` with the version deleted, on the `settings`
  topic. One key's events come in version order. Read the new value with `settings.get`.

## PTYs

rlxd holds processes on pseudo-terminals (handoff 1.4), as tmux held the fleet's panes,
with ConPTY on Windows. A PTY outlives every client, and one any connection spawned is in
every connection's `pty.list`.

- **The screen is tmux's.** `pty.screen`'s `text` is what `tmux capture-pane -e -p`
  prints for the same screen, byte for byte, so a renderer that reads tmux's captures
  reads it: SGR sequences in tmux's forms and order, trailing spaces dropped, and a
  newline after every row. `scrollback: n` puts up to `n` lines of history above it, as
  `-S -n` does.
- **Input is tmux's.** `keys` are tmux's key names, sent as `send-keys` sends them,
  honouring the modes the program set; a name tmux doesn't know is typed as its
  characters, as `send-keys` types it. `paste` is `paste-buffer -p`'s: newlines as
  carriage returns, bracketed when the program asked for bracketed paste.
- **A process that ends** keeps its last screen and exit status, `exited` and `status`,
  until `pty.kill`, as tmux's `remain-on-exit on` keeps a pane. `status` is the exit
  code, or 128 plus the signal that ended it, with `signal` too. `pty.write` and
  `pty.resize` to it get `pty_exited`.
- **`pty.kill`** ends the process and every process in its group (on Windows, its job),
  and frees the PTY. A process that left the group itself is not ended, as under tmux.
- **The stream.** `pty.subscribe {pty_id, read_only}` first sends a `pty.snapshot`, the
  screen as `pty-screen-reply.json` describes it, whose `seq` the stream goes on from.
  Then come the PTY's `pty.output` and `pty.exited` events. A client too slow to keep up
  never holds the PTY back: what it missed is dropped, and it gets a new `pty.snapshot`.
  The reply, `{seq}`, ends the stream once the PTY is killed or rlxd stops.
- **Output is kept apart.** rlxd keeps `pty.output` events in a budget of their own, so
  a busy PTY never pushes another topic's events out. A stream that asked for `pty` gets
  a `daemon.gap` when output it hadn't read was dropped; a stream that didn't ask for it
  never gets a gap on its account. `pty.exited` is kept with the other events.
- **Who may do what.**

  | Role | `pty.list`, `pty.screen`, `pty.subscribe {read_only: true}` | `pty.write` | `pty.spawn`, `pty.resize`, `pty.kill`, `pty.subscribe {read_only: false}` |
  |---|---|---|---|
  | `cli`, `tui` | yes | yes | yes |
  | `remote` | yes | yes | no |
  | `app` | yes | no | no |

  Any other call gets `role_not_allowed`, with `detail.role` and `detail.method`. A
  connection subscribed to a PTY read-only gets `read_only` for its `pty.write` to it.
  `pty.spawn` never waits on the app gate, and a PTY keeps working with the app quit.
- **Limits.** A home holds at most 64 PTYs, the exited ones included, else
  `too_many_ptys`. Each keeps at most 50,000 lines of history. A `pty.write` carries at
  most 1 MiB.
- **The terminal** a process sees is `TERM=xterm-256color` with `COLORTERM=truecolor`,
  unless `env` says otherwise. The fleet's tmux sets `tmux-256color`, which macOS's
  terminfo lacks.

## Versioning

`schema_version` is an integer, and every document here carries `1`. A document with
any other `schema_version` fails to parse rather than misparse.

- New fields and new event types keep the version. Clients ignore what they don't
  know, so no schema a client reads forbids extra properties.
- The one schema that does is what a writer sends: `settings-set.json`'s
  `send_safety_mode` value takes no field but `default` and `connectors`, as rlxd
  does, since a policy is taken whole. A new field there is not additive for writers,
  because an older rlxd refuses it, so it raises `protocol_version` by one. Readers
  still take it in the meantime (`setting.json`, `settings-list-reply.json`).
- Removing or renaming a field raises `protocol_version` by one. The new schemas go in
  `schemas/v2/`, and rlxd keeps serving v1 for one rlx minor release.
