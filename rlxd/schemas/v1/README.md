# rlxd schemas, version 1

> **v1 is a draft until rlx 0.10.3 ships.** Until that release, v1 can still change in
> ways the versioning rules below would otherwise forbid, as the handoff settles. From
> 0.10.3 on, v1 changes only additively.

These are JSON Schemas (draft 2020-12) for rlxd's wire format, following handoff 0.10.3
rev 6: 7.2, 7.3, 7.4, 5.1 and 4.1. They are generated from the types in
`rlxd/crates/rlxd-proto`.

**Don't edit them by hand.** Change the types, then regenerate and commit:

```sh
cd rlxd
cargo run -p rlxd-proto --bin gen-schemas            # regenerate
cargo run -p rlxd-proto --bin gen-schemas -- --check # what CI runs: exit 1 on drift
```

The types hold every document to the same rules as its schema. A document the schema
refuses also fails to parse, with an error naming the field. The types are stricter in
two places JSON Schema can't reach:

- A float with a zero fraction, such as `1.0` where an integer belongs, passes the
  schema and fails to parse.
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
| `settings-changed.json` | `settings.changed`'s data, on the `settings` topic: `{key, version, by_device?}` |

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
(the owner's multi-device brief, §4). Device-local state stays on the device: SSH
profiles, the window's size and position, per-device notifications, caches.

- **The keys.** rlxd knows four, each with a shape: `theme` (a name of 1 to 64
  characters), `layout` (an object), `agent_sign_off` (a string of at most 512
  characters) and `send_safety_mode`. Any dotted key, `<namespace>.<name>`, holds any JSON
  value but `null`, up to 64 KiB. Another key without a dot gets `unknown_setting`.
- **`send_safety_mode`** is `{default, connectors?}`. Each mode is `draft`,
  `standing_approval` or `auto`, safest first, and `connectors` maps a connector's name to
  its mode. A key never set means `draft`. Only an app-role connection, a paired device,
  may set it; any other gets `not_gate`. Every change is logged, and its event names the
  device.
- **Versions.** Each key counts its writes from 1. `settings.set` with `expected_version`
  writes only when that is the version now, else it gets `conflict` with
  `detail.version`; read again and write from that. Without it, the last write wins.
- **Events.** Every write, once on disk, is a `settings.changed {key, version,
  by_device?}` on the `settings` topic, one key's in version order. Read the value with
  `settings.get`.

## Versioning

`schema_version` is an integer, and every document here carries `1`. A document with
any other `schema_version` fails to parse rather than misparse.

- New fields and new event types keep the version. Clients ignore what they don't
  know, so no schema here forbids extra properties.
- Removing or renaming a field raises `protocol_version` by one. The new schemas go in
  `schemas/v2/`, and rlxd keeps serving v1 for one rlx minor release.
