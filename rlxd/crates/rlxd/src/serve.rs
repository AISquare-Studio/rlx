//! `rlxd serve [--detach]`: one rlxd per home (handoff 0.10.3, 1.3).
//!
//! In order, a start:
//!
//! 1. makes the home's `run/`, `rlxd/` and `rlxd/logs/` (each 0700);
//! 2. takes `run/rlxd.lock`, or exits 1 with `already_running`;
//! 3. opens the log, and makes the token once;
//! 4. opens the event bus, which numbers on from `rlxd/seq` (`crate::bus`);
//! 5. listens on the socket or pipe;
//! 6. registers in `<home>/daemons/` (4.1), with a heartbeat every 5 s;
//! 7. publishes `daemon.started`, and serves until `stop`, SIGTERM or Ctrl-C.
//!
//! A stop publishes `daemon.stopping`, gives the connections a moment to send it, writes
//! the bus's last seq, removes the socket, leaves the registry, then lets the lock go, so
//! once the lock is free everything else is done. `--detach` starts the same in the background and returns once it takes
//! a connection, printing its `status`.

use std::fs;
use std::io;
use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use rlxd_client::{Home, Reply};
use rlxd_proto::{VERSION, codes, methods};
use tokio::task::JoinSet;

use crate::bus::Bus;
use crate::listener::Listener;
use crate::log::{error, info, warning};
use crate::registry::{self, Registration};
use crate::server::{self, Daemon};
use crate::{cli, lock, log, out, token};

/// The hidden flag a detached rlxd is started with: log to the file only.
pub const BACKGROUND_FLAG: &str = "--background";

/// How long a start waits for the lock while another process holds it: a probe takes it
/// for an instant (`lock::is_held`), and must not make a start fail.
const LOCK_WAIT: Duration = Duration::from_secs(1);

/// How long `--detach` waits for the new rlxd to take a connection.
const START_WAIT: Duration = Duration::from_secs(15);

/// How long a stopping rlxd gives its connections to finish.
const FINISH_WAIT: Duration = Duration::from_secs(3);

/// `rlxd serve`, in the foreground: logs to stderr too, unless `background`.
pub fn serve(background: bool) -> ExitCode {
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(e) => return out::fail(codes::START_FAILED, &e.to_string(), None),
    };
    if let Err(e) = prepare(&home) {
        let message = format!(
            "cannot make rlxd's folders in {}: {e}",
            home.root().display()
        );
        return out::fail(codes::START_FAILED, &message, None);
    }
    let held = match lock::acquire(&home.lock(), LOCK_WAIT) {
        Ok(Some(held)) => held,
        Ok(None) => return already_running(&home),
        Err(e) => {
            let message = format!("cannot take {}: {e}", home.lock().display());
            return out::fail(codes::START_FAILED, &message, None);
        }
    };
    if let Err(e) = log::open(&home.log_file(), !background) {
        let message = format!("cannot open {}: {e}", home.log_file().display());
        return out::fail(codes::START_FAILED, &message, None);
    }
    info!(
        "rlxd {VERSION} starting: pid {}, home {}",
        std::process::id(),
        home.root().display()
    );
    let result = token::ensure(&home.token())
        .map_err(|e| format!("cannot make {}: {e}", home.token().display()))
        .and_then(|token| run(home, token));
    drop(held);
    match result {
        Ok(()) => {
            info!("rlxd stopped");
            ExitCode::SUCCESS
        }
        Err(message) => {
            error!("rlxd could not run: {message}");
            out::fail(codes::START_FAILED, &message, None)
        }
    }
}

/// Serves until a stop, on a runtime of its own.
fn run(home: Home, token: String) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the runtime: {e}"))?;
    let result = runtime.block_on(serve_until_stopped(home, token));
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

