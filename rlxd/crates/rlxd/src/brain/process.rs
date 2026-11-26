//! One brain process: started in a tree of its own, read frame by frame, and killed with
//! everything it started.
//!
//! - **The tree.** On POSIX the brain leads a process group of its own, so a stop signals
//!   the group: the brain and every child it has not moved out (a detached worker starts
//!   a session of its own, as it does under `rlx hook`, and outlives the hook on purpose).
//!   On Windows a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is made first, and
//!   the brain is started suspended, put in it, then let run, so nothing it starts escapes
//!   the job: the job's end, or rlxd's, ends the whole tree.
//! - **Its output.** Each line on its stdout is a frame ([`BrainOutput`]), read by a task
//!   of its own into a channel, so waiting for one is cancel safe. Its stderr goes, line by
//!   line and stamped with the time, to `<home>/rlxd/logs/brain.log`, which rotates at 1
//!   MiB and keeps one old log.
//! - **Its end.** A thread waits for it; on POSIX it then kills what is left of its group.
//!   [`Process::exit`] says when it has ended, and how.

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};

use rlxd_proto::BrainOutput;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, watch};

use crate::clock;
use crate::log::LogFile;

/// The longest line the brain may write: 32 MiB. A hook's reply carries its output, at most
/// what rlxd's own frames carry, base64 and all.
const MAX_LINE: usize = 32 << 20;

/// The longest stderr line kept in the brain's log; the rest of a longer one is dropped.
const MAX_LOG_LINE: usize = 64 << 10;

/// One frame from the brain, or the text of a line that is not one.
pub type Frame = Result<BrainOutput, String>;

/// Held while rlxd starts a brain. On macOS a pipe is made close-on-exec only after it is
/// made, so a program another thread starts in between inherits it: a brain holding the
/// pipe of a probe would keep that probe waiting, and a program holding the brain's stdin
/// would keep the brain from seeing rlxd go.
static SPAWNING: Mutex<()> = Mutex::new(());

/// A running brain.
pub struct Process {
    /// Its process id.
    pub pid: u32,
    /// Its stdin, where rlxd writes; `None` once closed.
    pub stdin: Option<ChildStdin>,
    /// Its frames, in order; closed at the end of its stdout.
    pub frames: mpsc::Receiver<Frame>,
    exit: watch::Receiver<Option<String>>,
    tree: Tree,
}

impl Process {
    /// Starts `program args…` in `cwd`, with `env` added to rlxd's own, its stderr logged
    /// to `log`.
    pub fn spawn(
        program: &Path,
        args: &[&str],
        cwd: &Path,
        env: &[(String, String)],
        log: Arc<Mutex<LogFile>>,
    ) -> io::Result<Self> {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (mut child, tree) = {
            let _alone = SPAWNING.lock().unwrap_or_else(PoisonError::into_inner);
            os::spawn(&mut command)?
        };
        let pid = child.id();
        let stdin = child.stdin.take().map(ChildStdin::from_std).transpose()?;
        let stdout = child.stdout.take().map(ChildStdout::from_std).transpose()?;
        let stderr = child.stderr.take().map(ChildStderr::from_std).transpose()?;
        let (frames_in, frames) = mpsc::channel(16);
        if let Some(stdout) = stdout {
            tokio::spawn(read_frames(stdout, frames_in));
        }
        if let Some(stderr) = stderr {
            tokio::spawn(log_lines(stderr, log));
        }
        let (exited, exit) = watch::channel(None);
        let group = tree.group();
        std::thread::spawn(move || {
            let said = match child.wait() {
                Ok(status) => status.to_string(),
                Err(e) => format!("its end could not be read: {e}"),
            };
            os::kill_group(group);
            exited.send_replace(Some(said));
        });
        Ok(Self {
            pid,
            stdin,
            frames,
            exit,
            tree,
        })
    }

    /// What says when it has ended, apart from the process, so a wait for its end and a
    /// read of its frames can go on together.
    pub fn exit_watch(&self) -> Exit {
        Exit(self.exit.clone())
    }

    /// Resolves once it has ended, with how it ended, such as `exit status: 1`.
    pub async fn exit(&self) -> String {
        self.exit_watch().wait().await
    }

    /// Whether it has ended.
    pub fn ended(&self) -> bool {
        self.exit.borrow().is_some()
    }

    /// Asks the tree to end: SIGTERM to its group on POSIX. Windows has no such ask, and
    /// ends it as [`Process::kill`] does.
    pub fn terminate(&self) {
        if !self.ended() {
            self.tree.terminate();
        }
    }

    /// Kills the tree: SIGKILL to its group on POSIX, the job ended on Windows.
    pub fn kill(&self) {
        self.tree.kill();
    }
}

/// When a brain ends, and how.
pub struct Exit(watch::Receiver<Option<String>>);

