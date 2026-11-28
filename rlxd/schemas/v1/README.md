# rlxd schemas, version 1

> **v1 is a draft until rlx 0.10.3 ships.** Until that release, v1 can still change in
> ways the versioning rules below would otherwise forbid, as the handoff settles. From
> 0.10.3 on, v1 changes only additively.

These are JSON Schemas (draft 2020-12) for rlxd's wire format, following handoff 0.10.3
rev 6 and 7: 7.2, 7.3, 7.4, 7.5, 5.1, 2.5, 4.1 and 1.4. They are generated from the types in
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
- A `pty.write`'s 128 KiB of `text`, `paste` or key names is counted in bytes. JSON Schema
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
| `pty-screen-reply.json` | `pty.screen`'s reply, and a `pty.snapshot`'s data: `{pty_id, text, cursor{x, y, visible}, cols, rows, alternate, history, exited, status?, signal?, error?, seq}`. `text` is byte for byte what `tmux capture-pane -e -p` prints. `status` comes exactly when `exited` |
| `pty-subscribe.json` | `pty.subscribe`'s params: `{pty_id, read_only}` |
| `pty-kill.json` | `pty.kill`'s params: `{pty_id}` |
| `pty-list-reply.json` | `pty.list`'s reply: `{ptys: [{pty_id, label?, argv, cwd, pid, cols, rows, started_at, exited, status?, signal?, error?}]}`, in the order they started |
| `pty-output.json` | `pty.output`'s data, on the `pty` topic: `{pty_id, bytes_b64}`, the process's bytes in standard base64 |
| `pty-exited.json` | `pty.exited`'s data, on the `pty` topic: `{pty_id, status, signal?, error?}` |
| `run-start.json` | `run.start`'s params (7.5): `{run_id, kind, prompt?, mount, needs_connectors?, output_schema?, persist_session?, space?, append_system_prompt?, partial?, timeout_s?, deadline_ms?, pinned_call?, needs_you_when?, job_id?}`. `run_id` is a lower-case UUID, Claude Code's session id; `space` follows the space name rule; each of `needs_connectors` is a connector's id (1 to 256 bytes, no control character) |
| `run-ref.json` | One run by id, `{run_id}`: `run.start`'s reply, and `run.cancel`'s and `run.get`'s params |
| `run-resume.json` | `run.resume`'s params: `{run_id, reply?}`. Refused: `run_alive`, `not_resumable` (a `send_executor`), `no_session` |
| `run-resume-reply.json` | `run.resume`'s reply: `{run_id, attempt}`, the new attempt, from 2 |
| `run-get-reply.json` | `run.get`'s reply: `{run_id, kind, state, attempt, space, client, job_id?, started_at, ended_at?, finished_at?, last_run_seq, last_event?, session, structured_output?, calls}`, from the run's durable record, so it survives a restart |
| `run-list.json` | `run.list`'s params: `{states?, job_id?, limit?}` |
| `run-list-reply.json` | `run.list`'s reply: `{runs: [{run_id, kind, state, started_at, ended_at?, last_run_seq, job_id?, client, attempt, space}]}`, the latest started first |
| `run-events.json` | `run.events`'s params: `{run_id, from_run_seq?}` (default 1) |
| `run-events-reply.json` | `run.events`' reply, which ends its stream: `{run_id, last_run_seq}` |
| `run-cancel-reply.json` | `run.cancel`'s reply: `{run_id, cancelled, state}`. A run that had ended stays as it was, `cancelled: false` |
| `run-event.json` | One event of a run's stream: `event.json`, plus each 7.5 run event type's `data` (`run.queued`, `run.started`, `run.text`, `run.tool_result`, `run.rate_limited`, `run.needs_you`, `run.done`, `run.failed`) and 7.3's `needs_you.raised {id, source, run_id?, agent_id?, reason, text}` and `needs_you.cleared {id}`. Other types need only `event.json` |
| `run-event-list.json` | A JSON array of them, as `jq -s .` makes of a capture of the `run` and `needs_you` topics |
| `job-put.json` | `job.put`'s params (7.6): `{job_id, type, schedule, run?, connector?, tool?, input?, catch_up, enabled?, loud?}`. `schedule` is `{every_minutes}` or `{cron, tz}`; `run` is a `run.start` template without `run_id`, checked as `run.start` checks its params. Unknown fields are refused. Refused: `kind_mount_mismatch`, `pinned_call_required`, `not_implemented` (a `watch` job until it is served), `invalid_params` |
| `job-ref.json` | `{job_id}`: `job.delete`'s, `job.pause`'s, `job.resume`'s and `job.run_now`'s params, and `job.put`'s, `job.delete`'s, `job.pause`'s and `job.resume`'s reply. `unknown_job` for a job rlxd doesn't hold |
| `job-list.json` | `job.list`'s params: `{}` |
| `job-list-reply.json` | `job.list`'s reply: `{jobs: [{job_id, type, schedule, catch_up, enabled, paused, loud, next_fire_at, last_fire_at, last_run_id, last_state, skipped_count}]}` |
| `job-run-now-reply.json` | `job.run_now`'s reply: `{job_id, run_id}`. Refused: `app_absent` with no app gate client, `job_running` while the job's last run is live, `rate_limited` (`detail.resets_at`) |
| `job-event.json` | One event of the `job` topic: `event.json`, plus `job.put`, `job.deleted`, `job.paused` and `job.resumed` `{job_id}`, `job.fired {job_id, run_id, scheduled_for, fired_at, cause, loud}` and `job.skipped {job_id, scheduled_for, reason, code?}`, `code` for `reason: "start_failed"` |
| `space.json` | `space.ensure`'s and `space.erase`'s params: `{name}`, 1 to 64 lower-case letters, digits, `-`, `_` and `.`, starting with a letter or digit, no `..`, no `.` at the end, not a Windows device name |
| `space-ensure-reply.json` | `space.ensure`'s reply: `{name, path}`, the space's folder, absolute and resolved |
| `space-erase-reply.json` | `space.erase`'s reply: `{removed: [path], links?: [path]}`, `links` being links inside the space removed as links. Every link is listed, under the space's resolved path. Refused whole: `runs_live` (`detail.run_ids`), `erase_in_progress`, `link_refused` (`detail.paths`), and `erase_incomplete` (`detail {removed, failed}`) when some paths stayed |
| `connectors.json` | `connectors.list`'s reply (5.3, 7.7): `{connectors: [{id, source, status, tools: [{name, read_only_hint}], checked_at?, identity}], as_of, from, model_turns, accounts: [{account, label, connectors, as_of, from, error}]}`. `accounts` is each Claude Code account rlx knows, by its slot (1 is the default account), or `account`'s alone; the top-level `connectors`, `as_of` and `from` are the account rlxd's runs use. `source` is the kind, `claude.ai` or `mcp` (local); `status` is `connected`, `needs-auth`, `pending` or `failed`; `read_only_hint` is always there, `true`, `false` or `null`; `checked_at` is when the status was last read; `identity` is `null` until rlxd has read one (`{workspace?, user?, email?}`); `as_of` is `null` before any read; `from` is `init` (a run's or a one-turn probe's) or `listing` (with `connectors.no_turn`); an account's `error` is why its latest refresh failed (`settle_timeout`, `no_route`, `probe_failed`, `claude_not_found`), its connectors then the ones read before; `model_turns` is 0, or 1 for a one-turn refresh |
| `connectors-call.json` | `connectors.call`'s params (5.3): `{run_id, connector, tool, input}`. `run_id` is a new lower-case UUID; `connector` is a connector's id (1 to 256 bytes, no control character); `tool` is an MCP tool's name (`mcp__…`, at most 512 bytes) of that connector; `input` is an object. Without the capability `connectors.call` it is `no_route`; refused for `remote` (`role_not_allowed`) |
| `connectors-call-reply.json` | `connectors.call`'s reply: `{run_id, decision, content?, is_error?, model_turns}`. `decision` is `allow`; `content` is the tool's result content as Claude Code gave it, up to 1 MiB (a longer one is cut, as `run.tool_result`'s is); `model_turns` is always 0. A deny is the error `denied` (`detail: {run_id, call_id, by, reason}`); a Claude Code without `mcp_call_check_permissions_v1` is `no_route`; any other end of the call's run is `call_failed` (`detail: {run_id, reason}`, its `run.failed` reason); settings rlxd did not write in the call's working folder are `untrusted_settings` (`detail: {run_id, path}`), with no call made |
| `connectors-list.json` | `connectors.list`'s params: `{refresh?, allow_turn?, account?}`. `account` is a slot, from 1; one rlx doesn't know is `no_such_account`. `refresh` reads every account's connectors (or `account`'s) with no turn, with `connectors.no_turn`, failing only when none was read; without that route it is `no_route`, unless `allow_turn`, which `remote` may not pass (`role_not_allowed`), and which probes one account: `account`, or the default one |
| `connectors-login.json` | `connectors.login`'s params: `{id}`, 1 to 256 bytes with no control character, not beginning with `-`. Refused for `remote` (`role_not_allowed`) |
| `connectors-login-reply.json` | `connectors.login`'s reply: `{login_id}`, which its `connector.login.*` events carry |
| `connector-id.json` | `connector.added`'s and `connector.removed`'s data, on the `connector` topic: `{account, id}`, `account` the slot (1 when absent, in an event from before accounts) |
| `connector-changed.json` | `connector.changed`'s data: `{account, id, status, previous_status}`, once a real change, `account` the slot (1 when absent, in an event from before accounts) |
| `connector-login-open-url.json` | `connector.login.open_url`'s data: `{login_id, id, url}`, `url` always `https://` |
| `connector-login-waiting.json` | `connector.login.waiting`'s data: `{login_id}` |
| `connector-login-done.json` | `connector.login.done`'s data: `{login_id, id, status}` |
| `connector-login-error.json` | `connector.login.error`'s data: `{login_id, code, message}`, `code` one of `claude_not_found`, `no_url`, `not_https`, `failed` and `timed_out` |

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
- **A `daemon.gap {from, to, topics: ["pty"]}`** is narrower: only `pty.output` events
  in that span are gone. Every other event in it still comes, before the gap. Only a
  stream that asked for `pty` gets one.
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
- **`error`** names why rlxd is ending, or has ended, a process itself, on its screen,
  its list entry and its `pty.exited`: `screen_failed` when the screen model failed on
  the program's output. That PTY alone ends, its screen is blank from then on, and
  `pty.write` and `pty.resize` to it get `pty_exited`. It is a name, 1 to 64 lowercase
  letters and underscores, and a client should take one it doesn't know as "ended by
  rlxd".
