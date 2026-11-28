//! `rlxd serve [--detach]`: one rlxd per home (handoff 0.10.3, 1.3).
//!
//! In order, a start:
//!
//! 1. makes the home's `run/`, `rlxd/` and `rlxd/logs/` (each 0700);
//! 2. takes `run/rlxd.lock`, or exits 1 with `already_running`;
//! 3. opens the log, makes the token once, and reads the config and the paired gate
//!    devices (`gate.json`);
//! 4. opens the event bus, which numbers on from `rlxd/seq` (`crate::bus`);
//! 5. listens on the socket or pipe;
//! 6. registers in `<home>/daemons/` (4.1), with a heartbeat every 5 s;
//! 7. starts the resident Python brain (`crate::brain`);
//! 8. publishes `daemon.started`, and serves until `stop`, SIGTERM or Ctrl-C.
//!
//! A stop publishes `daemon.stopping`, gives the connections a moment to send it, stops
//! the brain and its tree, writes the bus's last seq, removes the socket, leaves the
//! registry, then lets the lock go, so
//! once the lock is free everything else is done. `--detach` starts the same in the background and returns once it takes
//! a connection and its brain runs (or has said why not), printing its `status`.

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

use crate::brain::Brain;
use crate::bus::Bus;
use crate::config::Config;
use crate::env::Env;
use crate::gate::Gate;
use crate::listener::Listener;
use crate::log::{error, info, warning};
use crate::registry::{self, Registration};
use crate::server::{self, Daemon};
use crate::settings::Settings;
use crate::{cli, lock, log, out, peer, token};

/// The hidden flag a detached rlxd is started with: log to the file only.
pub const BACKGROUND_FLAG: &str = "--background";

/// The flag for gate dev mode (handoff 0.10.3, 5.1), passed on to a detached rlxd. Only a
/// build with the `gate-dev-mode` feature has it.
#[cfg(feature = "gate-dev-mode")]
pub const GATE_DEV_MODE_FLAG: &str = "--gate-dev-mode";

/// How long a start waits for the lock while another process holds it: a probe takes it
/// for an instant (`lock::is_held`), and must not make a start fail.
const LOCK_WAIT: Duration = Duration::from_secs(1);

/// How long `--detach` waits for the new rlxd to take a connection.
const START_WAIT: Duration = Duration::from_secs(15);

/// How long a stopping rlxd gives its connections to finish.
const FINISH_WAIT: Duration = Duration::from_secs(3);

/// How long rlxd waits, at a stop, for the connections it aborted to be dropped.
const ABORT_WAIT: Duration = Duration::from_secs(1);

/// `rlxd serve`, in the foreground: logs to stderr too, unless `background`. With
/// `gate_dev_mode`, or `gate.dev_mode` in the config, an unsigned app may be the gate, in
/// a build with the `gate-dev-mode` feature only: a release build ignores the config's
/// key, and never gets the flag.
pub fn serve(background: bool, gate_dev_mode: bool) -> ExitCode {
    if background {
        close_inherited();
    }
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
    log::log_panics();
    info!(
        "rlxd {VERSION} starting: pid {}, home {}",
        std::process::id(),
        home.root().display()
    );
    #[cfg(windows)]
    if std::env::var_os(IN_JOB_VAR).is_some() {
        warning!("{IN_JOB}; started inside it, rlxd ends when that job is closed");
    }
    let result = token::ensure(&home.token())
        .map_err(|e| format!("cannot make {}: {e}", home.token().display()))
        .and_then(|token| {
            let config = Config::load(&home.config())?;
            let gate = open_gate(&home, dev_mode(gate_dev_mode, &config))?;
            run(home, token, gate, dev_flags(gate_dev_mode))
        });
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

/// Whether the gate runs in dev mode: asked for by the flag or the config, in a build that
/// has it.
#[cfg(feature = "gate-dev-mode")]
fn dev_mode(flag: bool, config: &Config) -> bool {
    flag || config.gate_dev_mode
}

/// A release build has no gate dev mode: the config's key is ignored, and said to be.
#[cfg(not(feature = "gate-dev-mode"))]
fn dev_mode(_flag: bool, config: &Config) -> bool {
    if config.gate_dev_mode {
        warning!(
            "ignoring gate.dev_mode in rlxd's config: this rlxd is a release build, which \
             has no dev mode, so only a signed app may be the gate"
        );
    }
    false
}

/// The flags a restarted rlxd needs, as the registry keeps them: gate dev mode's, when on.
fn dev_flags(gate_dev_mode: bool) -> &'static [&'static str] {
    #[cfg(feature = "gate-dev-mode")]
    if gate_dev_mode {
        return &[GATE_DEV_MODE_FLAG];
    }
    let _ = gate_dev_mode;
    &[]
}

