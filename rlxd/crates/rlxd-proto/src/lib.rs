//! rlxd's wire types, and the JSON Schemas generated from them.
//!
//! These are the shapes of handoff 0.10.3's app contract (rev 5):
//!
//! - [`Envelope`] (7.2): every response, from the CLI under `--json --schema-version 1`
//!   and from the socket, where it also carries the request's `id`;
//! - [`Request`] (7.2): one newline-delimited request frame on the socket;
//! - [`HelloParams`] and [`HelloData`] (7.2, 5.1): the protocol handshake, the first
//!   frame on a connection and its reply;
//! - [`Event`] (7.3): one event on the event stream; `event-list.json` is an array of
//!   them;
//! - [`DaemonRecord`] (4.1): one non-UI rlx process in `<home>/daemons/`, a file that
//!   Python's registry and rlxd both write and read.
//!
//! # One set of rules
//!
//! The checked-in `rlxd/schemas/v1/*.json` are generated from these types by the
//! `gen-schemas` binary ([`schemas::all`]), and CI fails when they drift. The types hold
//! every document to the same rules as its schema, so rlxd and its clients need no
//! schema at run time:
//!
//! - **Parsing refuses** what the schema refuses: another `schema_version` ([`V1`]), a
//!   missing or contradictory field, an empty code, a `time` that is not RFC 3339, a
//!   type that is not `<topic>.<event>`, a `run` event without its run fields. The error
//!   names the field at fault.
//! - **The constructors refuse it too,** returning [`Invalid`], and printing a value
//!   whose fields were changed into a broken shape fails rather than emit it.
//! - **Each type's `validate()`** is that one set of rules, for code that builds a value
//!   field by field.
//!
//! JSON Schema cannot express two of the rules, so the types are stricter there:
//!
//! - It reads `1.0` as the integer `1`, which the types refuse. No producer of these
//!   documents writes integers that way.
//! - It cannot compare one field with another. So the schema checks that an event's
//!   `type` starts with its `topic` only for the topics v1 lists, while the types check
//!   it for every topic.
//!
//! The doc comments of these types and their fields become the schemas' descriptions,
//! so they are written for an app's developer and carry no rustdoc links.
//!
//! This crate does no I/O: it is types, constants and schema generation only, so the
//! daemon, `rlxd-client` and rlxapp can all depend on it.
//!
//! # The handshake
//!
//! A client's first frame is `hello`:
//!
//! - its [`PROTOCOL_VERSION`] in `protocol_version`;
//! - its `capabilities` and the shared token;
//! - who it is: `client`, `client_version` and `role`.
//!
//! The reply names:
//!
//! - the `protocol_version` the connection speaks;
//! - every version rlxd serves (`protocol_versions`);
//! - `rlxd_version` and rlxd's `capabilities`.
//!
//! A version rlxd does not serve gets the error [`codes::INCOMPATIBLE_PROTOCOL`], with
//! `detail.supported` listing the versions it does, and never a silent misparse
//! ([`ErrorBody::incompatible_protocol`]). The protocol version is the `schema_version`
//! of every envelope and event.
//!
//! # The gate credential
//!
//! The shared token lets a client in. It does not make a connection the tool-call gate.
//! Only rlxapp may hold the gate role (5.1), and rlxd checks two things:
//!
//! - **the credential.** A `hello {role: "app"}` carries `gate_credential`, which
//!   `gate.pair` mints once, for a peer that passes the identity check. Pairing again
//!   replaces it.
//! - **the peer's identity**, where the OS allows it.
//!
//! A `role: app` hello without a valid credential, or from a peer that fails the
//! identity check, gets [`codes::NOT_GATE`], and it never counts as an app client. So
//! does a tool-call decision from a connection that is not the gate.
//!
//! Gate dev mode is for unsigned dev and CI builds, and is never the default. It
//! relaxes only the identity check; the credential is still required. In CI, the
//! first `rlxd call --as-app` pairs, keeps the credential in
//! `$AISQUARE_HOME/test-gate-credential`, and later calls present it.
//!
//! This crate carries the field and the code. Minting, checking and refusing are the
//! daemon's.
//!
//! # Two sequences
//!
//! - **`seq`**, on every event: strictly increasing per home, across every topic.
//!   `events.subscribe {since}` resumes after it.
//! - **`data.run_seq`**, on every event of the `run` topic: strictly increasing per run,
//!   from 1. Replaying one run from a known `run_seq` needs it.
//!
//! Every run event's `data` also carries the run's other fields (7.5): `run_id`, `kind`
//! and `attempt`, which starts at 1 and goes up by one with each `run.resume`, plus
//! `job_id` when a scheduler job fired the run. [`Event::for_run`] sets them, and
//! [`Event::run_scope`] reads them back.
//!
//! # Versioning
//!
//! [`SCHEMA_VERSION`], the protocol version, is an integer. New fields and new event
//! types keep it, and clients ignore what they don't know: no type here denies unknown
//! fields. Removing or renaming a field raises `protocol_version` by one, and rlxd keeps
//! serving the previous one for one rlx minor release.
//!
//! The wire structs and [`Role`] are `#[non_exhaustive]`. Build the structs with their
//! constructors, match them with `..`, and give a `match` on a role a wildcard arm. A
//! field or a role added later then breaks neither the wire nor a caller's code.
//!
//! # v1 is a draft until 0.10.3 ships
//!
//! Until rlx 0.10.3 is released, v1 can still change in ways the rules above would
//! otherwise forbid, as the handoff settles. From that release on, v1 changes only
//! additively, under the rules above.
//!
//! # Follow-ups
//!
//! These are noted here, and are not part of v1 yet:
//!
//! - **A stream id on event frames.** Event frames carry no request or subscription id,
//!   so a client can't tell two streams on one connection apart. Adding an optional `id`
//!   is additive. The name is reserved for 1.7's event bus to settle.
//! - **The daemon record's strings** may still be empty: `host`, `version` and the items
//!   of `argv`. `cwd` isn't required to be absolute. Python's registry (#576) and this
//!   schema tighten them together, or not at all.

mod check;
mod daemon;
mod envelope;
mod event;
mod hello;
mod marker;
mod request;
pub mod schemas;

pub use check::Invalid;
pub use daemon::{DaemonRecord, kinds as daemon_kinds};
pub use envelope::{Data, Detail, Envelope, ErrorBody, Outcome, codes};
pub use event::{Event, RunScope, topics};
pub use hello::{HelloData, HelloParams, Role};
pub use marker::{False, True, V1};
pub use request::{Params, Request, RequestId, methods};

/// The protocol version this crate speaks: `hello`'s `protocol_version`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Every protocol version this crate can serve: `hello`'s reply's `protocol_versions`.
pub const PROTOCOL_VERSIONS: &[u32] = &[PROTOCOL_VERSION];

/// The `schema_version` of every document this crate reads and writes, which is the
/// protocol version.
pub const SCHEMA_VERSION: u32 = PROTOCOL_VERSION;

/// rlxd's version, which is rlx's version: one number (handoff 0.10.3, 1.2).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A document's `schema_version`, read without parsing the rest. Use it to tell a newer
/// document, which the types refuse, from a broken one.
pub fn schema_version_of(document: &serde_json::Value) -> Option<u64> {
    document.get("schema_version")?.as_u64()
}
