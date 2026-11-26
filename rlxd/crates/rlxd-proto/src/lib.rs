//! rlxd's wire types, and the JSON Schemas generated from them.
//!
//! These are the shapes of handoff 0.10.3's app contract (rev 6):
//!
//! - [`Envelope`] (7.2): every response, from the CLI under `--json --schema-version 1`
//!   and from the socket, where it also carries the request's `id`;
//! - [`Request`] (7.2): one newline-delimited request frame on the socket;
//! - [`HelloParams`] and [`HelloData`] (7.2, 5.1): the protocol handshake, the first
//!   frame on a connection and its reply;
//! - [`Event`] (7.3): one event on the event stream; `event-list.json` is an array of
//!   them;
//! - [`SubscribeParams`] (7.3): `events.subscribe`'s params, which open the stream;
//! - the settings API (the multi-device brief, §4): [`SettingsGetParams`],
//!   [`SettingsSetParams`], [`SettingsDeleteParams`], [`SettingsListParams`],
//!   [`Setting`], [`SettingsList`] and [`SettingsChanged`], app settings every device
//!   shares;
//! - [`StatusData`] (7.4): `status`'s reply, what rlxd is and whether Claude Code is
//!   installed and signed in;
//! - [`HereData`]: `rlxd here`'s reply, the probe rlx.app runs over SSH: what rlx is
//!   installed, whether its rlxd runs, and the protocol versions it serves;
//! - [`DaemonRecord`] (4.1): one non-UI rlx process in `<home>/daemons/`, a file that
//!   Python's registry and rlxd both write and read;
//! - [`BrainInput`] and [`BrainOutput`] (1.6): the frames rlxd and its resident Python
//!   brain, `rlx brain --stdio`, write to each other. That protocol is internal to one rlx
//!   release, and versioned on its own ([`BRAIN_PROTOCOL`]);
//! - [`HookParams`], [`HookReply`] and [`HookEventData`] (1.5, 7.3): a Claude Code hook
//!   rlxd-hook hands rlxd over the socket, what rlxd answers, and the `hook.*` event it
//!   publishes.
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
//! JSON Schema cannot express three of the rules, so the types are stricter there:
//!
//! - It reads `1.0` as the integer `1`, which the types refuse. No producer of these
//!   documents writes integers that way.
//! - It cannot count how deep a value nests: a setting's value is at most 32 arrays or
//!   objects deep ([`MAX_SETTING_DEPTH`]).
//! - It cannot compare one field with another. So the schema checks that an event's
//!   `type` starts with its `topic` only for the topics v1 lists (7.3's and 7.8's), while
//!   the types check it for every topic.
//!
//! Every unsigned integer's schema carries its Rust type's `maximum` too, so a
//! `protocol_version` of 2^32 or a `seq` of 2^64 is refused by both. A `time` with
//! second `60` is refused: RFC 3339 allows a leap second, but validators disagree on
//! it, so the types hold to `59`, the bound they agree on.
//!
//! The unchecked serde code of each wire type lives on a private mirror struct
//! (`#[serde(remote = …)]`). So `Event::deserialize` and `Event::serialize`, called by
//! path, are the validating trait impls; `tests/direct_calls.rs` holds every type to that.
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
//! ([`ErrorBody::incompatible_protocol`]). rlxd reads the version with
//! [`protocol_version_of`] before it parses the rest, so a hello it can't parse still
//! gets that answer. The protocol version is the `schema_version`
//! of every envelope and event.
//!
//! # The gate credential
//!
//! The shared token lets a client in. It does not make a connection the tool-call gate.
//! Only rlxapp may hold the gate role (5.1), and rlxd checks two things:
//!
//! - **the credential.** A `hello {role: "app"}` carries `gate_credential`, which
//!   `gate.pair` mints for one named device, once, for a peer that passes the identity
//!   check. The first pairing is trust on first use. Re-pairing a device needs its
//!   current credential and replaces it; a new device, once one is paired, needs an
//!   approval from a paired device (`gate.pair_answer`) or the user on the host.
//! - **the peer's identity**, where the OS allows it, for a device paired locally.
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
//! This crate carries the field, the method names ([`methods`]) and the codes. Minting,
//! checking and refusing are the daemon's.
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
//! # The stream
//!
//! `events.subscribe` ([`SubscribeParams`]) turns its connection into a stream. A
//! connection carries at most one, so event frames carry no `id`: a frame with `ok` is a
//! reply, and any other is an event. The subscribe's own reply comes last and ends the
//! stream, `{seq}`, the last seq the stream covered. Replies to other requests sent on
//! the connection meanwhile come between the events, each with its `id`.
//!
//! A `daemon.gap {from, to}` ([`event_types::DAEMON_GAP`]) says the events numbered
//! `from` to `to` are gone. It starts the stream when `since` is older than the oldest
//! event rlxd holds, and can also come anywhere later, for a client so slow that rlxd
//! drops events it has not read. It comes whatever the topics, and its own `seq` is `to`.
//! A `since` past the last event gets [`codes::SINCE_AHEAD`], with `detail.head`.
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
//! - **A stream id on event frames.** 1.7 settled on one stream per connection, so event
//!   frames carry no `id`. Should a connection ever carry more than one, an optional `id`
//!   on event frames, the subscribe's, is additive. The name stays reserved for it.
//! - **The `daemon` events' `data`** is not checked by `event.json` yet: `daemon.gap`'s
//!   `{from, to}` and the others are described in [`event_types`] only.
//! - **Lowering `send_safety_mode` needs no confirm** in rlxd: an app-role connection's
//!   write takes effect at once, and is logged as a warning. Whether it should need one,
//!   such as a confirm on another paired device, is with the hub.
//! - **`send_safety_mode`'s shape and mode names** (`{default, connectors?}`, `draft`,
//!   `standing_approval`, `auto`) are rlxd's proposal until the remote addendum states
//!   them.
//! - **A numbering epoch.** A `since` from a home whose numbering started again (its
//!   mark deleted) gets `since_ahead` only until the new numbering passes it. An id made
//!   with the numbering, carried in `status` and `daemon.started`, would let a client
//!   always tell.
//! - **The daemon record's strings** may still be empty: `host`, `version` and the items
//!   of `argv`. `cwd` isn't required to be absolute. Python's registry (#576) and this
//!   schema tighten them together, or not at all.

