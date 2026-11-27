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
//! So a program rlxd starts is started holding [`alone`]: the brain (`crate::brain`),
//! Claude Code's probes (`crate::claude`), and the runner's runs (`crate::runner::process`,
//! its guard on POSIX and the run itself on Windows). Only the start itself is held, never
//! the wait for the program's end. The PTY host's start (`crate::pty`, `spawn_command`)
//! does not take it yet.

use std::sync::{Mutex, MutexGuard, PoisonError};

static SPAWNING: Mutex<()> = Mutex::new(());

/// Held while one program starts; no other starts until it is dropped.
pub(crate) fn alone() -> MutexGuard<'static, ()> {
    SPAWNING.lock().unwrap_or_else(PoisonError::into_inner)
}
