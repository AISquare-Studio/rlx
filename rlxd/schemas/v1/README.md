# rlxd schemas, version 1

> **v1 is a draft until the handoff's third revision lands.** The wire format is not
> frozen. It follows rlxapp's Engine change spec now (the `hello` handshake and the
> per-run sequence below), and the third revision may still rename or move fields.
> Until then, v1 can change in ways the versioning rules below would otherwise forbid.

JSON Schemas (draft 2020-12) for rlxd's wire format (handoff 0.10.3, 7.2 and 7.3, with
sections 1 and 3 of rlxapp's Engine change spec).
They are generated from the types in `rlxd/crates/rlxd-proto`. **Don't edit them by
hand**: change the types, then regenerate and commit.

```sh
cd rlxd
cargo run -p rlxd-proto --bin gen-schemas            # regenerate
cargo run -p rlxd-proto --bin gen-schemas -- --check # what CI runs: exit 1 on drift
```

| File | What it describes |
|---|---|
| `envelope.json` | Every response: `{schema_version, ok, data}` or `{schema_version, ok, error{code, message, hint?, detail?}}`, plus `id` on the socket. `incompatible_protocol` must carry `detail.supported` |
| `event.json` | One event: `{schema_version, seq, time, topic, type, data}`. On the `run` topic, `data` also carries `run_id` and `run_seq` |
| `event-list.json` | A JSON array of events, as `jq -s .` makes of `rlxd events`' lines |
| `request.json` | One socket request frame: `{id, method, params?}` |
| `hello.json` | `hello`'s params: `{token, protocol_version, client, client_version, role, capabilities, env?}`, where `role` is `app`, `tui`, `remote` or `cli` |
| `hello-reply.json` | `hello`'s reply data: `{protocol_version, protocol_versions, rlxd_version, capabilities}` |

Validate a document with `uvx check-jsonschema --schemafile rlxd/schemas/v1/<schema> <file>`.

## The handshake

The first frame is `hello` with the client's `protocol_version`. The reply names the
version the connection speaks, and every version rlxd serves. A version rlxd does not
serve gets the error `incompatible_protocol`, whose `detail.supported` lists the ones it
does, never a silent misparse. The protocol version is the `schema_version` of every
envelope and event.

## Two sequences

- `seq`, on every event: strictly increasing per home. `events.subscribe {since}`
  resumes after it.
- `data.run_seq`, on every `run`-topic event, next to `data.run_id`: strictly increasing
  per run, from 1, so one run can be replayed from a known point.

## Versioning

`schema_version` is an integer, and every document here carries `1`.

- New fields and new event types keep the version. Clients ignore what they don't
  know, so no schema here forbids extra properties.
- Removing or renaming a field raises the version. The new schemas go in
  `schemas/v2/`, and rlxd serves the old version for one minor release.
