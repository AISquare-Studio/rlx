//! The library rlxapp depends on to talk to rlxd: connect, call, subscribe (handoff
//! 0.10.3, section 1). rlxapp takes it as a git dependency pinned to a release tag.
//!
//! A stub for now (1.1): it re-exports the wire types. The connection, `hello`, calls
//! and the event subscription arrive with 1.3 and 1.7.

pub use rlxd_proto as proto;