impl Exit {
    /// Resolves once the brain has ended, with how it ended, such as `exit status: 1`.
    pub async fn wait(&mut self) -> String {
        match self.0.wait_for(Option::is_some).await {
            Ok(said) => said.clone().unwrap_or_default(),
            Err(_) => "its end was not seen".to_owned(),
        }
    }
}

/// Reads `stdout` line by line into `frames`, until its end.
async fn read_frames(stdout: impl AsyncRead + Unpin, frames: mpsc::Sender<Frame>) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match (&mut reader)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut line)
            .await
        {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let frame = if line.len() > MAX_LINE {
            Err(format!("a line longer than {MAX_LINE} bytes"))
        } else {
            serde_json::from_slice::<BrainOutput>(&line).map_err(|e| {
                let text = String::from_utf8_lossy(&line);
                let shown: String = text.trim_end().chars().take(200).collect();
                format!("{shown:?}, which is not a frame of this protocol ({e})")
            })
        };
        let too_long = line.len() > MAX_LINE;
        if frames.send(frame).await.is_err() || too_long {
            return;
        }
    }
}

/// Writes each line of `stderr` to `log`, stamped with the time.
async fn log_lines(stderr: impl AsyncRead + Unpin, log: Arc<Mutex<LogFile>>) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line[..line.len().min(MAX_LOG_LINE)]);
        let entry = format!("{} {}\n", clock::now(), text.trim_end());
        log.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write_line(&entry);
    }
}

#[cfg(unix)]
mod os {
    //! A process group of the brain's own.
    #![allow(unsafe_code)]

    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    /// The brain's process group.
    pub struct Tree {
        group: i32,
    }

    pub fn spawn(command: &mut Command) -> io::Result<(Child, Tree)> {
        command.process_group(0);
        let child = command.spawn()?;
        let group = i32::try_from(child.id()).map_err(|e| io::Error::other(e.to_string()))?;
        Ok((child, Tree { group }))
    }

    impl Tree {
        pub fn group(&self) -> i32 {
            self.group
        }

        pub fn terminate(&self) {
            signal(self.group, libc::SIGTERM);
        }

        pub fn kill(&self) {
            kill_group(self.group);
        }
    }

    /// SIGKILL to the group, as soon as its leader is reaped: whatever it started and left
    /// in its group goes with it.
    pub fn kill_group(group: i32) {
        signal(group, libc::SIGKILL);
    }

    fn signal(group: i32, signal: i32) {
        if group > 1 {
            // SAFETY: a negative pid names a process group; the group is the brain's own,
            // made with it, and a group with nobody left in it makes this fail harmlessly.
            unsafe { libc::kill(-group, signal) };
        }
    }
}

#[cfg(windows)]
mod os {
    //! A Job Object made before the brain runs.
    #![allow(unsafe_code)]

    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::{ChildExt, CommandExt};
    use std::process::{Child, Command};
    use std::sync::Arc;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED, ResumeThread};

    /// The brain's job. Closing its last handle ends every process in it.
    pub struct Tree {
        job: Arc<OwnedHandle>,
    }

    pub fn spawn(command: &mut Command) -> io::Result<(Child, Tree)> {
        let job = job()?;
        // CREATE_NO_WINDOW: a detached rlxd has no console, and a console program it
        // starts would open a window of its own.
        command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
        let mut child = command.spawn()?;
        let process = child.as_raw_handle() as HANDLE;
        // SAFETY: both handles are open; the job's is ours, the process's is the child's.
        if unsafe { AssignProcessToJobObject(job.as_raw_handle() as HANDLE, process) } == 0 {
            let error = io::Error::last_os_error();
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        let thread = child.main_thread_handle().as_raw_handle() as HANDLE;
        // SAFETY: the child's main thread handle, open while `child` is.
        if unsafe { ResumeThread(thread) } == u32::MAX {
            let error = io::Error::last_os_error();
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok((child, Tree { job: Arc::new(job) }))
    }

    /// A job that ends its processes when its last handle closes.
    fn job() -> io::Result<OwnedHandle> {
        // SAFETY: no name and no security attributes: an anonymous job of this process's.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a handle this process just opened, and owns from here on.
        let job = unsafe { OwnedHandle::from_raw_handle(raw as _) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` is the structure the class names, and outlives the call.
        let set = unsafe {
            SetInformationJobObject(
                job.as_raw_handle() as HANDLE,
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

    impl Tree {
        /// Windows has no process group: the job stands for it.
        pub fn group(&self) -> Arc<OwnedHandle> {
            Arc::clone(&self.job)
        }

        pub fn terminate(&self) {
            self.kill();
        }

        pub fn kill(&self) {
            kill_group(Arc::clone(&self.job));
        }
    }

    /// Ends every process in the job.
    pub fn kill_group(job: Arc<OwnedHandle>) {
        // SAFETY: the job's handle is open while `job` is held.
        unsafe { TerminateJobObject(job.as_raw_handle() as HANDLE, 1) };
    }
}

use os::Tree;
