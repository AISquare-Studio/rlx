# rlxd schemas, version 1

> **v1 is a draft until rlx 0.10.3 ships.** Until that release, v1 can still change in
> ways the versioning rules below would otherwise forbid, as the handoff settles. From
> 0.10.3 on, v1 changes only additively.

These are JSON Schemas (draft 2020-12) for rlxd's wire format, following handoff 0.10.3
rev 5: 7.2, 7.3, 5.1 and 4.1. They are generated from the types in
`rlxd/crates/rlxd-proto`.

**Don't edit them by hand.** Change the types, then regenerate and commit:

```sh
cd rlxd
cargo run -p rlxd-proto --bin gen-schemas            # regenerate
cargo run -p rlxd-proto --bin gen-schemas -- --check # what CI runs: exit 1 on drift
```

The types hold every document to the same rules as its schema. A document the schema
refuses also fails to parse, with an error naming the field. The one exception is a
float with a zero fraction, such as `1.0` where an integer belongs: JSON Schema accepts
it, and the types refuse it.

| File | What it describes |
|---|---|
| `daemon-record.json` | One record in `<home>/daemons/` (4.1): `{id, kind, pid, host, version, started_at, heartbeat_at, argv, restartable, cwd?}`, which Python's registry and rlxd share |
| `envelope.json` | Every response: `{schema_version, ok: true, data}` or `{schema_version, ok: false, error{code, message, hint?, detail?}}`, never both, plus `id` on the socket. `detail` is a string or an object. `incompatible_protocol` must carry `detail.supported` |
| `event.json` | One event: `{schema_version, seq, time, topic, type, data}`. On the `run` topic, `data` also carries `run_id` and `run_seq` |
| `event-list.json` | A JSON array of events, as `jq -s .` makes of `rlxd events`' lines |
| `request.json` | One socket request frame: `{id, method, params?}`. `id` is at most 2^53 - 1 |
| `hello.json` | `hello`'s params: `{token, protocol_version, client, client_version, role, gate_credential?, capabilities, env?}`, where `role` is `app`, `tui`, `remote` or `cli` |
| `hello-reply.json` | `hello`'s reply data: `{protocol_version, protocol_versions, rlxd_version, capabilities}` |

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
- `data.run_seq`, on every `run`-topic event, next to `data.run_id`: strictly increasing
  per run, from 1, so one run can be replayed from a known point.

## Versioning

`schema_version` is an integer, and every document here carries `1`. A document with
any other `schema_version` fails to parse rather than misparse.

- New fields and new event types keep the version. Clients ignore what they don't
  know, so no schema here forbids extra properties.
- Removing or renaming a field raises `protocol_version` by one. The new schemas go in
  `schemas/v2/`, and rlxd keeps serving v1 for one rlx minor release.
