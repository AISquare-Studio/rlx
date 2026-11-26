//! One program start at a time, across rlxd.
//!
//! On macOS a pipe is made close-on-exec only after it is made (`pipe`, then
//! `FD_CLOEXEC`), so a program another thread starts in between inherits it, and holds it
//! open for as long as it runs:
//! - a brain holding a Claude Code probe's stdout keeps that probe, and the `status` that
//!   asked, waiting out the probe's timeout;
//! - a probe or any other program holding the brain's stdin keeps the brain from seeing
//!   rlxd go.
//!
//! So every program rlxd starts is started holding [`alone`]: the brain
//! (`crate::brain`) and Claude Code's probes (`crate::claude`). A later item that starts
//! programs in rlxd (the runner, the PTY host) takes it too. Only the start itself is
//! held, never the wait for the program's end.

use std::sync::{Mutex, MutexGuard, PoisonError};

static SPAWNING: Mutex<()> = Mutex::new(());

/// Held while one program starts; no other starts until it is dropped.
pub(crate) fn alone() -> MutexGuard<'static, ()> {
    SPAWNING.lock().unwrap_or_else(PoisonError::into_inner)
}
