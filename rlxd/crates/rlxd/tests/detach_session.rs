//! `rlxd serve --detach` leaves whatever started it (task #38): a field report saw the
//! background tasks of an interactive Claude Code session killed at about 26 to 30
//! minutes, and everything they started with them, and agents and people start rlxd from
//! inside such sessions. On POSIX a detached rlxd leads a session of its own, so ending the
//! session and process group it was started from does not reach it. On Windows it breaks
//! away from the caller's Job Object, so closing a kill-on-close job (Node's, which Claude
//! Code runs on, is one) does not end it.

mod common;

use std::thread;
use std::time::{Duration, Instant};

use common::TestHome;

/// Waits until this home's rlxd answers `status`: its pid.
fn answering(home: &TestHome) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let run = home.rlxd(&["status", "--json"]);
        if run.code() == Some(0) {
            return run.data()["pid"].as_u64().expect("status says its pid");
        }
        assert!(Instant::now() < deadline, "rlxd never answered: {run:?}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// That rlxd `pid` still answers, a while after what started it was ended.
fn still_answering(home: &TestHome, pid: u64) {
    thread::sleep(Duration::from_secs(1));
    let run = home.rlxd(&["status", "--json"]);
    assert_eq!(
        run.code(),
        Some(0),
        "rlxd was ended with its starter: {run:?}"
    );
    assert_eq!(run.data()["pid"], pid, "the same rlxd, not another");
}

/// Every process in session `session`, as `ps` lists them.
#[cfg(unix)]
#[allow(unsafe_code)]
fn members(session: libc::pid_t) -> Vec<libc::pid_t> {
    let listed = std::process::Command::new("ps")
        .args(["-A", "-o", "pid="])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<libc::pid_t>().ok())
        // SAFETY: getsid only reads the process table.
        .filter(|&pid| unsafe { libc::getsid(pid) } == session)
        .collect()
}

#[cfg(unix)]
#[test]
#[allow(unsafe_code)]
fn serve_detach_outlives_the_session_and_process_group_it_was_started_from() {
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;

    let home = TestHome::new();
    let rlxd = PathBuf::from(env!("CARGO_BIN_EXE_rlxd"));
    // A shell leading a session of its own, as a background task's is: it starts rlxd,
    // then stays, as the task would.
    let script = format!("'{}' serve --detach; exec sleep 600", rlxd.display());
    let mut shell = home.command_of(Path::new("/bin/sh"), &["-c", &script]);
    shell
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe, and nothing else runs between fork and exec.
    unsafe {
        shell.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut shell = common::launch(&mut shell).expect("the shell starts");
    let session = libc::pid_t::try_from(shell.id()).expect("a pid");
    let pid = answering(&home);
    let rlxd_pid = libc::pid_t::try_from(pid).expect("a pid");
    // SAFETY: as in `members`.
    let its_session = unsafe { libc::getsid(rlxd_pid) };
    assert_eq!(its_session, rlxd_pid, "rlxd leads a session of its own");
    assert_ne!(its_session, session);

    // The session ends as a background task's does: everything in it, and its process
    // group, hung up and then killed.
    for signal in [libc::SIGHUP, libc::SIGKILL] {
        for member in members(session) {
            // SAFETY: a signal to a process of the shell's session, which this test made.
            unsafe { libc::kill(member, signal) };
        }
        // SAFETY: the same, to the shell's process group.
        unsafe { libc::kill(-session, signal) };
    }
    let _ = shell.wait();
    assert!(members(session).is_empty(), "the session has ended");
    still_answering(&home, pid);
}

#[cfg(windows)]
#[test]
#[allow(unsafe_code)]
fn serve_detach_outlives_the_job_object_it_was_started_in() {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{CREATE_SUSPENDED, GetCurrentProcess};

    let breakaway = JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
    // A process can leave a job only if every job it is in lets it: when this test runs
    // in a job that does not (a CI runner's may not), no process it starts can, rlxd
    // included, and there is nothing to show.
    let mut inside = 0;
    // SAFETY: asks about this process, into a BOOL.
    unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &raw mut inside) };
    if inside != 0 {
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // SAFETY: the job this process is in (a null handle), into the struct the class
        // names, at its own size.
        let read = unsafe {
            QueryInformationJobObject(
                std::ptr::null_mut(),
                JobObjectExtendedLimitInformation,
                (&raw mut limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if read == 0 || (limits.BasicLimitInformation.LimitFlags & breakaway) == 0 {
            eprintln!("skipped: this test's own job lets no process break away from it");
            return;
        }
    }

    let home = TestHome::new();
    // A job as Node's libuv makes for the children of Claude Code: killed when it closes,
    // breakaway allowed.
    // SAFETY: a new, unnamed job; closed below.
    let job: HANDLE = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    assert!(!job.is_null(), "a job: {}", std::io::Error::last_os_error());
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    // SAFETY: `limits` is the struct the class names, at its own size.
    let set = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    assert_ne!(set, 0, "{}", std::io::Error::last_os_error());

    // `rlxd serve --detach`, started suspended, put in the job, then let go: so all it
    // starts is started from inside the job.
    let mut starter = home.command(&["serve", "--detach"]);
    starter.creation_flags(CREATE_SUSPENDED);
    let mut starter = common::launch(&mut starter).expect("the starter starts");
    // SAFETY: the starter's process handle, which `starter` holds open.
    let assigned = unsafe { AssignProcessToJobObject(job, starter.as_raw_handle() as HANDLE) };
    if assigned == 0 {
        let _ = starter.kill();
        // SAFETY: closing the job made above.
        unsafe { CloseHandle(job) };
        eprintln!(
            "skipped: the starter could not be put in a job here: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    assert!(resume(starter.id()), "the starter's thread resumes");
    let started = starter.wait().expect("the starter ends");
    assert!(started.success(), "rlxd serve --detach: {started}");
    let pid = answering(&home);

    // The job closes, as it does when its owner ends: what is still in it is killed.
    // SAFETY: closing the job made above, its last handle.
    unsafe { CloseHandle(job) };
    still_answering(&home, pid);
}

/// Resumes the one thread of the suspended process `pid`, as rlxd's runner does
/// (`runner/process.rs`). Whether it did.
#[cfg(windows)]
#[allow(unsafe_code)]
fn resume(pid: u32) -> bool {
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
        dwSize: u32::try_from(size_of::<THREADENTRY32>()).unwrap_or(0),
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
