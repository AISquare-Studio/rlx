//! The PTY host (handoff 0.10.3, 1.4): processes on pseudo-terminals rlxd holds, as tmux
//! held the fleet's panes, so they outlive every client. ConPTY on Windows.
//!
//! - **A PTY** is a process on a pseudo-terminal, its screen (`crate::vt`, which
//!   prints as tmux's `capture-pane -e -p` does), and three threads: one reads the
//!   process's output into the screen, one writes its input in order, and one waits for
//!   it to end. A process that ends keeps its last screen and exit status until
//!   `pty.kill`, as tmux's `remain-on-exit on` keeps a pane.
//! - **Every PTY is the home's.** One spawned by any connection is in every connection's
//!   `pty.list`; each has 128 random bits for its id.
//! - **Keys are tmux's** (`keys`): a name is read as `send-keys` reads it, and sent as
//!   tmux sends it, honouring the program's modes; a name tmux doesn't know is typed.
//! - **Writes queue**, in order, behind the program's own replies to its queries: a
//!   `pty.write` returns once its bytes are queued, as tmux's `send-keys` returns once
//!   they are buffered. A program that reads none of them stops taking more at 16 MiB.
//! - **`pty.kill`** sends SIGHUP and SIGTERM to the process's whole group, SIGKILL to
//!   what is left after two seconds, and returns once the group is gone. A process that
//!   left the group itself is not ended, as under tmux.
//! - **Limits:** [`MAX_PTYS`] PTYs a home, the exited ones included, and
//!   [`SCROLLBACK`] lines of history each.

pub mod keys;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::shim;

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use rlxd_proto::{
    MAX_PTY_SCROLLBACK, PtyCursor, PtyExited, PtyInfo, PtyInput, PtyKillParams, PtyList, PtyOutput,
    PtyRef, PtyResizeParams, PtyScreen, PtyScreenParams, PtySpawnParams, PtyWriteParams, VERSION,
    event_types, topics,
};

use crate::bus::Bus;
use crate::clock;
use crate::log::{error, info, warning};
use crate::vt::Terminal;

/// The most PTYs a home holds, live and exited.
pub const MAX_PTYS: usize = 64;

/// The lines of history each PTY keeps, as the fleet's tmux does (`history-limit`).
pub const SCROLLBACK: u32 = MAX_PTY_SCROLLBACK;

/// The most input queued for a program that isn't reading it.
const MAX_QUEUED: usize = 16 << 20;

/// How long `pty.kill` waits after SIGTERM before SIGKILL, and then for the group to go.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// How long the end of a process waits for its last output to reach the screen.
const DRAIN: Duration = Duration::from_millis(500);

/// How much one read of a PTY's output takes: one `pty.output` event's worth at most.
const READ_CHUNK: usize = 32 << 10;

/// Why a PTY call was refused.
#[derive(Debug)]
pub enum Refused {
    /// The home holds [`MAX_PTYS`] already.
    TooMany,
    /// No PTY has that id.
    Unknown(String),
    /// The PTY's process has ended.
    Exited(String),
    /// The process could not be started: why.
    SpawnFailed(String),
    /// The params can't be acted on: why.
    Invalid(String),
    /// The program is not reading its input, and the queue is full.
    Busy(String),
}

/// How a PTY's process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Exit {
    status: u32,
    signal: Option<u32>,
}

/// What a PTY's threads share, under its lock.
struct State {
    terminal: Terminal,
    exit: Option<Exit>,
    /// The output is read to its end: the terminal closed.
    drained: bool,
}

/// One PTY.
struct Pty {
    id: String,
    label: Option<String>,
    argv: Vec<String>,
    cwd: String,
    pid: u32,
    started_at: String,
    state: Mutex<State>,
    changed: Condvar,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    input: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    queued: Arc<AtomicUsize>,
    /// The Job Object that holds the PTY's processes.
    #[cfg(windows)]
    job: windows::Job,
}

