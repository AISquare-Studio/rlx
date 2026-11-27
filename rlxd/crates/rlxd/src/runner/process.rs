//! A run's process tree (handoff 0.10.3, 2.1 and 5.1): started so that it can be killed
//! whole, and so that it never outlives rlxd.
//!
//! - **Its own group, made before it runs.** On macOS and Linux the run runs under a
//!   guard, `rlxd __run-guard` ([`super::guard`]), which leads a new session and so a new
//!   process group: Claude Code runs in it, and so does what Claude Code starts. On Windows Claude Code
//!   is started suspended, put in a Job Object of its own, and only then let run, so
//!   nothing of it runs outside the job.
//! - **Killing the tree.** A cancel or a timeout kills the whole group (`SIGKILL`), or
//!   terminates the whole job. The group's id is the guard's pid, which rlxd holds until
//!   it reaps the guard: it never signals the group after that ([`Tree::reap`]).
//! - **When the guard exits,** however it exits, rlxd kills its group at once, before it
//!   reaps the guard: a thread per run waits for the guard's exit without reaping it
//!   (`waitid` with `WNOWAIT`). So Claude Code never outlives a guard that was killed alone.
//! - **When rlxd dies,** the tree goes with it:
//!   - Windows: the job has `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, and rlxd holds its only
//!     handle, which the OS closes when rlxd exits, however it exits.
//!   - macOS and Linux: the guard sees rlxd go (its parent is no longer the rlxd that
//!     started it; it checks every 100 ms) and kills its group. On Linux it also has
//!     `PR_SET_PDEATHSIG = SIGTERM`, which it catches to do the same at once, and Claude
//!     Code has `PR_SET_PDEATHSIG = SIGKILL` from the guard. That signal comes when the
//!     thread that started the process exits, not the process, so every run is started
//!     from one thread that lives as long as rlxd ([`Spawner`]).
//!   - When rlxd and the guard die together (`killall -9 rlxd` kills both, since the
//!     guard is an rlxd), rlxd's next start in the same boot sweeps the run's group
//!     ([`sweep`]) by the recorded guard pid and start time: the whole group if the guard
//!     still runs with that start time, nothing if another process has that pid, and
//!     otherwise, once one of them carries the run's id ([`RUN_ID_ENV`]), each process of
//!     the guard's group and session that started at or after the run. macOS hides an
//!     Apple binary's environment, so a run whose leftovers are all Apple's is not
//!     swept: nothing is killed that can't be told for the run's.
//!   - Everywhere, the run's stdin closes with rlxd, and Claude Code ends its turn and
//!     exits on it; that is a courtesy, not what the guarantee rests on.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use tokio::process::{Child, Command};
use tokio::sync::oneshot;

/// The variable every process of a run inherits, set to its run's id: how rlxd's start
/// sweep knows a process for the run's, when nothing else is left of the run to go by.
pub const RUN_ID_ENV: &str = "RLXD_RUN_ID";

/// What starts a run's process: its program, arguments, folder and environment.
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
}

