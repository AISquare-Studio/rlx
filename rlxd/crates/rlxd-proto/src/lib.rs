//! rlxd's wire types, and the JSON Schemas generated from them.
//!
//! These are the shapes of handoff 0.10.3's app contract:
//!
//! - [`Envelope`] (7.2): every response, from the CLI under `--json --schema-version 1`
//!   and from the socket, where it also carries the request's `id`;
//! - [`Request`] (7.2): one newline-delimited request frame on the socket;
//! - [`HelloParams`] and [`HelloData`] (7.2): the first frame on a connection, and its reply;
//! - [`Event`] (7.3): one event on the event stream; `event-list.json` is an array of them.
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
//! [`SCHEMA_VERSION`] is an integer. New fields and new event types keep it, and
//! clients ignore what they don't know: no type here denies unknown fields. Removing
//! or renaming a field raises it, and rlxd serves the old version for one minor release.
//!
//! The wire structs are `#[non_exhaustive]`: build them with their constructors, and
//! match them with `..`. A field added later then breaks neither the wire nor a
//! caller's code.
//!
//! # v1 is a draft
//!
//! Version 1 is not frozen until the handoff's third revision. rlxapp's architecture
//! review asks for engine changes that revision will carry. On the wire it is expected
//! to add a protocol (handshake) version to `hello`, and per-stream sequence numbers
//! to events alongside the per-home `seq`. Until then, v1 can still change in ways the
//! rules above would otherwise forbid.

mod envelope;
mod event;
mod hello;
mod marker;
mod request;
pub mod schemas;

pub use envelope::{Data, Envelope, ErrorBody, Outcome, codes};
pub use event::{Event, topics};
pub use hello::{HelloData, HelloParams, Role};
pub use marker::{False, True};
pub use request::{Params, Request, RequestId, methods};

/// The schema version this crate speaks, and the `schema_version` of every document it builds.
pub const SCHEMA_VERSION: u32 = 1;

/// Every schema version this crate can serve, as `hello`'s reply lists them.
pub const SCHEMA_VERSIONS: &[u32] = &[SCHEMA_VERSION];

/// rlxd's version, which is rlx's version: one number (handoff 0.10.3, 1.2).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