async fn serve_until_stopped(home: Home, token: String) -> Result<(), String> {
    let bus = Bus::open(&home)?;
    let mut listener = Listener::bind(&home).map_err(|e| e.to_string())?;
    let endpoint = home.endpoint();
    let registration = match Registration::register(&home) {
        Ok(registration) => {
            info!(
                "registered in {} as {}",
                home.daemons().display(),
                registration.id()
            );
            Some(registration)
        }
        Err(e) => {
            warning!("running unregistered, trying again at each heartbeat: {e}");
            None
        }
    };
    let keeper = Keeper::start(home.clone(), registration);
    let daemon = Daemon::new(home, token, listener.acl(), bus);
    daemon.started();
    tokio::spawn(watch_signals(Arc::clone(&daemon)));
    info!("listening on {endpoint}");
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    connections.spawn(server::serve_connection(Arc::clone(&daemon), stream));
                }
                Err(e) => {
                    warning!("a connection could not be taken: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            () = daemon.stopped() => break,
        }
    }
    drop(listener);
    let finished = async { while connections.join_next().await.is_some() {} };
    if tokio::time::timeout(FINISH_WAIT, finished).await.is_err() {
        warning!(
            "connections still open after {} s: closing them",
            FINISH_WAIT.as_secs()
        );
        connections.abort_all();
    }
    daemon.bus().close();
    keeper.stop();
    Ok(())
}

/// Keeps the registration fresh on a thread of its own, and leaves the registry when
/// stopped.
struct Keeper {
    stop: mpsc::Sender<()>,
    thread: thread::JoinHandle<()>,
}

impl Keeper {
    fn start(home: Home, mut registration: Option<Registration>) -> Self {
        let (stop, stopped) = mpsc::channel();
        let thread = thread::spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(registry::HEARTBEAT) {
                match registration.as_mut() {
                    Some(registration) => {
                        if let Err(e) = registration.beat() {
                            warning!("the heartbeat could not be written: {e}");
                        }
                    }
                    None => match Registration::register(&home) {
                        Ok(registered) => {
                            info!("registered as {}", registered.id());
                            registration = Some(registered);
                        }
                        Err(e) => warning!("still unregistered: {e}"),
                    },
                }
            }
            if let Some(registration) = registration {
                registration.leave();
                info!("left the registry");
            }
        });
        Self { stop, thread }
    }

    /// Leaves the registry, and waits until it has.
    fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.thread.join();
    }
}

#[cfg(unix)]
async fn watch_signals(daemon: Arc<Daemon>) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut interrupt), Ok(mut hangup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    ) else {
        warning!("signals cannot be watched: stop rlxd with `rlxd stop`");
        return;
    };
    loop {
        tokio::select! {
            _ = term.recv() => daemon.stop("sigterm"),
            _ = interrupt.recv() => daemon.stop("sigint"),
            _ = hangup.recv() => info!("SIGHUP ignored: rlxd outlives its terminal"),
        }
    }
}

#[cfg(windows)]
async fn watch_signals(daemon: Arc<Daemon>) {
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close};
    // A detached rlxd has no console, and so gets none of these.
    let (Ok(mut c), Ok(mut brk), Ok(mut close)) = (ctrl_c(), ctrl_break(), ctrl_close()) else {
        return;
    };
    loop {
        tokio::select! {
            _ = c.recv() => daemon.stop("ctrl_c"),
            _ = brk.recv() => daemon.stop("ctrl_break"),
            _ = close.recv() => daemon.stop("console_closed"),
        }
    }
}

/// `rlxd serve --detach`: starts rlxd in the background, detached from this terminal,
/// and returns once it takes a connection, printing its `status`.
pub fn detach() -> ExitCode {
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(e) => return out::fail(codes::START_FAILED, &e.to_string(), None),
    };
    match start(&home, |_| {}) {
        Ok(reply) => out::print_reply(&reply),
        Err(StartError::AlreadyRunning) => already_running(&home),
        Err(StartError::Failed { message, hint }) => {
            out::fail(codes::START_FAILED, &message, hint.as_deref())
        }
    }
}

/// Why [`start`] gave no rlxd of its own.
pub enum StartError {
    /// An rlxd already runs for the home.
    AlreadyRunning,
    /// It could not start: what went wrong, and where to look.
    Failed {
        /// What went wrong.
        message: String,
        /// Where to look, such as the log.
        hint: Option<String>,
    },
}