- **`pty.kill`** ends the process and every process in its group (on Windows, its job),
  and frees the PTY. A process that left the group itself is not ended, as under tmux.
- **The stream.** `pty.subscribe {pty_id, read_only}` first sends a `pty.snapshot`, the
  screen as `pty-screen-reply.json` describes it, whose `seq` the stream goes on from.
  Then come the PTY's `pty.output` and `pty.exited` events. A client too slow to keep up
  never holds the PTY back: what it missed is dropped, and it gets a new `pty.snapshot`.
  The reply, `{seq}`, ends the stream once the PTY is killed or rlxd stops.
- **Output is kept apart.** rlxd keeps `pty.output` events in a budget of their own, so
  a busy PTY never pushes another topic's events out. A stream that asked for `pty` gets
  a `daemon.gap {from, to, topics: ["pty"]}` when output it hadn't read was dropped, and
  still every other event of that span; a stream that didn't ask for it never gets a gap
  on its account. `pty.exited` is kept with the other events, so a replay of `pty` still
  has it after a flood of output.
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
  most 128 KiB: an eighth of a frame, so even one of control characters, six bytes of
  JSON each, fits one. A longer write gets `write_too_large {max, bytes}`, and a frame over
  1 MiB gets `frame_too_large {max}`, with no `id`, which rlxd didn't read; rlxd skips the
  rest of that frame, and the connection stays open for the next request either way.
  A program that reads none of its input gets 16 MiB of it queued, then `pty_busy`:
  nothing of that write was taken, so write it again once the program reads. A
  `pty.screen` (or a stream's snapshot) longer than the 16 MiB line a client reads gets
  `screen_too_long`: ask for less `scrollback`.
- **The terminal** a process sees is `TERM=xterm-256color` with `COLORTERM=truecolor`,
  unless `env` says otherwise. The fleet's tmux sets `tmux-256color`, which macOS's
  terminfo lacks.
- **The environment** is rlxd's own, as the fleet's tmux server gives a window its own:
  less what ties a process to one session, board or agent (`core/spawn.py`'s
  `tmux_client_env`: the tracing identity, `AISQUARE_BROKERED*`, the board's
  variables, each agent's process identity, and `CODEX_HOME` from a Codex tool shell),
  and less tmux's own (`TMUX`, `TMUX_PANE`, `TERM_PROGRAM`, `TERM_PROGRAM_VERSION`).
  `pty-spawn.json` lists every name. `env` can put any back. For §1.8: a tmux pane gets
  `TERM_PROGRAM=tmux` and its own `TMUX`; an rlxd PTY gets neither.

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