impl Pty {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Queues `bytes` for the program, after what is queued already.
    fn send(&self, bytes: Vec<u8>) -> Result<(), Refused> {
        if bytes.is_empty() {
            return Ok(());
        }
        let input = self.input.lock().unwrap_or_else(|p| p.into_inner());
        let Some(input) = input.as_ref() else {
            return Err(Refused::Exited(self.id.clone()));
        };
        let len = bytes.len();
        if self.queued.load(Ordering::Acquire) + len > MAX_QUEUED {
            return Err(Refused::Busy(format!(
                "pty {} isn't reading its input: {} bytes wait already",
                self.id,
                self.queued.load(Ordering::Acquire)
            )));
        }
        self.queued.fetch_add(len, Ordering::AcqRel);
        input
            .send(bytes)
            .map_err(|_| Refused::Exited(self.id.clone()))
    }

    fn info(&self) -> Result<PtyInfo, Refused> {
        let state = self.state();
        let (cols, rows) = state.terminal.size();
        let info = PtyInfo::new(
            &self.id,
            self.argv.clone(),
            &self.cwd,
            self.pid,
            cols,
            rows,
            &self.started_at,
        )
        .and_then(|info| info.with_label(self.label.clone()));
        let info = match state.exit {
            Some(exit) => info.and_then(|info| info.with_exit(exit.status, exit.signal)),
            None => info,
        };
        info.map_err(|e| Refused::Invalid(e.to_string()))
    }
}

/// The home's PTYs.
pub struct Host {
    ptys: Mutex<Vec<Arc<Pty>>>,
    bus: Arc<Bus>,
    /// Bumped each time a PTY goes, so a stream watching one can end.
    gone: tokio::sync::watch::Sender<u64>,
}

impl Host {
    /// No PTYs yet; each one's end is published on `bus`.
    pub fn new(bus: Arc<Bus>) -> Self {
        Self {
            ptys: Mutex::new(Vec::new()),
            bus,
            gone: tokio::sync::watch::Sender::new(0),
        }
    }

    /// Changes each time a PTY goes.
    pub fn gone(&self) -> tokio::sync::watch::Receiver<u64> {
        self.gone.subscribe()
    }

    /// Whether the PTY `id` is still the home's.
    pub fn exists(&self, id: &str) -> bool {
        self.ptys().iter().any(|pty| pty.id == id)
    }

    fn ptys(&self) -> MutexGuard<'_, Vec<Arc<Pty>>> {
        self.ptys.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn find(&self, id: &str) -> Result<Arc<Pty>, Refused> {
        self.ptys()
            .iter()
            .find(|pty| pty.id == id)
            .cloned()
            .ok_or_else(|| Refused::Unknown(id.to_owned()))
    }

