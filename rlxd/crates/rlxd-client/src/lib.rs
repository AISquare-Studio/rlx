//! The library rlxapp depends on to talk to rlxd: find it, connect, say `hello` and call
//! (handoff 0.10.3, sections 1 and 7.2). rlxapp takes it as a git dependency pinned to a
//! release tag.
//!
//! - [`Home`] resolves the home as rlx does (`RLX_HOME`, then `AISQUARE_HOME`), and
//!   derives every path rlxd and its clients use from it.
//! - [`Connection`] opens the home's socket, or named pipe on Windows, and sends requests
//!   and reads their replies, one at a time.
//!
//! ```no_run
//! use rlxd_client::{Connection, Home};
//! use rlxd_client::proto::{HelloParams, Role};
//!
//! let home = Home::from_env()?;
//! let token = std::fs::read_to_string(home.token())?;
//! let mut connection = Connection::open(&home)?;
//! let hello = HelloParams::new(token.trim(), "my-tool", "0.1.0", Role::Cli)?;
//! assert!(connection.hello(&hello)?.is_ok());
//! let status = connection.request("status", Default::default())?;
//! println!("{}", status.line);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! The event subscription arrives with the event bus (1.7).

mod connection;
mod home;
#[cfg(windows)]
pub mod windows;

pub use connection::{CallError, ConnectError, Connection, MAX_LINE, Reply};
pub use home::{DEFAULT_HOME_DIR, HOME_ENV, Home, LEGACY_HOME_ENV, NoHome};
pub use rlxd_proto as proto;