/// A started run's process tree, to kill. The leader's [`Child`] is apart from it, so
/// one task can wait on the leader while another kills the tree.
pub struct Tree {
    /// rlxd's child: on macOS and Linux the run's guard ([`super::guard`]), on Windows
    /// Claude Code.
    pub pid: u32,
    /// When it started, as [`start_time`] says, for the sweep at rlxd's next start.
    pub started: Option<String>,
    /// Whether rlxd's child is reaped, by rlxd or (never meant to happen) anything else:
    /// from then on its pid may be another process's, so it is never signalled again
    /// ([`Tree::reap`]).
    reaped: Arc<Mutex<bool>>,
    /// Set once the guard has exited and its group was killed, before it is reaped.
    #[cfg(unix)]
    exited: Arc<std::sync::atomic::AtomicBool>,
    /// The read end of the guard's pipe for how Claude Code ended, non-blocking.
    #[cfg(unix)]
    status: std::os::fd::OwnedFd,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl Tree {
    /// Kills every process of the tree at once: on macOS and Linux the run's whole process
    /// group, guard and Claude Code and what it started; on Windows the whole job. Never
    /// once rlxd has reaped its child, whose pid is the group's id.
    pub fn kill(&self) {
        let reaped = self.reaped.lock().unwrap_or_else(|p| p.into_inner());
        if !*reaped {
            kill_tree(self);
        }
    }

    /// Reaps `child`, this tree's, once it has exited, waiting at most `wait` (forever
    /// when `None`): its exit status, or `None` when it still runs. A kill and a reap
    /// never cross: the reap holds the lock [`Tree::kill`] takes, so no signal goes to a
    /// pid after it was reaped.
    pub async fn reap(
        &self,
        child: &mut Child,
        wait: Option<std::time::Duration>,
    ) -> Option<std::process::ExitStatus> {
        let deadline = wait.map(|wait| tokio::time::Instant::now() + wait);
        loop {
            // On macOS and Linux, only once the guard's exit was seen and its group
            // killed: a reap before that would free the group's id.
            #[cfg(unix)]
            let ready = self.exited.load(std::sync::atomic::Ordering::SeqCst);
            #[cfg(not(unix))]
            let ready = true;
            if ready {
                let mut reaped = self.reaped.lock().unwrap_or_else(|p| p.into_inner());
                match child.try_wait() {
                    Ok(Some(status)) => {
                        *reaped = true;
                        return Some(status);
                    }
                    Ok(None) => {}
                    Err(_) => {
                        *reaped = true;
                        return None;
                    }
                }
            }
            if deadline.is_some_and(|at| tokio::time::Instant::now() >= at) {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// How Claude Code ended, as its guard wrote it once it had (`exit status: 1`,
    /// `signal: 6`): `None` when the guard wrote nothing, being killed first. Read once the
    /// guard has exited: the pipe never blocks, and holds a few bytes at most.
    #[cfg(unix)]
    #[allow(unsafe_code)]
    pub fn ended_how(&self) -> Option<String> {
        use std::os::fd::AsRawFd;
        let mut bytes = [0_u8; 64];
        let mut len = 0;
        while len < bytes.len() {
            let rest = &mut bytes[len..];
            // SAFETY: read(2) into `rest`, writable for its length, from the pipe's
            // non-blocking read end.
            let read = unsafe {
                libc::read(
                    self.status.as_raw_fd(),
                    rest.as_mut_ptr().cast(),
                    rest.len(),
                )
            };
            match usize::try_from(read) {
                Ok(0) => break,
                Ok(n) => len += n,
                Err(_) if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        super::guard::read_status(std::str::from_utf8(&bytes[..len]).ok()?)
    }
}

/// Starts runs' processes from one thread that lives as long as rlxd: on Linux a child's
/// `PR_SET_PDEATHSIG` fires when the thread that started it exits, so no run may be
/// started from a thread that ends sooner. Starting them one at a time also keeps one
/// run's pipes from leaking into another's child where the OS makes pipes
/// close-on-exec only after it has made them (macOS).
pub struct Spawner {
    requests: mpsc::Sender<Request>,
}

/// A launch, and where its tree goes once started.
type Request = (Launch, oneshot::Sender<io::Result<(Child, Tree)>>);

impl Spawner {
    /// The spawner, on its own thread, inside the current tokio runtime.
    pub fn start() -> Self {
        let (requests, waiting) = mpsc::channel::<Request>();
        let runtime = tokio::runtime::Handle::current();
        thread::Builder::new()
            .name("rlxd-runs".to_owned())
            .spawn(move || {
                let _inside = runtime.enter();
                for (launch, reply) in waiting {
                    let _ = reply.send(spawn(launch));
                }
            })
            .expect("the run spawner's thread starts");
        Self { requests }
    }

    /// Starts `launch` as a tree of its own: its leader, and the tree.
    pub async fn spawn(&self, launch: Launch) -> io::Result<(Child, Tree)> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send((launch, reply))
            .map_err(|_| io::Error::other("the run spawner has stopped"))?;
        answer
            .await
            .map_err(|_| io::Error::other("the run spawner has stopped"))?
    }
}

/// A command for `launch`, its stdio piped and its child killed if rlxd drops it.
fn command(launch: Launch) -> Command {
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .current_dir(&launch.cwd)
        .envs(launch.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Not kill_on_drop: a kill of the guard alone would leave Claude Code behind. A
    // dropped run's guard sees rlxd go, or rlxd's shutdown kills its group.
    command
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn spawn(launch: Launch) -> io::Result<(Child, Tree)> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    // Claude Code runs under the guard:
    // `rlxd __run-guard --parent <rlxd> --status-fd <fd> -- <claude> <args…>`. On Linux by
    // /proc/self/exe, which still names this rlxd after an upgrade replaced its file.
    let rlxd = if cfg!(target_os = "linux") {
        PathBuf::from("/proc/self/exe")
    } else {
        std::env::current_exe()?
    };
    // How Claude Code ended comes back over a pipe the guard inherits, past stdio's
    // numbers; rlxd's end never blocks.
    let (reader, writer) = io::pipe()?;
    let reader = OwnedFd::from(reader);
    let writer = OwnedFd::from(writer);
    // SAFETY: fcntl(2) F_DUPFD_CLOEXEC of an open descriptor, to one numbered 3 or more.
    let raw = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if raw == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` was just made by F_DUPFD_CLOEXEC and is owned by nothing else.
    let writer = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: fcntl(2) F_GETFL and F_SETFL on the read end, open.
    let nonblocking = unsafe {
        let flags = libc::fcntl(reader.as_raw_fd(), libc::F_GETFL);
        flags != -1
            && libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) != -1
    };
    if !nonblocking {
        return Err(io::Error::last_os_error());
    }
    let status_fd = writer.as_raw_fd();
    let mut args: Vec<OsString> = vec![
        "__run-guard".into(),
        "--parent".into(),
        std::process::id().to_string().into(),
        "--status-fd".into(),
        status_fd.to_string().into(),
        "--".into(),
        launch.program.into(),
    ];
    args.extend(launch.args);
    let launch = Launch {
        program: rlxd,
        args,
        cwd: launch.cwd,
        env: launch.env,
    };
    let mut command = command(launch);
    #[cfg(target_os = "linux")]
    let rlxd = libc::pid_t::try_from(std::process::id()).unwrap_or(0);
    // SAFETY: the closure runs in the child between fork and exec, and calls only
    // setsid(2), fcntl(2), prctl(2) and getppid(2), which are async-signal-safe, and
    // builds an io::Error from an errno without allocating.
    unsafe {
        command.pre_exec(move || {
            // The guard leads a new session, and so the run's process group: no terminal,
            // and a session id, the guard's pid, that the sweep checks.
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            // The guard keeps its end of the status pipe across exec.
            if libc::fcntl(status_fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            {
                // SIGTERM, which the guard catches to kill the run's group.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(io::Error::last_os_error());
                }
                // rlxd may have died between the fork and the prctl: then nothing will
                // send the signal, so don't run.
                if libc::getppid() != rlxd {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    // Only the guard holds the write end now: the pipe ends with it.
    drop(writer);
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("the run's process has no pid"))?;
    let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reaped = Arc::new(Mutex::new(false));
    {
        // The moment the guard exits, unreaped, its group is killed: whatever of the run
        // is left, even when the guard was killed alone.
        let exited = Arc::clone(&exited);
        let reaped = Arc::clone(&reaped);
        let watcher = std::thread::Builder::new()
            .name("rlxd-run-group".to_owned())
            .spawn(move || {
                let seen = wait_without_reaping(pid);
                let mut reaped = reaped.lock().unwrap_or_else(|p| p.into_inner());
                if seen {
                    if !*reaped {
                        kill_group(pid);
                    }
                } else {
                    // Not rlxd's child any more (something reaped it): its pid, the
                    // group's id, may be another's now, and is never signalled again.
                    *reaped = true;
                }
                exited.store(true, std::sync::atomic::Ordering::SeqCst);
            });
        if let Err(e) = watcher {
            // Without the watcher, the run's group can't be followed: end it now, while
            // the guard is unreaped and its id still the run's.
            kill_group(pid);
            return Err(e);
        }
    }
    Ok((
        child,
        Tree {
            pid,
            started: start_time(pid),
            reaped,
            exited,
            status: reader,
        },
    ))
}

/// Waits until rlxd's child `pid` has exited, leaving it unreaped (`WNOWAIT`), so its pid,
/// the run's group's id, stays held. Whether its exit was seen so: `false` when `pid` is
/// not rlxd's child to wait for (`ECHILD`: reaped already), whose id may then be another's.
#[cfg(unix)]
#[allow(unsafe_code)]
fn wait_without_reaping(pid: u32) -> bool {
    let id = libc::id_t::from(pid);
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
        if waited == 0 {
            // An exit only: macOS reports a stop too (SIGSTOP, SIGTSTP), which ends
            // nothing, though WSTOPPED wasn't asked for.
            if matches!(
                info.si_code,
                libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
            ) {
                return true;
            }
            thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return false;
        }
    }
}

/// Kills the process group `group` (`SIGKILL`).
#[cfg(unix)]
#[allow(unsafe_code)]
fn kill_group(group: u32) {
    if let Ok(group) = libc::pid_t::try_from(group) {
        // SAFETY: killpg(2); every caller holds the group's id: its leader, rlxd's child,
        // is not reaped yet.
        unsafe { libc::killpg(group, libc::SIGKILL) };
    }
}

#[cfg(unix)]
fn kill_tree(tree: &Tree) {
    // The guard is rlxd's child and not reaped (Tree::kill), so its pid, the group's id,
    // is still the run's.
    kill_group(tree.pid);
}

/// Ends what is left of a run whose guard `pid`, started at `started` (as [`start_time`]
/// said), an earlier rlxd recorded: rlxd's start sweep, for a run in flight when it died.
///
/// - The guard still runs with that start time: its whole group is killed.
/// - Another process now has that pid: the run's group is gone (no pid is given out
///   while a group of that id exists), and nothing is killed.
/// - No process has it: the guard died too, as `killall -9 rlxd` does, and the group may
///   outlive it, or be another's that took its id once it was empty. It is the run's
///   only when one of its processes is known for the run's: in session `pid` (the
///   guard's), started at or after `started`, and carrying `RLXD_RUN_ID=<run_id>`
///   ([`RUN_ID_ENV`]). A group's id can't be taken while a process of the group lives,
///   so then the group is the run's, and each of its processes in that session that
///   started at or after `started` is killed, one whose environment the OS hides
///   included (macOS hides an Apple binary's). Otherwise nothing is.
///
/// Only for a run of this boot ([`boot_id`]): across a reboot a pid, a group id and (on
/// Linux) a start time may be anyone's. Whether it killed anything.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn sweep(pid: u32, started: &str, run_id: &str) -> bool {
    let Ok(guard) = libc::pid_t::try_from(pid) else {
        return false;
    };
    match start_time(pid) {
        Some(now) if now == started && session_of(guard) == Some(guard) => {
            kill_group(pid);
            return true;
        }
        Some(_) => return false,
        None => {}
    }
    let Some(since) = start_key(started) else {
        return false;
    };
    let members: Vec<libc::pid_t> = group_members(pid)
        .into_iter()
        .filter(|member| {
            start_time(*member)
                .as_deref()
                .and_then(start_key)
                .is_some_and(|key| key >= since)
        })
        .filter_map(|member| libc::pid_t::try_from(member).ok())
        .filter(|member| session_of(*member) == Some(guard))
        .collect();
    let known = members
        .iter()
        .any(|member| u32::try_from(*member).is_ok_and(|member| carries(member, run_id)));
    if !known {
        return false;
    }
    let mut killed = false;
    for member in members {
        // SAFETY: kill(2) to a process of the run's group and session that started with
        // or after the run, the group known for the run's by one of them.
        killed |= unsafe { libc::kill(member, libc::SIGKILL) } == 0;
    }
    killed
}

/// Whether process `pid`'s environment, as it started, holds `RLXD_RUN_ID=<run_id>`: from
/// `/proc/<pid>/environ` on Linux, `KERN_PROCARGS2` on macOS (rlxd's own user's processes
/// only, as the run's are).
#[cfg(unix)]
fn carries(pid: u32, run_id: &str) -> bool {
    let wanted = format!("{RUN_ID_ENV}={run_id}");
    environment(pid).is_some_and(|env| {
        env.split(|&b| b == 0)
            .any(|entry| entry == wanted.as_bytes())
    })
}

/// Process `pid`'s environment as it started, its entries NUL-separated.
#[cfg(target_os = "linux")]
fn environment(pid: u32) -> Option<Vec<u8>> {
    std::fs::read(format!("/proc/{pid}/environ")).ok()
}

/// Process `pid`'s arguments and environment as it started (`KERN_PROCARGS2`: its argument
/// count, its executable's path, then its arguments and environment, NUL-separated), the
/// count and path left out. What follows the environment is not one of its entries.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn environment(pid: u32) -> Option<Vec<u8>> {
    let mut argmax: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: sysctl(3) of kern.argmax, an int, into `argmax` of `size` bytes.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&raw mut argmax).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    let mut buffer = vec![0_u8; usize::try_from(argmax).ok()?];
    let mut size = buffer.len();
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROCARGS2,
        libc::c_int::try_from(pid).ok()?,
    ];
    // SAFETY: sysctl(3) of the process's arguments into `buffer`, writable for `size`
    // bytes; it says how many it wrote.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    buffer.truncate(size);
    let count = buffer.get(..4)?;
    let argc = usize::try_from(i32::from_ne_bytes(count.try_into().ok()?)).ok()?;
    // The executable's path, then the NULs that pad it.
    let mut rest = &buffer[4..];
    let path_end = rest.iter().position(|&b| b == 0)?;
    rest = &rest[path_end..];
    let start = rest.iter().position(|&b| b != 0)?;
    rest = &rest[start..];
    // The arguments.
    for _ in 0..argc {
        let end = rest.iter().position(|&b| b == 0)?;
        rest = rest.get(end + 1..)?;
    }
    Some(rest.to_vec())
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn environment(_pid: u32) -> Option<Vec<u8>> {
    None
}

/// Raises the soft limit on open files to the hard one (capped at 65,536, and on macOS at
/// `kern.maxfilesperproc`): an erase holds one folder open per level it is down
/// ([`super::remove::MAX_DEPTH`]), and launchd starts a daemon with 256. The limit it has
/// then, or `None` when it could not be read.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn raise_fd_limit() -> Option<libc::rlim_t> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) into a writable rlimit.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } != 0 {
        return None;
    }
    let target = limit.rlim_max.min(65_536);
    #[cfg(target_os = "macos")]
    let target = {
        let mut target = target;
        let mut most: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        // SAFETY: kern.maxfilesperproc is an int, which `most` is, of `size` bytes.
        let read = unsafe {
            libc::sysctlbyname(
                c"kern.maxfilesperproc".as_ptr(),
                (&raw mut most).cast(),
                &raw mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if read == 0
            && let Ok(most) = libc::rlim_t::try_from(most)
            && most > 0
        {
            target = target.min(most);
        }
        target
    };
    if target <= limit.rlim_cur {
        return Some(limit.rlim_cur);
    }
    let raised = libc::rlimit {
        rlim_cur: target,
        rlim_max: limit.rlim_max,
    };
    // SAFETY: setrlimit(2) of the soft limit, no higher than the hard one.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const raised) } == 0 {
        Some(target)
    } else {
        Some(limit.rlim_cur)
    }
}

/// Process `pid`'s session id (`getsid`), or `None` when there is no such process.
#[cfg(unix)]
#[allow(unsafe_code)]
fn session_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    // SAFETY: getsid(2) of a pid; a gone one fails with ESRCH.
    let session = unsafe { libc::getsid(pid) };
    (session != -1).then_some(session)
}

/// A [`start_time`] as a pair that orders by time: `sec.usec` on macOS, clock ticks on
/// Linux.
#[cfg(unix)]
fn start_key(started: &str) -> Option<(u64, u64)> {
    match started.split_once('.') {
        Some((sec, usec)) => Some((sec.parse().ok()?, usec.parse().ok()?)),
        None => Some((started.parse().ok()?, 0)),
    }
}

/// The processes in process group `group`.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn group_members(group: u32) -> Vec<u32> {
    let Ok(group) = libc::pid_t::try_from(group) else {
        return Vec::new();
    };
    let mut pids = vec![0 as libc::pid_t; 4096];
    let size = libc::c_int::try_from(pids.len() * std::mem::size_of::<libc::pid_t>())
        .unwrap_or(libc::c_int::MAX);
    // SAFETY: `pids` is writable for `size` bytes; proc_listpgrppids fills pids and
    // returns how many.
    let count = unsafe { libc::proc_listpgrppids(group, pids.as_mut_ptr().cast(), size) };
    let count = usize::try_from(count).unwrap_or(0).min(pids.len());
    pids[..count]
        .iter()
        .filter_map(|pid| u32::try_from(*pid).ok())
        .filter(|pid| *pid != 0)
        .collect()
}

/// The processes in process group `group`: `/proc/<pid>/stat`'s fifth field.
#[cfg(target_os = "linux")]
fn group_members(group: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                stat.rfind(')').is_some_and(|at| {
                    stat[at + 1..].split_whitespace().nth(2) == Some(&group.to_string())
                })
            })
        })
        .collect()
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn group_members(_group: u32) -> Vec<u32> {
    Vec::new()
}

/// When process `pid` started, as a token two reads of the same process agree on and
/// another process with that pid would not: `proc_pidinfo`'s start time on macOS.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn start_time(pid: u32) -> Option<String> {
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: proc_bsdinfo is plain integers and byte arrays, for which zero is valid.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is a writable proc_bsdinfo of `size` bytes, as PROC_PIDTBSDINFO
    // fills.
    let written =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (written == size).then(|| format!("{}.{:06}", info.pbi_start_tvsec, info.pbi_start_tvusec))
}

/// `/proc/<pid>/stat`'s `starttime`, in clock ticks since boot, on Linux.
#[cfg(target_os = "linux")]
pub fn start_time(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name, field 2, is in parentheses and may hold spaces: count from the
    // last ')'. starttime is field 22, the 20th after it.
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(19).map(str::to_owned)
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
pub fn start_time(_pid: u32) -> Option<String> {
    None
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn spawn(launch: Launch) -> io::Result<(Child, Tree)> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};

    // SAFETY: no security attributes and no name: a new, unnamed job.
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a job handle just made, owned by nothing else.
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let size = u32::try_from(std::mem::size_of_val(&limits)).unwrap_or(u32::MAX);
    // SAFETY: `limits` is the extended limit information the class names, of `size`.
    let set = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size,
        )
    };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut command = command(launch);
    // Suspended until it is in the job; no console window of its own.
    command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
    let mut child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("the run's process has no pid"))?;
    let placed = child.raw_handle().is_some_and(|process| {
        // SAFETY: both handles are open: the job's, and the child's process handle.
        unsafe { AssignProcessToJobObject(job.as_raw_handle(), process) != 0 }
    });
    if !placed || !resume_first_thread(pid) {
        let error = io::Error::last_os_error();
        let _ = child.start_kill();
        return Err(io::Error::other(format!(
            "the run's process could not be put in its job and started: {error}"
        )));
    }
    Ok((
        child,
        Tree {
            pid,
            started: None,
            reaped: Arc::new(Mutex::new(false)),
            job,
        },
    ))
}

/// Resumes the one thread of the suspended process `pid`. Whether it did.
#[cfg(windows)]
#[allow(unsafe_code)]
fn resume_first_thread(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    // SAFETY: a snapshot of every thread on the system; closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return false;
    }
    let mut entry = THREADENTRY32 {
        dwSize: u32::try_from(std::mem::size_of::<THREADENTRY32>()).unwrap_or(0),
        ..THREADENTRY32::default()
    };
    let mut resumed = false;
    // SAFETY: `entry` is a THREADENTRY32 whose dwSize says so, as both calls need.
    let mut more = unsafe { Thread32First(snapshot, &raw mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: opening a thread by id for resuming only; closed below.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if !thread.is_null() {
                // SAFETY: `thread` is open with THREAD_SUSPEND_RESUME.
                resumed = unsafe { ResumeThread(thread) } != u32::MAX;
                // SAFETY: closing the handle just opened.
                unsafe { CloseHandle(thread) };
            }
            break;
        }
        // SAFETY: as Thread32First.
        more = unsafe { Thread32Next(snapshot, &raw mut entry) } != 0;
    }
    // SAFETY: closing the snapshot made above.
    unsafe { CloseHandle(snapshot) };
    resumed
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn kill_tree(tree: &Tree) {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::System::JobObjects::TerminateJobObject;
    // SAFETY: the job's handle is open for as long as the tree is.
    unsafe { TerminateJobObject(tree.job.as_raw_handle(), 1) };
}

/// Windows needs no sweep: a job whose last handle closes kills its processes.
#[cfg(windows)]
pub fn sweep(_pid: u32, _started: &str, _run_id: &str) -> bool {
    false
}

/// This boot of the OS, as a token another boot would not give: Linux's
/// `/proc/sys/kernel/random/boot_id`, macOS's `kern.bootsessionuuid` (made at each boot;
/// `kern.boottime` moves when the clock is set), and on Windows the boot time from the
/// tick count, to the second, marked `~` since it may drift by one.
#[cfg(target_os = "linux")]
pub fn boot_id() -> Option<String> {
    let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(id.trim().to_owned()).filter(|id| !id.is_empty())
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn boot_id() -> Option<String> {
    let mut uuid = [0_u8; 64];
    let mut size = uuid.len();
    // SAFETY: kern.bootsessionuuid is a NUL-ended string, which `uuid` holds `size` bytes
    // of; sysctlbyname(3) writes at most that and says how many it wrote.
    let read = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            uuid.as_mut_ptr().cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    let text = std::ffi::CStr::from_bytes_until_nul(uuid.get(..size)?).ok()?;
    Some(text.to_str().ok()?.to_owned()).filter(|id| !id.is_empty())
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub fn boot_id() -> Option<String> {
    use windows_sys::Win32::System::SystemInformation::GetTickCount64;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis();
    // SAFETY: GetTickCount64 takes nothing and only reads a counter.
    let up = u128::from(unsafe { GetTickCount64() });
    Some(format!("~{}", now.saturating_sub(up) / 1000))
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
pub fn boot_id() -> Option<String> {
    None
}

/// Whether two [`boot_id`]s name one boot: the same id, or on Windows boot times at
/// most two seconds apart.
pub fn same_boot(a: &str, b: &str) -> bool {
    match (a.strip_prefix('~'), b.strip_prefix('~')) {
        (Some(a), Some(b)) => match (a.parse::<i64>(), b.parse::<i64>()) {
            (Ok(a), Ok(b)) => (a - b).abs() <= 2,
            _ => false,
        },
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn this_boot_is_one_boot_and_a_windows_boot_time_may_drift_a_second() {
        if let Some(id) = super::boot_id() {
            assert!(super::same_boot(&id, &id));
        }
        assert!(super::same_boot("~1000", "~1001"));
        assert!(!super::same_boot("~1000", "~1010"));
        assert!(!super::same_boot("a", "b"));
    }

    use super::*;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_process_start_time_is_stable_and_none_for_no_process() {
        let me = std::process::id();
        let first = start_time(me).expect("this process has a start time");
        assert_eq!(start_time(me).as_deref(), Some(first.as_str()));
        assert!(
            !sweep(me, "0.000000", "r"),
            "a start time that differs kills nothing"
        );
        assert_eq!(start_time(u32::try_from(i32::MAX).unwrap()), None);
    }

    /// Started by [`leaderless_group`] as a process of its group: this test binary,
    /// whose environment the OS shows (macOS hides an Apple binary's, `sleep`'s).
    #[test]
    #[ignore = "a process for the sweep's test to start"]
    fn sleeper() {
        if std::env::var_os("RLXD_TEST_SLEEPER").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
    }

    /// A shell that leads a new group (and with `new_session`, a new session) starts a
    /// `sleep` and a [`sleeper`] in it, `RLXD_RUN_ID=<run_id>` in their environment when
    /// given, and exits, reaped: a group without its leader, as a run's is after
    /// `killall -9 rlxd`. The shell's pid, the group's id, and the two others'.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[allow(unsafe_code)]
    fn leaderless_group(new_session: bool, run_id: Option<&str>) -> (u32, [u32; 2]) {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args([
                "-c",
                "sleep 30 </dev/null >/dev/null 2>&1 & a=$!; \
                 \"$0\" --ignored --exact runner::process::tests::sleeper \
                 </dev/null >/dev/null 2>&1 & echo $a $!",
            ])
            .arg(std::env::current_exe().unwrap())
            .env_remove(RUN_ID_ENV)
            .env("RLXD_TEST_SLEEPER", "1")
            .stdout(Stdio::piped());
        if let Some(run_id) = run_id {
            command.env(RUN_ID_ENV, run_id);
        }
        if new_session {
            // SAFETY: setsid(2) in the child between fork and exec, async-signal-safe.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        } else {
            command.process_group(0);
        }
        let mut shell = command.spawn().unwrap();
        let mut line = String::new();
        io::BufReader::new(shell.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        shell.wait().unwrap();
        let pids: Vec<u32> = line
            .split_whitespace()
            .map(|pid| pid.parse().unwrap())
            .collect();
        // Each is the shell's fork until it has run its program.
        let ran = std::time::Instant::now() + std::time::Duration::from_secs(10);
        for pid in &pids {
            while command_of(*pid).ends_with("sh") {
                assert!(
                    std::time::Instant::now() < ran,
                    "{pid} never ran its program"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        (shell.id(), [pids[0], pids[1]])
    }

    /// The program `pid` runs, as `ps` names it.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn command_of(pid: u32) -> String {
        let ps = std::process::Command::new("ps")
            .args(["-o", "comm=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&ps.stdout).trim().to_owned()
    }

    /// Whether `pid` runs, a zombie not counted.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn runs(pid: u32) -> bool {
        let ps = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&ps.stdout);
        !stat.trim().is_empty() && !stat.trim().starts_with('Z')
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[allow(unsafe_code)]
    #[test]
    fn the_sweep_kills_a_leaderless_group_only_of_the_guards_session_and_the_runs_id() {
        /// Kills the processes it holds when the test ends, passed or failed.
        struct Started(Vec<u32>);
        impl Drop for Started {
            fn drop(&mut self) {
                for pid in &self.0 {
                    // SAFETY: kill(2) of a process this test started.
                    unsafe { libc::kill(libc::pid_t::try_from(*pid).unwrap(), libc::SIGKILL) };
                }
            }
        }
        let before = start_time(std::process::id()).unwrap();
        let mut started = Started(Vec::new());
        // Groups whose leaders are gone, whose processes started after `before`: a pid's
        // later groups, not the run's. Each stays.
        for (new_session, run_id, what) in [
            (false, Some("r1"), "in another session (this test's)"),
            (true, None, "without the run's id"),
            (true, Some("r2"), "with another run's id"),
        ] {
            let (group, both) = leaderless_group(new_session, run_id);
            started.0.extend(both);
            assert!(!sweep(group, &before, "r1"), "a group {what} is left alone");
            for pid in both {
                assert!(runs(pid), "a group {what}: its processes run on");
            }
        }
        // The run's: the group leads its own session, as the guard's does, and a process
        // of it carries the run's id, so the sleep goes too, whose environment macOS
        // hides.
        let (group, both) = leaderless_group(true, Some("r1"));
        started.0.extend(both);
        // Recorded as started after them, it is not the run's: nothing is killed.
        // (A cat that waits on its stdin, so its start time can be read before it exits.)
        let mut later = std::process::Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let after = start_time(later.id()).unwrap();
        drop(later.stdin.take());
        later.wait().unwrap();
        assert!(
            start_key(&after) > start_key(&before),
            "{after} after {before}"
        );
        assert!(
            !sweep(group, &after, "r1"),
            "processes started before the run are not the run's"
        );
        for pid in both {
            assert!(runs(pid), "started before the run: runs on");
        }
        assert!(
            sweep(group, &before, "r1"),
            "the guard's group and session, with the run's id, are swept"
        );
        let gone = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while both.iter().any(|pid| runs(*pid)) {
            assert!(
                std::time::Instant::now() < gone,
                "the swept sleep still runs"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// On Windows a run's tree is a job, which this starts directly. (On macOS and Linux a
    /// run starts under rlxd's guard, which only the rlxd binary has: tests/runner.rs.)
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_killed_tree_ends_its_leader() {
        let spawner = Spawner::start();
        let (mut child, tree) = spawner
            .spawn(Launch {
                program: "cmd".into(),
                args: vec!["/c".into(), "ping -n 30 127.0.0.1 > NUL".into()],
                cwd: std::env::temp_dir(),
                env: Vec::new(),
            })
            .await
            .unwrap();
        tree.kill();
        let status = tree
            .reap(&mut child, Some(std::time::Duration::from_secs(10)))
            .await
            .expect("the leader ends once killed");
        assert!(!status.success());
    }
}
