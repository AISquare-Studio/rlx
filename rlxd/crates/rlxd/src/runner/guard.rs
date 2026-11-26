//! `rlxd __run-guard -- <claude> <args…>`: what each run's Claude Code is started under on
//! macOS and Linux (handoff 0.10.3, 2.1 and 5.1), so that every process of the run ends
//! with it, and no process that isn't the run's is ever killed.
//!
//! - **Claude Code leads its own process group**, started by the guard with its stdio as
//!   the guard got it, so the run's pipes are Claude Code's own. The guard then points its
//!   own stdin, stdout and stderr at `/dev/null`, so the pipes end when Claude Code's do.
//! - **When Claude Code exits,** the guard kills its whole group (`SIGKILL`): what Claude
//!   Code left running, such as MCP servers or a tool's background process. It does so
//!   before it reaps Claude Code, so the group's id, Claude Code's pid, is still held by
//!   the exited process and can't be anyone else's: the OS gives out no pid, as a process
//!   or a group id, while a process or a group still holds it.
//! - **It reports Claude Code's end faithfully:** it exits with Claude Code's code, or
//!   dies of the signal Claude Code died of.
//! - **When rlxd dies,** the guard kills the group and exits. It checks its parent every
//!   100 ms (an exited parent hands its children to another), and on `SIGTERM` or
//!   `SIGHUP` it does the same at once: rlxd's cancel and timeout send `SIGTERM`, and on
//!   Linux the guard's parent-death signal is `SIGTERM`. Until Claude Code is reaped, the
//!   group's id is held, so these kills reach the run's group and nothing else.
//!
//! It is a hidden subcommand of rlxd, absent from `--help`, and it only ever kills Claude
//! Code's group.
#![allow(unsafe_code)]

use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread;
use std::time::Duration;

/// The run's group (Claude Code's pid) while it may be killed: 0 once Claude Code is
/// reaped, when nothing may be.
static GROUP: AtomicI32 = AtomicI32::new(0);

/// How often the guard checks that rlxd still runs.
const PARENT_CHECK: Duration = Duration::from_millis(100);

/// `rlxd __run-guard -- <program> <args…>`.
pub fn main(argv: &[OsString]) -> ExitCode {
    // SAFETY: getppid(2) takes nothing and can't fail.
    let parent = unsafe { libc::getppid() };
    install_handlers();
    let Some((program, args)) = argv.split_first() else {
        eprintln!("usage: rlxd __run-guard -- <program> [args...]");
        return ExitCode::from(2);
    };
    let mut command = std::process::Command::new(program);
    {
        use std::os::unix::process::CommandExt;
        command.args(args).process_group(0);
    }
    let child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!(
                "rlxd __run-guard: {} could not start: {e}",
                program.to_string_lossy()
            );
            return ExitCode::from(127);
        }
    };
    let Ok(pid) = libc::pid_t::try_from(child.id()) else {
        return ExitCode::from(127);
    };
    GROUP.store(pid, Ordering::SeqCst);
    release_stdio();
    // A thread waits for Claude Code to exit without reaping it, so its pid stays held.
    let exited = Arc::new(AtomicBool::new(false));
    {
        let exited = Arc::clone(&exited);
        thread::spawn(move || {
            wait_without_reaping(pid);
            exited.store(true, Ordering::SeqCst);
        });
    }
    while !exited.load(Ordering::SeqCst) {
        // SAFETY: getppid(2) can't fail.
        if unsafe { libc::getppid() } != parent {
            // rlxd is gone: end the run's group now. Claude Code is not reaped, so the
            // group is still the run's.
            kill_group();
        }
        thread::sleep(PARENT_CHECK);
    }
    // Claude Code has exited and is not reaped: its group is still the run's alone.
    kill_group();
    GROUP.store(0, Ordering::SeqCst);
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: `status` is a writable int; `pid` is this process's child.
        let reaped = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        if reaped == pid {
            break;
        }
        if reaped == -1 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return ExitCode::from(1);
        }
    }
    pass_on(status)
}

/// Waits until `pid` has exited, leaving it unreaped (`WNOWAIT`).
fn wait_without_reaping(pid: libc::pid_t) {
    let Ok(id) = libc::id_t::try_from(pid) else {
        return;
    };
    loop {
        // SAFETY: siginfo_t is plain data, for which zero is a valid value.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is writable; waitid(2) with WNOWAIT leaves the child unreaped.
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                id,
                &raw mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if waited == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return;
        }
    }
}

/// Kills the run's group, while it is the run's.
fn kill_group() {
    let group = GROUP.load(Ordering::SeqCst);
    if group > 0 {
        // SAFETY: killpg(2) on the run's group, whose id its unreaped leader holds.
        unsafe { libc::killpg(group, libc::SIGKILL) };
    }
}

/// On `SIGTERM` or `SIGHUP`: kill the run's group, then exit.
extern "C" fn on_signal(signal: libc::c_int) {
    kill_group();
    // SAFETY: _exit(2) is async-signal-safe.
    unsafe { libc::_exit(128 + signal) };
}

fn install_handlers() {
    for signal in [libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: on_signal calls only async-signal-safe functions (an atomic load,
        // killpg, _exit).
        unsafe { libc::signal(signal, on_signal as *const () as libc::sighandler_t) };
    }
}

/// Points the guard's own stdin, stdout and stderr at `/dev/null`: Claude Code holds the
/// run's pipes, and they end when it does.
fn release_stdio() {
    // SAFETY: opening /dev/null, then dup2(2) onto 0-2 and closing the spare.
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null < 0 {
            return;
        }
        for fd in 0..=2 {
            libc::dup2(null, fd);
        }
        if null > 2 {
            libc::close(null);
        }
    }
}

/// Exits as Claude Code did: with its code, or of its signal.
fn pass_on(status: libc::c_int) -> ExitCode {
    if libc::WIFSIGNALED(status) {
        let signal = libc::WTERMSIG(status);
        // SAFETY: back to the default action for `signal`, then raised on this process,
        // which then ends as Claude Code did.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
        return ExitCode::from(u8::try_from(128 + signal).unwrap_or(255));
    }
    ExitCode::from(u8::try_from(libc::WEXITSTATUS(status)).unwrap_or(1))
}
