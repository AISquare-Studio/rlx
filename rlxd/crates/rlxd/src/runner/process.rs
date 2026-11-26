//! A run's process tree (handoff 0.10.3, 2.1 and 5.1): started so that it can be killed
//! whole, and so that it never outlives rlxd.
//!
//! - **Its own group, made before it runs.** On macOS and Linux the run's Claude Code runs
//!   under a guard, `rlxd __run-guard` ([`super::guard`]): Claude Code leads a process
//!   group of its own, which what it starts joins, and the guard ends that group when
//!   Claude Code exits, when rlxd dies, or when rlxd tells it to. On Windows Claude Code
//!   is started suspended, put in a Job Object of its own, and only then let run, so
//!   nothing of it runs outside the job.
//! - **Killing the tree.** A cancel or a timeout sends the guard `SIGTERM`, and it kills
//!   the whole group, or terminates the whole job. rlxd never signals its child after it
//!   has reaped it ([`Tree::reap`]), and the guard never kills the group after it has
//!   reaped Claude Code, so no kill can reach a process whose pid was given out again.
//! - **When rlxd dies,** the tree goes with it:
//!   - Windows: the job has `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, and rlxd holds its only
//!     handle, which the OS closes when rlxd exits, however it exits.
//!   - macOS and Linux: the guard sees rlxd go (its parent changes; it checks every
//!     100 ms) and kills the run's group. On Linux it also has `PR_SET_PDEATHSIG =
//!     SIGTERM`, which it catches to do the same at once. That signal comes when the
//!     thread that started the guard exits, not the process, so every run is started
//!     from one thread that lives as long as rlxd ([`Spawner`]); the guard checks, after
//!     setting it, that rlxd had not already gone.
//!   - Should the guard itself be killed with rlxd, rlxd records each guard's pid and the
//!     time it started ([`start_time`]) in the run's record, and when it starts again it
//!     sends `SIGTERM` to a recorded guard that still runs with that same start time, so a
//!     pid the OS has given to another process since is never signalled. Survivors of a
//!     guard killed by something else, with no live leader, are left: nothing proves them
//!     the run's.
//!   - Everywhere, the run's stdin closes with rlxd, and Claude Code ends its turn and
//!     exits on it; that is a courtesy, not what the guarantee rests on.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;

use tokio::process::{Child, Command};
use tokio::sync::oneshot;

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
    /// Whether rlxd has reaped its child: from then on its pid may be another process's,
    /// so it is never signalled again ([`Tree::reap`]).
    reaped: std::sync::Mutex<bool>,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl Tree {
    /// Kills every process of the tree at once: on macOS and Linux, `SIGTERM` to the run's
    /// guard, which kills Claude Code's group and exits; on Windows, the whole job. Never
    /// once rlxd has reaped its child.
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
            {
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
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn spawn(launch: Launch) -> io::Result<(Child, Tree)> {
    // Claude Code runs under the guard, `rlxd __run-guard -- <claude> <args…>`.
    let rlxd = std::env::current_exe()?;
    let mut args: Vec<OsString> = vec!["__run-guard".into(), "--".into(), launch.program.into()];
    args.extend(launch.args);
    let launch = Launch {
        program: rlxd,
        args,
        cwd: launch.cwd,
        env: launch.env,
    };
    let mut command = command(launch);
    command.process_group(0);
    #[cfg(target_os = "linux")]
    {
        let rlxd = libc::pid_t::try_from(std::process::id()).unwrap_or(0);
        // SAFETY: the closure runs in the child between fork and exec, and calls only
        // prctl(2) and getppid(2), which are async-signal-safe, and builds an io::Error
        // from an errno without allocating.
        unsafe {
            command.pre_exec(move || {
                // SIGTERM, which the guard catches to kill the run's group first.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(io::Error::last_os_error());
                }
                // rlxd may have died between the fork and the prctl: then nothing will
                // send the signal, so don't run.
                if libc::getppid() != rlxd {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }
    let child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("the run's process has no pid"))?;
    Ok((
        child,
        Tree {
            pid,
            started: start_time(pid),
            reaped: std::sync::Mutex::new(false),
        },
    ))
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn kill_tree(tree: &Tree) {
    let Ok(guard) = libc::pid_t::try_from(tree.pid) else {
        return;
    };
    // SAFETY: kill(2) to rlxd's own child, not yet reaped (Tree::kill), so the pid is
    // still the guard's; it kills Claude Code's group and exits.
    unsafe { libc::kill(guard, libc::SIGTERM) };
}

/// Ends the run of guard `pid`, left by an earlier rlxd, if `pid` still runs and started
/// at `started`, as [`start_time`] said when it was recorded: a pid given to another
/// process since is left alone. The guard, sent `SIGTERM`, kills its run's group and
/// exits. Whether it sent one.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn kill_recorded(pid: u32, started: &str) -> bool {
    if start_time(pid).as_deref() != Some(started) {
        return false;
    }
    let Ok(guard) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: kill(2); the start time matched, so the process is the run's guard.
    unsafe { libc::kill(guard, libc::SIGTERM) == 0 }
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
            reaped: std::sync::Mutex::new(false),
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
pub fn kill_recorded(_pid: u32, _started: &str) -> bool {
    false
}

/// This boot of the OS, as a token another boot would not give: Linux's
/// `/proc/sys/kernel/random/boot_id`, macOS's `kern.boottime`, and on Windows the boot time
/// from the tick count, to the second, marked `~` since it may drift by one.
#[cfg(target_os = "linux")]
pub fn boot_id() -> Option<String> {
    let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(id.trim().to_owned()).filter(|id| !id.is_empty())
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn boot_id() -> Option<String> {
    let mut boot = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut size = std::mem::size_of::<libc::timeval>();
    // SAFETY: kern.boottime is a struct timeval, which `boot` is, of `size` bytes.
    let read = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&raw mut boot).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (read == 0).then(|| format!("{}.{:06}", boot.tv_sec, boot.tv_usec))
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
            !kill_recorded(me, "0.000000"),
            "a start time that differs kills nothing"
        );
        assert_eq!(start_time(u32::try_from(i32::MAX).unwrap()), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_killed_tree_ends_its_leader() {
        let (program, args): (PathBuf, Vec<OsString>) = if cfg!(windows) {
            (
                "cmd".into(),
                vec!["/c".into(), "ping -n 30 127.0.0.1 > NUL".into()],
            )
        } else {
            ("sh".into(), vec!["-c".into(), "sleep 30".into()])
        };
        let spawner = Spawner::start();
        let (mut child, tree) = spawner
            .spawn(Launch {
                program,
                args,
                cwd: std::env::temp_dir(),
                env: Vec::new(),
            })
            .await
            .unwrap();
        tree.kill();
        let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
            .await
            .expect("the leader ends once killed")
            .unwrap();
        assert!(!status.success());
    }
}
