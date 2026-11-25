//! rlxd's wire types, and the JSON Schemas generated from them.
//!
//! These are the shapes of handoff 0.10.3's app contract, with the protocol changes of
//! rlxapp's Engine change spec (sections 1 and 3), which win over the handoff until its
//! third revision lands:
//!
//! - [`Envelope`] (7.2): every response, from the CLI under `--json --schema-version 1`
//!   and from the socket, where it also carries the request's `id`;
//! - [`Request`] (7.2): one newline-delimited request frame on the socket;
//! - [`HelloParams`] and [`HelloData`] (7.2, spec 3): the protocol handshake, the first
//!   frame on a connection and its reply;
//! - [`Event`] (7.3, spec 1): one event on the event stream; `event-list.json` is an
//!   array of them;
//! - [`DaemonRecord`] (4.1): one non-UI rlx process in `<home>/daemons/`, a file that
//!   Python's registry and rlxd both write and read.
//!
//! # The handshake
//!
//! A client's first frame is `hello` with its [`PROTOCOL_VERSION`] in
//! `protocol_version`, its `capabilities`, its token, and who it is (`client`,
//! `client_version`, `role`). The reply names the `protocol_version` the connection
//! speaks, every version rlxd serves (`protocol_versions`), `rlxd_version` and rlxd's
//! `capabilities`. A version rlxd does not serve gets the error
//! [`codes::INCOMPATIBLE_PROTOCOL`], with `detail.supported` listing the versions it
//! does, and never a silent misparse ([`ErrorBody::incompatible_protocol`]). The
//! protocol version is the `schema_version` of every envelope and event.
//!
//! # Two sequences
//!
//! - **`seq`**, on every event: strictly increasing per home, across every topic.
//!   `events.subscribe {since}` resumes after it.
//! - **`data.run_seq`**, on every event of the `run` topic, next to `data.run_id`:
//!   strictly increasing per run, from 1. Replaying one run from a known `run_seq` needs
//!   it ([`Event::for_run`], [`Event::run_scope`]).
//!
//! The checked-in `rlxd/schemas/v1/*.json` are generated from these types by the
//! `gen-schemas` binary ([`schemas::all`]), and CI fails when they drift.
//!
//! The doc comments of these types and their fields become the schemas' descriptions,
//! so they are written for an app's developer and carry no rustdoc links.
//!
//! This crate does no I/O: it is types, constants and schema generation only, so the
//! daemon, `rlxd-client` and rlxapp can all depend on it.
//!
//! # Versioning
//!
//! [`SCHEMA_VERSION`], the protocol version, is an integer. New fields and new event types keep it, and
//! clients ignore what they don't know: no type here denies unknown fields. Removing
//! or renaming a field raises it, and rlxd serves the old version for one minor release.
//!
//! The wire structs are `#[non_exhaustive]`: build them with their constructors, and
//! match them with `..`. A field added later then breaks neither the wire nor a
//! caller's code.
//!
//! # v1 is a draft
//!
//! Version 1 is not frozen until the handoff's third revision lands. It follows rlxapp's
//! Engine change spec now, and the third revision may still rename or move what is
//! here. Until then, v1 can change in ways the rules above would otherwise forbid.

mod daemon;
mod envelope;
mod event;
mod hello;
mod marker;
mod request;
pub mod schemas;

pub use daemon::{DaemonRecord, kinds as daemon_kinds};
pub use envelope::{Data, Envelope, ErrorBody, Outcome, codes};
pub use event::{Event, RunScope, topics};
pub use hello::{HelloData, HelloParams, Role};
pub use marker::{False, True};
pub use request::{Params, Request, RequestId, methods};

/// The protocol version this crate speaks: `hello`'s `protocol_version`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Every protocol version this crate can serve: `hello`'s reply's `protocol_versions`.
pub const PROTOCOL_VERSIONS: &[u32] = &[PROTOCOL_VERSION];

/// The `schema_version` of every document this crate builds, which is the protocol version.
pub const SCHEMA_VERSION: u32 = PROTOCOL_VERSION;

/// rlxd's version, which is rlx's version: one number (handoff 0.10.3, 1.2).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