    /// Starts `params.argv` on a new PTY, for `who`, as logged.
    pub fn spawn(&self, params: &PtySpawnParams, who: &str) -> Result<PtyRef, Refused> {
        let mut ptys = self.ptys();
        if ptys.len() >= MAX_PTYS {
            return Err(Refused::TooMany);
        }
        let cwd = match &params.cwd {
            Some(cwd) => {
                let path = Path::new(cwd);
                if !path.is_absolute() || !path.is_dir() {
                    return Err(Refused::SpawnFailed(format!(
                        "cwd {cwd:?} is not an absolute path to a directory"
                    )));
                }
                cwd.clone()
            }
            None => home_dir(),
        };
        #[cfg(windows)]
        let job = {
            // The shim starts the program, so a program that isn't there is seen here,
            // as CreateProcess would look for it, while spawn_failed can still say so.
            let path = match params.env.as_ref().and_then(|env| env.get("PATH")) {
                Some(Some(path)) => Some(path.clone()),
                Some(None) => Some(String::new()),
                None => None,
            };
            if windows::find_program(&params.argv[0], path.as_deref(), Path::new(&cwd)).is_none() {
                return Err(Refused::SpawnFailed(format!(
                    "cannot start {:?}: no such program",
                    params.argv[0]
                )));
            }
            windows::Job::create(&new_id())
                .map_err(|e| Refused::SpawnFailed(format!("no job for the pty: {e}")))?
        };
        #[cfg(windows)]
        let mut command = {
            let rlxd = std::env::current_exe()
                .map_err(|e| Refused::SpawnFailed(format!("rlxd can't find itself: {e}")))?;
            let mut command = CommandBuilder::new(rlxd);
            command.args(["__pty-shim", "--job", job.name(), "--"]);
            command.args(&params.argv);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = CommandBuilder::new(&params.argv[0]);
            command.args(&params.argv[1..]);
            command
        };
        command.cwd(&cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        for (name, value) in params.env.iter().flatten() {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            }
        }
        let size = PtySize {
            rows: params.rows,
            cols: params.cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let failed = |what: &str, e: &dyn std::fmt::Display| {
            Refused::SpawnFailed(format!("{what} {:?}: {e}", params.argv[0]))
        };
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|e| failed("no pseudo-terminal for", &e))?;
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| failed("cannot start", &e))?;
        drop(pair.slave);
        let Some(pid) = child.process_id() else {
            return Err(Refused::SpawnFailed(format!(
                "{:?} started with no process id",
                params.argv[0]
            )));
        };
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| failed("no output for", &e))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| failed("no input for", &e))?;
        let id = new_id();
        let (input, queue) = mpsc::channel::<Vec<u8>>();
        let queued = Arc::new(AtomicUsize::new(0));
        let pty = Arc::new(Pty {
            id: id.clone(),
            label: params.label.clone(),
            argv: params.argv.clone(),
            cwd,
            pid,
            started_at: clock::now(),
            state: Mutex::new(State {
                terminal: Terminal::new(params.cols, params.rows, SCROLLBACK, VERSION),
                exit: None,
                drained: false,
            }),
            changed: Condvar::new(),
            master: Mutex::new(Some(pair.master)),
            input: Mutex::new(Some(input)),
            queued: Arc::clone(&queued),
            #[cfg(windows)]
            job,
        });
        let short = &id[..8];
        let threads = [
            spawn_thread(format!("pty-{short}-in"), move || {
                write_input(queue, writer, &queued)
            }),
            {
                let pty = Arc::clone(&pty);
                let bus = Arc::clone(&self.bus);
                spawn_thread(format!("pty-{short}-out"), move || {
                    read_output(&pty, reader, &bus);
                })
            },
            {
                let pty = Arc::clone(&pty);
                let bus = Arc::clone(&self.bus);
                spawn_thread(format!("pty-{short}-wait"), move || {
                    wait_for_end(&pty, child, &bus);
                })
            },
        ];
        if let Some(e) = threads.into_iter().find_map(Result::err) {
            error!("pty {id}: a thread could not start: {e}");
        }
        info!(
            "pty {id} spawned: {:?} as pid {pid}, {}x{}, by {who}",
            params.argv[0], params.cols, params.rows
        );
        ptys.push(pty);
        PtyRef::new(id).map_err(|e| Refused::Invalid(e.to_string()))
    }

    /// Types, presses or pastes `params`' input into its PTY.
    pub fn write(&self, params: &PtyWriteParams) -> Result<PtyRef, Refused> {
        let pty = self.find(&params.pty_id)?;
        let bytes = {
            let state = pty.state();
            if state.exit.is_some() {
                return Err(Refused::Exited(pty.id.clone()));
            }
            let modes = state.terminal.input_modes();
            match params.input() {
                PtyInput::Text(text) => text.as_bytes().to_vec(),
                PtyInput::Paste(text) => paste(text, modes.bracketed_paste),
                PtyInput::Keys(names) => keys::send_keys(names, modes),
                _ => return Err(Refused::Invalid("no input this rlxd knows".to_owned())),
            }
        };
        pty.send(bytes)?;
        PtyRef::new(&pty.id).map_err(|e| Refused::Invalid(e.to_string()))
    }

    /// Resizes `params`' PTY, the program's terminal and the screen alike.
    pub fn resize(&self, params: &PtyResizeParams) -> Result<PtyRef, Refused> {
        let pty = self.find(&params.pty_id)?;
        let mut state = pty.state();
        if state.exit.is_some() {
            return Err(Refused::Exited(pty.id.clone()));
        }
        let master = pty.master.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(master) = master.as_ref() {
            let size = PtySize {
                rows: params.rows,
                cols: params.cols,
                pixel_width: 0,
                pixel_height: 0,
            };
            master
                .resize(size)
                .map_err(|e| Refused::Invalid(format!("cannot resize pty {}: {e}", pty.id)))?;
        }
        state.terminal.resize(params.cols, params.rows);
        PtyRef::new(&pty.id).map_err(|e| Refused::Invalid(e.to_string()))
    }

    /// `params`' PTY's screen, as tmux's `capture-pane -e -p` prints it.
    pub fn screen(&self, params: &PtyScreenParams) -> Result<PtyScreen, Refused> {
        let pty = self.find(&params.pty_id)?;
        let state = pty.state();
        let terminal = &state.terminal;
        let (x, y, visible) = terminal.cursor();
        let (cols, rows) = terminal.size();
        let text = terminal.capture(params.scrollback.unwrap_or(0));
        let screen = PtyScreen::new(
            &pty.id,
            text,
            PtyCursor::new(x, y, visible),
            cols,
            rows,
            self.bus.head(),
        )
        .and_then(|screen| screen.with_state(terminal.alternate(), terminal.history()));
        let screen = match state.exit {
            Some(exit) => screen.and_then(|screen| screen.with_exit(exit.status, exit.signal)),
            None => screen,
        };
        screen.map_err(|e| Refused::Invalid(e.to_string()))
    }

    /// Every PTY of the home, in the order they started.
    pub fn list(&self) -> Result<PtyList, Refused> {
        let ptys: Vec<Arc<Pty>> = self.ptys().clone();
        let infos = ptys
            .iter()
            .map(|pty| pty.info())
            .collect::<Result<Vec<_>, _>>()?;
        PtyList::new(infos).map_err(|e| Refused::Invalid(e.to_string()))
    }

    /// Ends `params`' PTY's process and its whole group, and frees the PTY, for `who`.
    pub fn kill(&self, params: &PtyKillParams, who: &str) -> Result<PtyRef, Refused> {
        let pty = self.find(&params.pty_id)?;
        // The process ends first, so its pty.exited reaches a stream before the stream
        // ends with the PTY.
        end(&pty);
        self.ptys().retain(|held| !Arc::ptr_eq(held, &pty));
        self.gone.send_modify(|n| *n += 1);
        info!("pty {} killed, by {who}", pty.id);
        PtyRef::new(&pty.id).map_err(|e| Refused::Invalid(e.to_string()))
    }

    /// Ends every PTY, as rlxd stops.
    pub fn kill_all(&self) {
        let ptys: Vec<Arc<Pty>> = std::mem::take(&mut *self.ptys());
        if !ptys.is_empty() {
            info!("ending {} pty(s) as rlxd stops", ptys.len());
        }
        let handles: Vec<_> = ptys
            .into_iter()
            .map(|pty| std::thread::spawn(move || end(&pty)))
            .collect();
        for handle in handles {
            let _ = handle.join();
        }
        self.gone.send_modify(|n| *n += 1);
    }
}

