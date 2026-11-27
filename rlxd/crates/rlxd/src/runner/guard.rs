//! `rlxd __run-guard --parent <pid> --status <file> -- <claude> <args…>`: what each run's
//! Claude Code is started under on macOS and Linux (handoff 0.10.3, 2.1 and 5.1), so that
//! every process of the run ends with it, and no process that isn't the run's is ever
//! killed.
//!
//! - **One group for the run.** rlxd starts the guard as the leader of a new process
//!   group, and the guard starts Claude Code in that same group, with its stdio as the
//!   guard got it, so the run's pipes are Claude Code's own. The guard then points its own
//!   stdin, stdout and stderr at `/dev/null`, so the pipes end when Claude Code's do. What
//!   Claude Code starts joins the group too.
//! - **The group's id is the guard's pid,** which no other process or group can take
//!   while the guard is alive or unreaped, nor while anything is left in the group. So
//!   rlxd can always kill the run's whole group, even after the guard is gone: rlxd
//!   kills it as soon as it sees the guard exit, before it reaps the guard.
//! - **When Claude Code exits,** the guard writes how it ended to the status file (`exit
//!   <code>` or `signal <n>`), then kills its whole group, itself included: what Claude
//!   Code left running, such as MCP servers or a tool's background process, goes with it.
//!   rlxd reads the file for `run.failed`'s message, so the guard never re-raises a
//!   signal, and the OS never blames rlxd for a crash of Claude Code's.
//! - **When rlxd dies,** the guard kills its group. It checks every 100 ms that its
//!   parent is still the rlxd that started it (`--parent`, the pid rlxd passed, so a parent
//!   gone before the guard ran is seen too), and on `SIGTERM` or `SIGHUP` it does the same
//!   at once: on Linux its parent-death signal is `SIGTERM`. On Linux Claude Code's own
//!   parent-death signal is `SIGKILL` from the guard, so it dies with the guard too.
//!
//! It is a hidden subcommand of rlxd, absent from `--help`, and it only ever kills its own
//! group.
#![allow(unsafe_code)]

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

/// How often the guard checks that rlxd still runs.
const PARENT_CHECK: Duration = Duration::from_millis(100);

/// What the guard was told: rlxd's pid, the status file, and what to run.
struct Args {
    parent: libc::pid_t,
    status: PathBuf,
    program: OsString,
    args: Vec<OsString>,
}

/// `rlxd __run-guard --parent <pid> --status <file> -- <program> <args…>`.
pub fn main(argv: &[OsString]) -> ExitCode {
    // Kill the group on SIGTERM or SIGHUP from the start: it is the guard's own, whatever
    // has been started in it yet.
    install_handlers();
    let Some(args) = parse(argv) else {
        eprintln!("usage: rlxd __run-guard --parent <pid> --status <file> -- <program> [args...]");
        return ExitCode::from(2);
    };
    // rlxd may already be gone.
    if !parent_is(args.parent) {
        kill_group();
    }
    let mut command = std::process::Command::new(&args.program);
    command.args(&args.args);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: getpid(2) can't fail.
        let guard = unsafe { libc::getpid() };
        // SAFETY: the closure runs in the child between fork and exec, and calls only
        // prctl(2) and getppid(2), which are async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != guard {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }
    let child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!(
                "rlxd __run-guard: {} could not start: {e}",
                args.program.to_string_lossy()
            );
            let _ = std::fs::write(&args.status, "exit 127\n");
            return ExitCode::from(127);
        }
    };
    let Ok(pid) = libc::pid_t::try_from(child.id()) else {
        kill_group();
        return ExitCode::from(1);
    };
    release_stdio();
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: `status` is a writable int; `pid` is this process's child.
        let waited = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
        if waited == pid {
            break;
        }
        if waited == -1 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            kill_group();
            return ExitCode::from(1);
        }
        if !parent_is(args.parent) {
            // rlxd is gone: end the run.
            kill_group();
        }
        thread::sleep(PARENT_CHECK);
    }
    let ended = if libc::WIFSIGNALED(status) {
        format!("signal {}\n", libc::WTERMSIG(status))
    } else {
        format!("exit {}\n", libc::WEXITSTATUS(status))
    };
    let _ = std::fs::write(&args.status, ended);
    // What Claude Code left running ends with it, and so does the guard.
    kill_group();
    ExitCode::SUCCESS
}

/// The arguments, or `None` when they are not the guard's.
fn parse(argv: &[OsString]) -> Option<Args> {
    let mut parent = None;
    let mut status = None;
    let mut rest = argv.iter();
    loop {
        match rest.next()?.to_str() {
            Some("--parent") => parent = rest.next()?.to_str()?.parse().ok(),
            Some("--status") => status = Some(PathBuf::from(rest.next()?)),
            Some("--") => break,
            _ => return None,
        }
    }
    let program = rest.next()?.clone();
    Some(Args {
        parent: parent?,
        status: status?,
        program,
        args: rest.cloned().collect(),
    })
}

/// Whether the guard's parent is still `parent`, the rlxd that started it.
fn parent_is(parent: libc::pid_t) -> bool {
    // SAFETY: getppid(2) can't fail.
    unsafe { libc::getppid() == parent }
}

/// Kills the guard's whole group, the guard included: the run's group, whose id is the
/// guard's own pid, so it can be no other.
fn kill_group() {
    // SAFETY: killpg(2) with 0, this process's own group.
    unsafe { libc::killpg(0, libc::SIGKILL) };
}

/// On `SIGTERM` or `SIGHUP`: kill the group.
extern "C" fn on_signal(_signal: libc::c_int) {
    kill_group();
}

fn install_handlers() {
    for signal in [libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: on_signal calls only killpg, which is async-signal-safe.
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

/// How Claude Code ended, as the guard wrote it to its status file: `"exit 1"` reads as
/// `exit status: 1`, `"signal 6"` as `signal: 6`. `None` when the file holds neither.
pub fn read_status(text: &str) -> Option<String> {
    let (how, n) = text.trim().split_once(' ')?;
    let n: i32 = n.parse().ok()?;
    match how {
        "exit" => Some(format!("exit status: {n}")),
        "signal" => Some(format!("signal: {n}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_file_reads_back_as_a_status() {
        assert_eq!(read_status("exit 1\n").as_deref(), Some("exit status: 1"));
        assert_eq!(read_status("signal 6").as_deref(), Some("signal: 6"));
        assert_eq!(read_status(""), None);
        assert_eq!(read_status("bogus 1"), None);
    }

    #[test]
    fn the_guard_takes_only_its_own_arguments() {
        let argv: Vec<OsString> = ["--parent", "42", "--status", "/s", "--", "claude", "-p"]
            .map(OsString::from)
            .to_vec();
        let args = parse(&argv).unwrap();
        assert_eq!(args.parent, 42);
        assert_eq!(args.status, PathBuf::from("/s"));
        assert_eq!(args.program, OsString::from("claude"));
        assert_eq!(args.args, [OsString::from("-p")]);
        assert!(
            parse(&["--", "claude"].map(OsString::from)).is_none(),
            "no parent"
        );
        assert!(parse(&["--parent", "1", "--status", "/s"].map(OsString::from)).is_none());
    }
}