/// Starts rlxd for `home` in the background, detached from this terminal, and returns its
/// `status` reply once it takes a connection. `adjust` changes its command first, such as
/// its environment.
pub fn start(home: &Home, adjust: impl FnOnce(&mut Command)) -> Result<Reply, StartError> {
    let failed = |message: String, hint: Option<String>| StartError::Failed { message, hint };
    match lock::is_held(&home.lock()) {
        Ok(true) => return Err(StartError::AlreadyRunning),
        Ok(false) => {}
        Err(e) => {
            let message = format!("cannot read {}: {e}", home.lock().display());
            return Err(failed(message, None));
        }
    }
    let command = background_command().map(|mut command| {
        adjust(&mut command);
        command
    });
    let mut child = match command.and_then(|mut c| c.spawn()) {
        Ok(child) => child,
        Err(e) => {
            let message = format!("cannot start rlxd in the background: {e}");
            return Err(failed(message, None));
        }
    };
    let deadline = Instant::now() + START_WAIT;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            if lock::is_held(&home.lock()).unwrap_or(false) {
                return Err(StartError::AlreadyRunning);
            }
            let mut message = format!("rlxd exited ({status}) before it took a connection");
            if let Some(said) = last_error(home) {
                message = format!("{message}: {said}");
            }
            return Err(failed(message, Some(see_the_log(home))));
        }
        if let Ok(reply) = cli::ask(home, methods::STATUS) {
            let pid = reply
                .data()
                .and_then(|data| data.get("pid"))
                .and_then(serde_json::Value::as_u64);
            if pid == Some(u64::from(child.id())) {
                return Ok(reply);
            }
            if child.try_wait().ok().flatten().is_some() || pid.is_some() {
                // Another rlxd took the home first; this one exits on its own.
                wait_for_loser(&mut child);
                return Err(StartError::AlreadyRunning);
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let message = format!(
                "rlxd did not take a connection within {} s, and was stopped",
                START_WAIT.as_secs()
            );
            return Err(failed(message, Some(see_the_log(home))));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Waits for `child`, a start that lost the home to another rlxd, to give up, which it
/// does once its [`LOCK_WAIT`] ends. So when [`start`] returns, no rlxd it started is
/// still waiting for the lock. One that still runs after that took the home after all,
/// from an rlxd that stopped meanwhile, and is stopped: nothing runs that the caller was
/// told it did not start.
fn wait_for_loser(child: &mut Child) {
    let deadline = Instant::now() + LOCK_WAIT + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// `rlxd serve` in the background: no stdin, its output to nowhere (it logs to its file),
/// in a process group or console of its own, so the terminal's signals do not reach it.
fn background_command() -> io::Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["serve", BACKGROUND_FLAG])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// DETACHED_PROCESS: no console. CREATE_NEW_PROCESS_GROUP: no Ctrl-C from ours.
        const FLAGS: u32 = 0x0000_0008 | 0x0000_0200;
        command.creation_flags(FLAGS);
        crate::windows::keep_std_handles_from_children();
    }
    Ok(command)
}

fn already_running(home: &Home) -> ExitCode {
    let message = format!("rlxd is already running for {}", home.root().display());
    out::fail(
        codes::ALREADY_RUNNING,
        &message,
        Some("`rlxd status` describes it, and `rlxd stop` stops it."),
    )
}

/// The message of the log's last `error` entry, which says why a start failed.
fn last_error(home: &Home) -> Option<String> {
    let log = fs::read_to_string(home.log_file()).ok()?;
    log.lines()
        .rev()
        .find_map(|line| line.split_once(" error ").map(|(_, said)| said.to_owned()))
}

fn see_the_log(home: &Home) -> String {
    format!("Its log is {}.", home.log_file().display())
}

/// Makes the home and rlxd's folders in it, each of rlxd's 0700.
fn prepare(home: &Home) -> io::Result<()> {
    fs::create_dir_all(home.root())?;
    for dir in [home.run_dir(), home.rlxd_dir(), home.logs()] {
        private_dir(&dir)?;
    }
    Ok(())
}

/// Makes `dir`, 0700 on POSIX, narrowing a wider one.
pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(dir)?.permissions().mode() & 0o777 != 0o700 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}