/// The gate, as gate.json says, logging loudly when it runs in dev mode.
fn open_gate(home: &Home, dev_mode: bool) -> Result<Gate, String> {
    let gate = Gate::load(&home.gate(), dev_mode)?;
    if dev_mode {
        warning!(
            "gate dev mode is ON: an unsigned app is accepted as the gate. The gate \
             credential is still required, but before the first pairing any local client \
             can pair. It is for development and CI only; never run rlx like this."
        );
    } else {
        info!("gate: signed app only");
    }
    peer::log_test_override();
    Ok(gate)
}

/// Serves until a stop, on a runtime of its own.
fn run(
    home: Home,
    token: String,
    gate: Gate,
    flags: &'static [&'static str],
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the runtime: {e}"))?;
    let result = runtime.block_on(serve_until_stopped(home, token, gate, flags));
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

async fn serve_until_stopped(
    home: Home,
    token: String,
    gate: Gate,
    flags: &'static [&'static str],
) -> Result<(), String> {
    let bus = Bus::open(&home)?;
    let mut listener = Listener::bind(&home).map_err(|e| e.to_string())?;
    let endpoint = home.endpoint();
    let registration = match Registration::register(&home, flags) {
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
    let keeper = Keeper::start(home.clone(), registration, flags);
    let settings = Settings::open(&home);
    let env = Arc::new(Env::open(&home));
    let brain = Brain::start(&home, env.subscribe())?;
    let daemon = Daemon::new(home, token, listener.acl(), gate, bus, settings, env, brain);
    daemon.started();
    tokio::spawn(watch_signals(Arc::clone(&daemon)));
    info!("listening on {endpoint}");
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer, hangup)) => {
                    connections.spawn(server::serve_connection(
                        Arc::clone(&daemon),
                        stream,
                        peer,
                        hangup,
                    ));
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
        // An aborted connection drops what it was doing, which undoes a pairing whose
        // reply it was still sending (`server::send_paired`): let that happen first.
        let dropped = async { while connections.join_next().await.is_some() {} };
        if tokio::time::timeout(ABORT_WAIT, dropped).await.is_err() {
            warning!("connections still not closed after their abort");
        }
    }
    daemon.brain().stop().await;
    // No PTY outlives rlxd: each process and its group end before it does.
    let ending = Arc::clone(&daemon);
    if tokio::task::spawn_blocking(move || ending.end_ptys())
        .await
        .is_err()
    {
        warning!("the ptys could not all be ended");
    }
    daemon.flush();
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
    fn start(
        home: Home,
        mut registration: Option<Registration>,
        flags: &'static [&'static str],
    ) -> Self {
        let (stop, stopped) = mpsc::channel();
        let thread = thread::spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(registry::HEARTBEAT) {
                match registration.as_mut() {
                    Some(registration) => {
                        if let Err(e) = registration.beat() {
                            warning!("the heartbeat could not be written: {e}");
                        }
                    }
                    None => match Registration::register(&home, flags) {
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
pub fn detach(gate_dev_mode: bool) -> ExitCode {
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(e) => return out::fail(codes::START_FAILED, &e.to_string(), None),
    };
    // Gate dev mode is the caller's choice, passed on; `rlxd here --start` never asks.
    let adjust = |command: &mut Command| {
        command.args(dev_flags(gate_dev_mode));
    };
    match start(&home, adjust) {
        Ok(reply) => out::print_reply(&brain_settled(&home, reply)),
        Err(StartError::AlreadyRunning) => already_running(&home),
        Err(StartError::Failed { message, hint }) => {
            out::fail(codes::START_FAILED, &message, hint.as_deref())
        }
    }
}

/// How long `--detach` waits for the brain to run, or to fail.
const BRAIN_WAIT: Duration = Duration::from_secs(35);

/// The `status` once the new rlxd's brain (`crate::brain`) runs, or says why not: `--detach`
/// returns then, so the status it prints, and the next one asked, name the brain. `reply`
/// as it was if rlxd stops answering meanwhile.
fn brain_settled(home: &Home, mut reply: Reply) -> Reply {
    let deadline = Instant::now() + BRAIN_WAIT;
    loop {
        let brain = reply.data().and_then(|data| data.get("brain"));
        let settled =
            brain.is_none_or(|brain| brain.get("pid").is_some() || brain.get("error").is_some());
        if settled || Instant::now() >= deadline {
            return reply;
        }
        thread::sleep(Duration::from_millis(25));
        match cli::ask(home, methods::STATUS) {
            Ok(next) if next.is_ok() => reply = next,
            _ => return reply,
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
/// its environment (each time it is made: on Windows it may be made twice,
/// [`background_command`]).
pub fn start(home: &Home, adjust: impl Fn(&mut Command)) -> Result<Reply, StartError> {
    let failed = |message: String, hint: Option<String>| StartError::Failed { message, hint };
    match lock::is_held(&home.lock()) {
        Ok(true) => return Err(StartError::AlreadyRunning),
        Ok(false) => {}
        Err(e) => {
            let message = format!("cannot read {}: {e}", home.lock().display());
            return Err(failed(message, None));
        }
    }
    // Only what this start writes to the log can say why it failed.
    let logged_before = fs::metadata(home.log_file()).map_or(0, |m| m.len());
    let spawn = |leave_job: bool| {
        background_command(leave_job).and_then(|mut command| {
            adjust(&mut command);
            command.spawn()
        })
    };
    let spawned = spawn(true);
    // A Job Object that lets no process break away refuses the start: rlxd starts in it,
    // as it did before it could leave one.
    #[cfg(windows)]
    let spawned = match spawned {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
            warning!("{IN_JOB}; rlxd is started inside it, and ends when that job is closed");
            spawn(false)
        }
        spawned => spawned,
    };
    let mut child = match spawned {
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
            if let Some(said) = last_error(home, logged_before) {
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
/// so nothing that ends its starter ends it (task #38: a background task of an interactive
/// Claude Code session is ended at about 26 to 30 minutes, with everything it started).
///
/// - **POSIX:** a session of its own (`setsid`, and so a process group of its own) with no
///   controlling terminal: neither a hangup nor a signal to the starter's session or
///   process group reaches it. It closes, first, any other descriptor it inherits
///   ([`close_inherited`]).
/// - **Windows:** no console, a process group of its own, and, with `leave_job`, out of
///   the starter's Job Object (`CREATE_BREAKAWAY_FROM_JOB`), so closing a kill-on-close job,
///   such as the one Node gives its children, does not end it. A job that lets no process
///   break away makes the start fail with `ERROR_ACCESS_DENIED`; [`start`] then starts it
///   without `leave_job`, inside that job.
fn background_command(leave_job: bool) -> io::Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["serve", BACKGROUND_FLAG])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let _ = leave_job; // a session of its own leaves whatever held the starter's
        // SAFETY: setsid is async-signal-safe, and is all that runs between fork and exec.
        #[allow(unsafe_code)]
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// DETACHED_PROCESS: no console. CREATE_NEW_PROCESS_GROUP: no Ctrl-C from ours.
        const FLAGS: u32 = 0x0000_0008 | 0x0000_0200;
        /// CREATE_BREAKAWAY_FROM_JOB: out of the starter's Job Object.
        const LEAVE_JOB: u32 = 0x0100_0000;
        command.creation_flags(if leave_job { FLAGS | LEAVE_JOB } else { FLAGS });
        if !leave_job {
            command.env(IN_JOB_VAR, "1"); // for its log
        }
        crate::windows::keep_std_handles_from_children();
    }
    Ok(command)
}

/// What `CreateProcessW` fails with when the caller's Job Object lets no process break
/// away from it (winerror.h).
#[cfg(windows)]
const ERROR_ACCESS_DENIED: i32 = 5;

/// Set for an rlxd [`start`] could not take out of its starter's Job Object, which says so
/// in its log as it starts.
#[cfg(windows)]
const IN_JOB_VAR: &str = "RLXD_STARTED_IN_JOB";

/// What is logged, by the starter and by rlxd, when rlxd could not leave the starter's job.
#[cfg(windows)]
const IN_JOB: &str = "the Job Object rlxd serve --detach runs in lets no process break away";

/// Closes every descriptor above stderr this process was started with. A detached rlxd
/// outlives the process that starts it, so a descriptor it inherited would stay open as
/// long as rlxd runs; if that were a pipe the starter's caller reads to its end, such as
/// `rlxd here --start`'s stdout, the caller would wait for rlxd to stop. Each descriptor
/// Rust opens is close-on-exec, but a process may still pass one on: on macOS a pipe is
/// made close-on-exec just after it is made, so a program that starts two commands at
/// once can give one command the other's pipe, and that command its rlxd.
///
/// It runs first, before rlxd opens anything, and lists the descriptors in
/// `/proc/self/fd` (Linux) or `/dev/fd` (macOS).
#[cfg(unix)]
#[allow(unsafe_code)]
fn close_inherited() {
    let Some(listing) = ["/proc/self/fd", "/dev/fd"]
        .into_iter()
        .find_map(|dir| fs::read_dir(dir).ok())
    else {
        return;
    };
    let inherited: Vec<libc::c_int> = listing
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .filter(|fd| *fd > 2)
        .collect();
    // The listing's own descriptor is among them, and closed by now: closing it again
    // fails, and changes nothing.
    for fd in inherited {
        // SAFETY: nothing in this process owns a descriptor above 2 yet, so none is
        // closed under its owner.
        unsafe { libc::close(fd) };
    }
}

/// Windows passes a child only the handles marked inheritable, and a detached rlxd's
/// starter marks its stdio not ([`crate::windows::keep_std_handles_from_children`]).
#[cfg(not(unix))]
fn close_inherited() {}

fn already_running(home: &Home) -> ExitCode {
    let message = format!("rlxd is already running for {}", home.root().display());
    out::fail(
        codes::ALREADY_RUNNING,
        &message,
        Some("`rlxd status` describes it, and `rlxd stop` stops it."),
    )
}

/// The message of the log's last `error` entry, which says why a start failed.
/// The message of the last `error` entry written to the log past its first `since` bytes:
/// what this start said. A log that rotated meanwhile is read whole.
fn last_error(home: &Home, since: u64) -> Option<String> {
    let log = fs::read(home.log_file()).ok()?;
    let start = usize::try_from(since).unwrap_or(0);
    let new = log.get(start..).unwrap_or(&log);
    String::from_utf8_lossy(new)
        .lines()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_error_reads_only_what_this_start_wrote() {
        let root = std::env::temp_dir().join(format!("rlxd-serve-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = Home::at(&root);
        fs::create_dir_all(home.logs()).unwrap();
        let earlier = "2026-11-26T12:00:00.000Z error rlxd could not run: an earlier start\n";
        fs::write(home.log_file(), earlier).unwrap();
        let since = fs::metadata(home.log_file()).unwrap().len();
        assert_eq!(
            last_error(&home, 0).as_deref(),
            Some("rlxd could not run: an earlier start")
        );
        assert_eq!(
            last_error(&home, since),
            None,
            "this start has said nothing yet"
        );
        let mut log = fs::read_to_string(home.log_file()).unwrap();
        log.push_str("2026-11-26T12:00:01.000Z info rlxd 0.10.1 starting\n");
        log.push_str("2026-11-26T12:00:01.100Z error rlxd panicked: at bus.rs\n");
        fs::write(home.log_file(), log).unwrap();
        assert_eq!(
            last_error(&home, since).as_deref(),
            Some("rlxd panicked: at bus.rs")
        );
        // A log that rotated meanwhile is shorter than where this start began: read whole.
        fs::write(
            home.log_file(),
            "2026-11-26T12:00:02.000Z error after rotation\n",
        )
        .unwrap();
        assert_eq!(last_error(&home, since).as_deref(), Some("after rotation"));
        fs::remove_dir_all(&root).unwrap();
    }
}
