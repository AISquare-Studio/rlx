//! PTYs on Windows (handoff 0.10.3, 1.4 and 2.2): each one's processes in a Job Object,
//! so `pty.kill` ends the whole tree, as a process group's signal does elsewhere.
//!
//! A job must hold a process before that process starts any other, or a child started in
//! between escapes it: the Windows trap. portable-pty can't put the process it starts
//! into a job as it creates it, so the ConPTY's process is rlxd itself, in a hidden mode,
//! the shim (`rlxd __pty-shim --job <name> -- <argv>`):
//!
//! 1. the daemon makes the job, named `Local\rlxd-pty-<128 random bits>` so no other
//!    process can guess it, with `KILL_ON_JOB_CLOSE`;
//! 2. the shim opens it, puts itself in it, and checks it is in it, or refuses to go on;
//! 3. only then does it start the real program, which, as everything it starts, is born
//!    in the job;
//! 4. it waits, and exits with the program's exit code, all 32 bits of it.
//!
//! The shim survives Ctrl-C and Ctrl-Break with a handler of its own: the console sends
//! them to every process on it, the real program among them, which handles them as it
//! would anywhere. First it turns Ctrl-C back on: a detached rlxd runs in a process group
//! of its own, which ignores Ctrl-C, and every process inherits that from the one that
//! starts it. Turned back on, the program inherits that instead. (A null handler for the
//! shim would not do: the program would inherit ignoring Ctrl-C again.) `pty.kill` is
//! `TerminateJobObject`.

#![allow(unsafe_code)]

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation, OpenJobObjectW,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// JOB_OBJECT_ASSIGN_PROCESS and JOB_OBJECT_QUERY, as winnt.h defines them.
const JOB_ASSIGN_AND_QUERY: u32 = 0x0001 | 0x0004;

/// The environment variable that names the job to the program it holds.
pub const JOB_VAR: &str = "RLXD_PTY_JOB";

/// The exit code of a shim that could not start its program.
const SHIM_FAILED: u8 = 2;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A PTY's Job Object, which the daemon holds. Closing it ends what it still holds.
pub struct Job {
    handle: HANDLE,
    name: String,
}

// SAFETY: a job handle may be used from any thread.
unsafe impl Send for Job {}
// SAFETY: every use of the handle is a kernel call that is safe to make concurrently.
unsafe impl Sync for Job {}

impl Job {
    /// A new job, named with 128 random bits, that ends its processes when it closes.
    pub fn create(random: &str) -> io::Result<Self> {
        let name = format!("Local\\rlxd-pty-{random}");
        let wide_name = wide(&name);
        // SAFETY: a null security descriptor and a NUL-terminated name.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), wide_name.as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self { handle, name };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` is the struct the class names, at its own size.
        let set = unsafe {
            SetInformationJobObject(
                job.handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if set == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Its name, for the shim.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Ends every process it holds, with exit code `code`.
    pub fn terminate(&self, code: u32) -> io::Result<()> {
        // SAFETY: the handle is the job's, open until drop.
        if unsafe { TerminateJobObject(self.handle, code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// How many processes it holds now.
    pub fn active(&self) -> io::Result<u32> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: `info` is the struct the class names, at its own size.
        let got = unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectBasicAccountingInformation,
                (&raw mut info).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if got == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.ActiveProcesses)
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle is the job's, and closed once.
        unsafe { CloseHandle(self.handle) };
    }
}

/// Keeps the shim alive through Ctrl-C and Ctrl-Break, which the console sends to the
/// real program too.
unsafe extern "system" fn survive(ctrl: u32) -> windows_sys::core::BOOL {
    i32::from(ctrl == CTRL_C_EVENT || ctrl == CTRL_BREAK_EVENT)
}

/// `rlxd __pty-shim --job <name> -- <argv>`: joins the job, then runs `argv` in it and
/// exits with its exit code.
pub fn shim(job: &str, argv: &[String]) -> ExitCode {
    let refuse = |why: String| {
        eprintln!("rlxd __pty-shim: {why}");
        ExitCode::from(SHIM_FAILED)
    };
    let Some(program) = argv.first() else {
        return refuse("no program to run".to_owned());
    };
    if !job.starts_with("Local\\rlxd-pty-") {
        return refuse(format!("{job:?} is not a PTY's job"));
    }
    let name = wide(job);
    // SAFETY: a NUL-terminated name; the handle stays open for the shim's life.
    let handle = unsafe { OpenJobObjectW(JOB_ASSIGN_AND_QUERY, 0, name.as_ptr()) };
    if handle.is_null() {
        return refuse(format!("no job {job}: {}", io::Error::last_os_error()));
    }
    // SAFETY: the job handle just opened, and this process's own pseudo-handle.
    if unsafe { AssignProcessToJobObject(handle, GetCurrentProcess()) } == 0 {
        return refuse(format!("cannot join {job}: {}", io::Error::last_os_error()));
    }
    let mut inside = 0;
    // SAFETY: as above, and a valid out-pointer.
    let checked = unsafe { IsProcessInJob(GetCurrentProcess(), handle, &raw mut inside) };
    if checked == 0 || inside == 0 {
        return refuse(format!("not in {job} after joining it"));
    }
    // Ctrl-C back on, for the program to inherit: a detached rlxd's process group
    // ignores it, and so would every process it starts.
    // SAFETY: a null handler and FALSE: restores normal Ctrl-C processing.
    unsafe { SetConsoleCtrlHandler(None, 0) };
    // SAFETY: `survive` is a valid handler for the process's life.
    unsafe { SetConsoleCtrlHandler(Some(survive), 1) };
    let status = std::process::Command::new(program)
        .args(&argv[1..])
        .env(JOB_VAR, job)
        .status();
    match status {
        Ok(status) => {
            // Windows exit codes are 32 bits: `exit` passes all of them on.
            let code = status.code().unwrap_or(1);
            std::process::exit(code)
        }
        Err(e) => refuse(format!("cannot start {program:?}: {e}")),
    }
}

/// Where `program` is, as `CreateProcess` would find it: as given when it has a path,
/// else on `PATH`, with each of `PATHEXT`'s extensions. `None` when nowhere.
pub fn find_program(program: &str, path: Option<&str>, cwd: &Path) -> Option<PathBuf> {
    let extensions: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
        .split(';')
        .filter(|e| !e.is_empty())
        .map(str::to_owned)
        .collect();
    let candidates = |base: PathBuf| -> Vec<PathBuf> {
        let mut all = vec![base.clone()];
        all.extend(extensions.iter().map(|e| {
            let mut with = base.clone().into_os_string();
            with.push(e);
            PathBuf::from(with)
        }));
        all
    };
    let given = Path::new(program);
    if given.components().count() > 1 || given.is_absolute() {
        let base = if given.is_absolute() {
            given.to_path_buf()
        } else {
            cwd.join(given)
        };
        return candidates(base).into_iter().find(|p| p.is_file());
    }
    let path = path
        .map(str::to_owned)
        .or_else(|| std::env::var("PATH").ok())?;
    std::env::split_paths(&path)
        .flat_map(|dir| candidates(dir.join(program)))
        .find(|p| p.is_file())
}