/// Ends `pty`'s process and its group, then closes the terminal.
fn end(pty: &Pty) {
    #[cfg(unix)]
    {
        if pty.state().exit.is_none() {
            unix::signal_group(pty.pid, unix::SIGHUP);
            unix::signal_group(pty.pid, unix::SIGTERM);
            wait_until(pty, KILL_GRACE, |state| state.exit.is_some());
        }
        if unix::group_alive(pty.pid) {
            unix::signal_group(pty.pid, unix::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        // Every process of the job at once, the shim and the program among them.
        if let Err(e) = pty.job.terminate(1) {
            warning!("pty {}: cannot end its job: {e}", pty.id);
        }
        wait_until(pty, KILL_GRACE, |state| state.exit.is_some());
        let deadline = Instant::now() + KILL_GRACE;
        while pty.job.active().is_ok_and(|n| n > 0) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if pty.job.active().is_ok_and(|n| n > 0) {
            warning!("pty {}: processes of its job still run", pty.id);
        }
    }
    pty.input.lock().unwrap_or_else(|p| p.into_inner()).take();
    pty.master.lock().unwrap_or_else(|p| p.into_inner()).take();
    #[cfg(unix)]
    {
        let deadline = Instant::now() + KILL_GRACE;
        while unix::group_alive(pty.pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if unix::group_alive(pty.pid) {
            warning!(
                "pty {}: processes of group {} still run after SIGKILL",
                pty.id,
                pty.pid
            );
        }
    }
}

/// Waits up to `timeout` for `done` to hold of `pty`'s state.
fn wait_until(pty: &Pty, timeout: Duration, done: impl Fn(&State) -> bool) {
    let deadline = Instant::now() + timeout;
    let mut state = pty.state();
    while !done(&state) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        state = pty
            .changed
            .wait_timeout(state, left)
            .unwrap_or_else(|p| p.into_inner())
            .0;
    }
}

fn spawn_thread(
    name: String,
    run: impl FnOnce() + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name(name).spawn(run)
}

/// Writes the queued input in order, until the PTY goes.
fn write_input(
    queue: mpsc::Receiver<Vec<u8>>,
    mut writer: Box<dyn Write + Send>,
    queued: &AtomicUsize,
) {
    for bytes in queue {
        let written = writer.write_all(&bytes).and_then(|()| writer.flush());
        queued.fetch_sub(bytes.len(), Ordering::AcqRel);
        if written.is_err() {
            return;
        }
    }
}

/// Reads the program's output into the screen, and sends its queries' replies back.
fn read_output(pty: &Pty, mut reader: Box<dyn Read + Send>, bus: &Bus) {
    let mut buf = vec![0u8; READ_CHUNK];
    let mut warned = false;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let chunk = &buf[..n];
        let replies = {
            // The screen takes the bytes and the bus numbers them under one lock, so a
            // snapshot taken under it holds exactly the output up to the bus's head.
            let mut state = pty.state();
            let replies = state.terminal.feed(chunk);
            let data = PtyOutput::new(&pty.id, base64(chunk)).and_then(|o| o.to_data());
            let published = match data {
                Ok(data) => bus
                    .publish(topics::PTY, event_types::PTY_OUTPUT, data)
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            if let Err(why) = published
                && !warned
            {
                warned = true;
                error!("pty {}: its output was not published: {why}", pty.id);
            }
            replies
        };
        if !replies.is_empty() {
            let _ = pty.send(replies);
        }
    }
    pty.state().drained = true;
    pty.changed.notify_all();
}

/// Waits for the program to end, keeps how it ended, and publishes `pty.exited`.
fn wait_for_end(pty: &Pty, child: Box<dyn portable_pty::Child + Send + Sync>, bus: &Bus) {
    let exit = ended(pty, child);
    // The last output can still be on its way: let it reach the screen first.
    wait_until(pty, DRAIN, |state| state.drained);
    let mut state = pty.state();
    state.exit = Some(exit);
    // Published under the lock, as output is: a snapshot either shows the end, or comes
    // before its event.
    let data = PtyExited::new(&pty.id, exit.status, exit.signal).and_then(|e| e.to_data());
    let published = match data {
        Ok(data) => bus
            .publish(topics::PTY, event_types::PTY_EXITED, data)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    drop(state);
    pty.changed.notify_all();
    info!(
        "pty {}: pid {} ended, status {}{}",
        pty.id,
        pty.pid,
        exit.status,
        exit.signal
            .map(|s| format!(" (signal {s})"))
            .unwrap_or_default()
    );
    if let Err(why) = published {
        error!("pty {}: pty.exited was not published: {why}", pty.id);
    }
}

#[cfg(unix)]
fn ended(pty: &Pty, child: Box<dyn portable_pty::Child + Send + Sync>) -> Exit {
    // `child` is std's, which would report a signal as code 1: reap by pid instead.
    drop(child);
    match unix::wait(pty.pid) {
        Ok(ended) => Exit {
            status: ended.status,
            signal: ended.signal,
        },
        Err(e) => {
            error!("pty {}: cannot wait for pid {}: {e}", pty.id, pty.pid);
            Exit {
                status: 1,
                signal: None,
            }
        }
    }
}

#[cfg(windows)]
fn ended(pty: &Pty, mut child: Box<dyn portable_pty::Child + Send + Sync>) -> Exit {
    match child.wait() {
        Ok(status) => Exit {
            status: status.exit_code(),
            signal: None,
        },
        Err(e) => {
            error!("pty {}: cannot wait for pid {}: {e}", pty.id, pty.pid);
            Exit {
                status: 1,
                signal: None,
            }
        }
    }
}

/// `bytes` in standard base64, with padding.
pub fn base64(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(DIGITS[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Standard base64 back to bytes: `None` when it isn't.
pub fn unbase64(text: &str) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        Some(u32::from(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        }))
    };
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 {
            return None;
        }
        let mut n = 0u32;
        for &c in &chunk[..4 - pad] {
            n = (n << 6) | value(c)?;
        }
        n <<= 6 * pad as u32;
        let decoded = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&decoded[..3 - pad]);
    }
    Some(out)
}

/// 128 random bits, as 32 lowercase hex digits.
fn new_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the OS gives random bytes");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The directory a PTY starts in when `pty.spawn` names none: the user's home.
fn home_dir() -> String {
    let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(name)
        .ok()
        .filter(|home| Path::new(home).is_dir())
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "C:\\".to_owned()
            } else {
                "/".to_owned()
            }
        })
}

