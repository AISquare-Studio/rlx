# rlxd schemas, version 1

> **v1 is a draft until the handoff's third revision.** The wire format is not frozen.
> rlxapp's architecture review asks for engine changes that revision will carry. On
> the wire it is expected to add a protocol (handshake) version to `hello`, and
> per-stream sequence numbers to events alongside the per-home `seq`. Until then, v1
> can still change in ways the versioning rules below would otherwise forbid.

JSON Schemas (draft 2020-12) for rlxd's wire format (handoff 0.10.3, 7.2 and 7.3).
They are generated from the types in `rlxd/crates/rlxd-proto`. **Don't edit them by
hand**: change the types, then regenerate and commit.

```sh
cd rlxd
cargo run -p rlxd-proto --bin gen-schemas            # regenerate
cargo run -p rlxd-proto --bin gen-schemas -- --check # what CI runs: exit 1 on drift
```

| File | What it describes |
|---|---|
| `envelope.json` | Every response: `{schema_version, ok, data}` or `{schema_version, ok, error{code, message, hint?, detail?}}`, plus `id` on the socket |
| `event.json` | One event: `{schema_version, seq, time, topic, type, data}` |
| `event-list.json` | A JSON array of events, as `jq -s .` makes of `rlxd events`' lines |
| `request.json` | One socket request frame: `{id, method, params?}` |
| `hello.json` | `hello`'s params: `{token, client, client_version, role, env?}`, where `role` is `app`, `tui`, `remote` or `cli` |
| `hello-reply.json` | `hello`'s reply data: `{daemon_version, schema_versions, capabilities}` |

Validate a document with `uvx check-jsonschema --schemafile rlxd/schemas/v1/<schema> <file>`.

## Versioning

`schema_version` is an integer, and every document here carries `1`.

- New fields and new event types keep the version. Clients ignore what they don't
  know, so no schema here forbids extra properties.
- Removing or renaming a field raises the version. The new schemas go in
  `schemas/v2/`, and rlxd serves the old version for one minor release.