pub mod b64;
mod brain;
mod check;
mod daemon;
mod envelope;
mod event;
mod hello;
mod here;
mod hook;
mod marker;
mod request;
pub mod schemas;
mod settings;
mod status;
mod subscribe;

pub use brain::{BRAIN_PROTOCOL, BRAIN_PROTOCOLS, BrainInput, BrainOutput, codes as brain_codes};
pub use check::Invalid;
pub use daemon::{DaemonRecord, kinds as daemon_kinds};
pub use envelope::{Data, Detail, Envelope, ErrorBody, Outcome, codes};
pub use event::{Event, RunScope, topics, types as event_types};
pub use hello::{HelloData, HelloParams, Role};
pub use here::{Auth, HereData, InstalledRlx, InstalledRlxd};
pub use hook::{
    HOOK_ENV_NAMES, HOOK_EVENTS, HookEventData, HookParams, HookReply, PRE_TOOL_USE,
    codes as hook_codes, deny_output, event_type as hook_event_type,
};
pub use marker::{False, True, V1};
pub use request::{Params, Request, RequestId, methods};
pub use settings::{
    MAX_DEPTH as MAX_SETTING_DEPTH, MAX_KEY as MAX_SETTING_KEY, Setting, SettingsChanged,
    SettingsDeleteParams, SettingsGetParams, SettingsList, SettingsListParams, SettingsSetParams,
    check_key as check_setting_key, check_value as check_setting_value,
    check_value_to_write as check_setting_value_to_write, is_protected as setting_is_protected,
    keys as setting_keys, send_safety_lowers, send_safety_modes,
};
pub use status::{Brain, Claude, Clients, Jobs, Paths, Runs, StatusData};
pub use subscribe::SubscribeParams;

/// The protocol version this crate speaks: `hello`'s `protocol_version`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Every protocol version this crate can serve: `hello`'s reply's `protocol_versions`.
pub const PROTOCOL_VERSIONS: &[u32] = &[PROTOCOL_VERSION];

/// The `schema_version` of every document this crate reads and writes, which is the
/// protocol version.
pub const SCHEMA_VERSION: u32 = PROTOCOL_VERSION;

/// The largest process id any document here may carry: 2^31 - 1, `i32::MAX`. A pid
/// goes to `kill`, whose `pid_t` is a C `int`, so a larger one would wrap to a negative
/// pid, and `-1` signals every process the caller may signal. Python's registry reads a
/// larger pid as no pid at all (`aisquare.core.registry.PID_MAX`).
pub const PID_MAX: u32 = 2_147_483_647;

/// rlxd's version, which is rlx's version: one number (handoff 0.10.3, 1.2).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A document's `schema_version`, read without parsing the rest. Use it to tell a newer
/// document, which the types refuse, from a broken one.
pub fn schema_version_of(document: &serde_json::Value) -> Option<u64> {
    document.get("schema_version")?.as_u64()
}

/// A `hello`'s `protocol_version`, read from its raw params without parsing the rest.
///
/// rlxd reads this first. [`HelloParams`] refuses version 0, a version above `u32`, and a
/// later protocol's hello whose other fields have moved. Each of those must still get
/// [`codes::INCOMPATIBLE_PROTOCOL`] ([`ErrorBody::incompatible_protocol`]), not a parse
/// error.
///
/// `None` when the params have no `protocol_version`, and also when it is not a whole
/// number from 0 to `u64::MAX` as JSON writes one: a string, `-1`, `2.0`, or `2^64` and
/// above. A caller that must answer those with `incompatible_protocol` too reads the
/// raw value itself.
pub fn protocol_version_of(params: &serde_json::Value) -> Option<u64> {
    params.get("protocol_version")?.as_u64()
}