/// `text` as tmux's `paste-buffer -p` sends it: each line made safe for a terminal, as
/// `vis(VIS_SAFE | VIS_NOSLASH)` makes it (a control character but tab, backspace, bell
/// and carriage return written as `^X`, delete as `^?`), each newline a carriage
/// return, and the whole bracketed when the program asked for bracketed paste.
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 12);
    if bracketed {
        out.extend_from_slice(b"\x1b[200~");
    }
    for c in text.chars() {
        match c {
            '\n' => out.push(b'\r'),
            '\t' | '\u{8}' | '\u{7}' | '\r' | ' '..='~' => out.push(c as u8),
            '\u{7f}' => out.extend_from_slice(b"^?"),
            c if (c as u32) < 0x20 => {
                out.push(b'^');
                out.push(c as u8 + b'@');
            }
            c => {
                let mut utf8 = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
            }
        }
    }
    if bracketed {
        out.extend_from_slice(b"\x1b[201~");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_paste_is_made_safe_and_bracketed_when_asked() {
        assert_eq!(paste("a\nb", false), b"a\rb");
        assert_eq!(paste("a\nb", true), b"\x1b[200~a\rb\x1b[201~");
        assert_eq!(paste("\x1b[31m\x03\x7f\tok\\", false), b"^[[31m^C^?\tok\\");
        assert_eq!(paste("中\r\n", false), "中\r\r".as_bytes());
    }

    #[test]
    fn base64_goes_there_and_back() {
        for bytes in [
            &b""[..],
            b"r",
            b"re",
            b"rea",
            b"ready\r\n",
            &[0, 255, 128, 7],
        ] {
            let text = base64(bytes);
            assert!(text.is_empty() || rlxd_proto::PtyOutput::new("0".repeat(32), &text).is_ok());
            assert_eq!(unbase64(&text).as_deref(), Some(bytes));
        }
        assert_eq!(base64(b"ready\r\n"), "cmVhZHkNCg==");
        assert_eq!(unbase64("abc"), None);
    }

    #[test]
    fn an_id_is_128_random_bits() {
        let (a, b) = (new_id(), new_id());
        assert_eq!(a.len(), 32);
        assert!(rlxd_proto::check_pty_id("id", &a).is_ok());
        assert_ne!(a, b);
    }
}
