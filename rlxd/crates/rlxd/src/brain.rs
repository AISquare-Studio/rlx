//! The resident Python brain (handoff 0.10.3, 1.6): rlxd starts one `rlx brain --stdio`
//! per home, keeps it running, and asks it what Python decides, so no Python process
//! starts for each question.
//!
//! - **Which rlx.** The `rlx` installed beside this rlxd, else in the install folders,
//!   else in an absolute folder on `PATH`: `rlxd here`'s lookup
//!   (`rlxd_client::install::find_rlx`), resolved to an absolute path at each start.
//!   `RLXD_RLX_BIN`, for tests and development, names it instead. None found is
//!   `brain_not_found`, tried again with backoff.
//! - **The protocol** is rlxd-proto's `BrainInput` and `BrainOutput`, JSON lines on the
//!   brain's stdin and stdout, opened by a hello naming the brain protocol each side
//!   speaks. A brain of another release, which shares none, or says anything this rlxd
//!   cannot read, is `brain_incompatible` in `status`, never a misparse.
//! - **One request at a time.** Requests wait in a queue and the brain runs them one
//!   after another, which is what keeps them apart: each may change the brain's process
//!   state (its environment, its directory) for its own run.
//! - **Accepted is the line.** The brain says `accepted` as soon as it has read a request,
//!   before it runs anything. A request it never accepted is [`CallError::NotDelivered`]:
//!   it never ran, and the caller may run it elsewhere. That covers a request still
//!   queued when its accept window ends (withdrawn, so it never runs), a brain that is
//!   missing, starting or waiting to restart, and one that died or broke the protocol
//!   before accepting. "Never accepted" is what the brain wrote, not only what rlxd read
//!   in time: a brain rlxd has not seen accept within [`ACK_WAIT`] of the write is killed
//!   and reaped, then every frame it had written is read (`refuse`), and an `accepted`
//!   among them makes the request one that may have run. Once accepted, a failure is
//!   [`CallError::Failed`] and an overrun [`CallError::TimedOut`] (the brain is killed and
//!   started again, so the queue moves): the request may have run, and must not run
//!   again.
//! - **Supervision.** A brain that ends is started again after a backoff that doubles
//!   from 250 ms to [`BACKOFF_CAP`] with each start that fails, and starts over once a
//!   brain has run [`HEALTHY`] from its hello. `status.brain` says its pid, `restarts`, `handled`, how
//!   many requests wait (`queued`, at most [`QUEUE_MOST`]: one more is not delivered), and
//!   while none runs, why (`error`) and how long until the next start (`backoff_ms`).
//! - **Its tree** (`process`): a process group on POSIX, a Job Object on Windows. When
//!   rlxd stops, the brain's stdin closes, which ends it; after [`STOP_GRACE`] its tree
//!   is signalled, then killed. If rlxd dies, the brain sees its stdin end and exits (and
//!   on Windows the job's end kills it).
//! - **Its stderr** goes to `<home>/rlxd/logs/brain.log`, 1 MiB at most, one old log kept.
//! - **The registry.** While a brain runs, rlxd registers it in `<home>/daemons/` as kind
//!   `brain` (4.1), with the brain's pid and `argv`, not restartable (rlxd restarts it,
//!   not an upgrade). rlxd holds the record's lock itself, rather than the brain: it is
//!   the brain's parent and the record's one writer, so it takes the lock before the
//!   record exists (no reader can see a fresh record with a free lock and prune it),
//!   writes the record only once the brain has said a hello this rlxd speaks, keeps it
//!   on a heartbeat of its own (every 5 s, a long hook or not, registering again at a
//!   beat if it could not at first), and removes it the moment it sees the brain end. If rlxd dies,
//!   the OS frees the lock, and the brain, its stdin closed, ends too.
//! - **Its environment** is rlxd's, plus the app's `PATH`, `CLAUDE_CONFIG_DIR` and
//!   `CLAUDE_SECURESTORAGE_CONFIG_DIR` (4.3), raw, those the app set, as they are when the
//!   brain starts: what a run's Claude Code gets (`crate::runner::run_env`). A change
//!   that alters them (`env.set`, an app `hello`'s `env`) starts the brain again with the
//!   new values once it is idle: the request it runs ends first, and the next request,
//!   queued or not, runs on the new brain. It is stopped as rlxd stops it (its stdin
//!   closed, then signalled, then killed) and started at once, with no backoff and no
//!   `error`: `restarts` counts it. A change while the brain starts, or waits to restart,
//!   reaches it the same way, since each start takes the values then.

mod process;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rlxd_client::{Home, install};
use rlxd_proto::{
    BRAIN_PROTOCOL, BRAIN_PROTOCOLS, BrainInput, BrainOutput, Data, EnvValues, ErrorBody, VERSION,
    brain_codes, codes, daemon_kinds,
};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout, timeout_at};

use crate::log::{LogFile, info, warning};
use crate::registry::{self, Registration};
use process::Process;

/// How long a new brain has to say its hello: Python's imports, on a slow disk.
const HELLO_WAIT: Duration = Duration::from_secs(30);

/// How long the brain has to say `accepted` once a request is written.
pub const ACK_WAIT: Duration = Duration::from_secs(2);

/// How long a stopping rlxd lets the brain end on its own, and then after a SIGTERM.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// The first wait before a brain is started again.
const BACKOFF_FIRST: Duration = Duration::from_millis(250);

/// The longest wait before a brain is started again.
pub const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// How long a brain must run for the next start to wait [`BACKOFF_FIRST`] again.
const HEALTHY: Duration = Duration::from_secs(30);

/// The brain's log, and its size and old logs kept.
const LOG_NAME: &str = "brain.log";
const LOG_MAX: u64 = 1 << 20;
const LOG_KEEP: u32 = 1;

/// The errors no restart mends, only an install or an upgrade: while one stands, a call
/// is refused at once rather than wait for a brain.
const LASTING: [&str; 3] = [
    brain_codes::BRAIN_NOT_FOUND,
    brain_codes::BRAIN_START_FAILED,
    brain_codes::BRAIN_INCOMPATIBLE,
];

/// The most requests that wait for the brain at once. One more is refused at once, never
/// delivered, so a wedged brain cannot gather every hook of every session behind it.
pub const QUEUE_MOST: usize = 64;

/// Where a queued request stands, which its caller and the supervisor race to change.
const QUEUED: u8 = 0;
const TAKEN: u8 = 1;
const WITHDRAWN: u8 = 2;

/// Why a call has no result.
#[derive(Debug)]
pub enum CallError {
    /// The brain never accepted the request, so nothing of it ran: the caller may run it
    /// elsewhere.
    NotDelivered(ErrorBody),
    /// The brain accepted the request, then it failed: it may have run, partly.
    Failed(ErrorBody),
    /// The brain accepted the request and did not answer in time: it may have run.
    TimedOut,
}

/// The brain, as rlxd keeps it: its state for `status`, and the queue its requests wait in.
pub struct Brain {
    shared: Arc<Shared>,
    queued: Arc<AtomicUsize>,
    jobs: mpsc::UnboundedSender<Job>,
    stop: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<()>>>,
}

/// What the supervisor and `status` share.
struct Shared {
    home: Home,
    state: Mutex<rlxd_proto::Brain>,
    log: Arc<Mutex<LogFile>>,
}

/// A request waiting for the brain.
struct Job {
    method: String,
    claim: Arc<Claim>,
    accept_by: Instant,
    deadline: Instant,
    reply: oneshot::Sender<Result<Data, CallError>>,
}

/// Where a request stands, the count of requests still waiting, which whoever moves it out
/// of [`QUEUED`] lowers, once, and its params, which a withdrawal drops at once: a withdrawn
/// request waits in the channel for the supervisor, but holds no hook's payload there.
struct Claim {
    state: AtomicU8,
    queued: Arc<AtomicUsize>,
    params: Mutex<Option<Data>>,
}

impl Claim {
    /// Moves the request from [`QUEUED`] to `to`: whether this call did.
    fn leave_queue(&self, to: u8) -> bool {
        let moved = self
            .state
            .compare_exchange(QUEUED, to, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if moved {
            self.queued.fetch_sub(1, Ordering::SeqCst);
            if to == WITHDRAWN {
                drop(self.take_params());
            }
        }
        moved
    }

    fn take_params(&self) -> Data {
        self.params
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or_default()
    }

    fn state(&self) -> u8 {
        self.state.load(Ordering::SeqCst)
    }
}

impl Job {
    /// Takes the job for the brain: `false`, and it never runs, when its caller withdrew
    /// it, left, or its accept window has ended.
    fn take(&self) -> bool {
        let next = if self.reply.is_closed() || Instant::now() >= self.accept_by {
            WITHDRAWN
        } else {
            TAKEN
        };
        self.claim.leave_queue(next) && next == TAKEN
    }

    fn answer(self, outcome: Result<Data, CallError>) {
        let _ = self.reply.send(outcome);
    }
}

/// How one brain's run ended.
enum Ended {
    /// rlxd is stopping.
    Stopping,
    /// The app's environment changed: it was stopped, to start again with the new values.
    EnvChanged,
    /// It ended or was stopped: why, as `status.brain.error` says.
    Failed(ErrorBody),
}

impl Brain {
    /// Starts supervising the home's brain, on the current runtime: it starts at once.
    /// `env` is the app's environment (`crate::env::Env::subscribe`): each start adds the
    /// values then to rlxd's environment, and a change starts the brain again once idle.
    pub fn start(home: &Home, env: watch::Receiver<EnvValues>) -> Result<Arc<Self>, String> {
        let log_path = home.logs().join(LOG_NAME);
        let log = LogFile::open(&log_path, LOG_MAX, LOG_KEEP)
            .map_err(|e| format!("cannot open {}: {e}", log_path.display()))?;
        let shared = Arc::new(Shared {
            home: home.clone(),
            state: Mutex::new(rlxd_proto::Brain::default()),
            log: Arc::new(Mutex::new(log)),
        });
        let (jobs, queue) = mpsc::unbounded_channel();
        let (stop, stopping) = watch::channel(false);
        let task = tokio::spawn(supervise(Arc::clone(&shared), queue, stopping, env));
        Ok(Arc::new(Self {
            shared,
            queued: Arc::new(AtomicUsize::new(0)),
            jobs,
            stop,
            task: Mutex::new(Some(task)),
        }))
    }

    /// `status.brain`.
    pub fn status(&self) -> rlxd_proto::Brain {
        let mut status = self.shared.state().clone();
        status.queued = self.queued.load(Ordering::SeqCst) as u64;
        status
    }

    /// Asks the brain `method` with `params`. The brain must accept it within
    /// `accept_within`, or it is withdrawn ([`CallError::NotDelivered`]), and answer it
    /// within `run_within`, both from now.
    pub async fn call(
        &self,
        method: &str,
        params: Data,
        accept_within: Duration,
        run_within: Duration,
    ) -> Result<Data, CallError> {
        if let Some(error) = self.shared.state().error.clone()
            && LASTING.contains(&error.code.as_str())
        {
            // No brain will run before rlx is installed or upgraded: do not wait for one.
            return Err(CallError::NotDelivered(error));
        }
        let waiting = self.queued.fetch_add(1, Ordering::SeqCst);
        let claim = Arc::new(Claim {
            state: AtomicU8::new(QUEUED),
            queued: Arc::clone(&self.queued),
            params: Mutex::new(Some(params)),
        });
        if waiting >= QUEUE_MOST {
            claim.leave_queue(WITHDRAWN);
            return Err(not_delivered(&format!(
                "the brain's queue already holds {QUEUE_MOST} requests"
            )));
        }
        let now = Instant::now();
        let accept_by = now + accept_within;
        let deadline = now + run_within.max(accept_within);
        let (reply, mut answer) = oneshot::channel();
        let job = Job {
            method: method.to_owned(),
            claim: Arc::clone(&claim),
            accept_by,
            deadline,
            reply,
        };
        if self.jobs.send(job).is_err() {
            claim.leave_queue(WITHDRAWN);
            return Err(not_delivered("rlxd is stopping, and its brain with it"));
        }
        match timeout_at(accept_by, &mut answer).await {
            Ok(Ok(outcome)) => return outcome,
            Ok(Err(_)) => return Err(dropped(&claim)),
            Err(_) => {}
        }
        if claim.leave_queue(WITHDRAWN) {
            let why = match self.shared.state().error.clone() {
                Some(error) => format!("no brain took it in time: {}", error.message),
                None => "no brain took it in time".to_owned(),
            };
            return Err(not_delivered(&why));
        }
        // Taken, or withdrawn by the supervisor: it answers, by the ack window or the
        // deadline; the margin covers killing a brain that overran.
        match timeout_at(deadline + ACK_WAIT + STOP_GRACE, answer).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(dropped(&claim)),
            Err(_) => Err(CallError::TimedOut),
        }
    }

    /// Stops the brain and its tree, and waits until they are gone.
    pub async fn stop(&self) {
        self.stop.send_replace(true);
        let task = self
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

/// How long `hook.ping` waits for the brain to take it, and to answer.
const PING_ACCEPT: Duration = Duration::from_secs(5);
const PING_RUN: Duration = Duration::from_secs(10);

/// `hook.ping`: the brain's `{pid}`, or why it did not answer.
pub async fn ping(brain: &Brain) -> Result<Data, ErrorBody> {
    match brain.call("ping", Data::new(), PING_ACCEPT, PING_RUN).await {
        Ok(data) => Ok(data),
        Err(CallError::NotDelivered(error) | CallError::Failed(error)) => Err(error),
        Err(CallError::TimedOut) => Err(failure(
            codes::TIMED_OUT,
            "the brain did not answer in time, and was started again",
        )),
    }
}

/// A job dropped unanswered: never taken means never run.
fn dropped(claim: &Claim) -> CallError {
    if claim.state() == TAKEN {
        CallError::Failed(failure(
            codes::INTERNAL,
            "the brain's supervisor dropped a request it had taken",
        ))
    } else {
        not_delivered("rlxd is stopping, and its brain with it")
    }
}

fn failure(code: &str, message: &str) -> ErrorBody {
    ErrorBody::new(code, message).unwrap_or_else(|_| {
        ErrorBody::new(codes::INTERNAL, "an error with no message").expect("a valid error")
    })
}

fn not_delivered(why: &str) -> CallError {
    CallError::NotDelivered(failure(brain_codes::BRAIN_UNRESPONSIVE, why))
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, rlxd_proto::Brain> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Starts the brain, and starts it again whenever it ends, until rlxd stops.
async fn supervise(
    shared: Arc<Shared>,
    mut queue: mpsc::UnboundedReceiver<Job>,
    mut stopping: watch::Receiver<bool>,
    mut env: watch::Receiver<EnvValues>,
) {
    let mut failures: u32 = 0;
    let mut started_once = false;
    loop {
        if *stopping.borrow() {
            break;
        }
        let (ended, ready) = run(
            &shared,
            &mut queue,
            &mut stopping,
            &mut env,
            &mut started_once,
        )
        .await;
        let error = match ended {
            Ended::Stopping => break,
            // Not a failure: the brain that ran was well, so the next start waits nothing.
            Ended::EnvChanged => {
                failures = 0;
                continue;
            }
            Ended::Failed(error) => error,
        };
        // Healthy is a run from a good hello, not from the start: a brain that hangs
        // before its hello is never healthy, and its backoff grows.
        failures = if ready.is_some_and(|at| at.elapsed() >= HEALTHY) {
            1
        } else {
            failures.saturating_add(1)
        };
        let wait = backoff(failures);
        warning!(
            "brain: {} ({}); starting it again in {} ms",
            error.message,
            error.code,
            wait.as_millis()
        );
        {
            let mut state = shared.state();
            state.pid = None;
            state.error = Some(error);
            state.backoff_ms = Some(u64::try_from(wait.as_millis()).unwrap_or(u64::MAX));
        }
        let waited = tokio::select! {
            () = sleep(wait) => true,
            () = stopped(&mut stopping) => false,
        };
        shared.state().backoff_ms = None;
        if !waited {
            break;
        }
    }
    shared.state().pid = None;
    queue.close();
    while let Some(job) = queue.recv().await {
        let _ = job.take();
        job.answer(Err(not_delivered(
            "rlxd is stopping, and its brain with it",
        )));
    }
}

/// The wait before start number `failures` + 1: 250 ms, doubling, at most [`BACKOFF_CAP`].
fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    (BACKOFF_FIRST * 2u32.pow(doublings)).min(BACKOFF_CAP)
}

/// One brain, from its start to its end: how it ended, and when it said a good hello.
async fn run(
    shared: &Shared,
    queue: &mut mpsc::UnboundedReceiver<Job>,
    stopping: &mut watch::Receiver<bool>,
    env: &mut watch::Receiver<EnvValues>,
    started_once: &mut bool,
) -> (Ended, Option<Instant>) {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from));
    let Some(program) = install::rlx_to_run(beside.as_deref()) else {
        let message = match std::env::var_os(install::RLX_BIN_ENV).filter(|v| !v.is_empty()) {
            Some(named) => format!(
                "{} names no program: {}",
                install::RLX_BIN_ENV,
                PathBuf::from(named).display()
            ),
            None => "no rlx was found beside rlxd, in the install folders or on PATH: \
                     reinstall rlx"
                .to_owned(),
        };
        let error = failure(brain_codes::BRAIN_NOT_FOUND, &message);
        return (Ended::Failed(error), None);
    };
    let args = ["brain", "--stdio"];
    // The values now: a change from here on is seen as the brain serves, and starts it again.
    let values = env.borrow_and_update().clone();
    let spawned = Process::spawn(
        &program,
        &args,
        shared.home.root(),
        &crate::runner::run_env(&values),
        Arc::clone(&shared.log),
    );
    let mut brain = match spawned {
        Ok(brain) => brain,
        Err(e) => {
            let message = format!("{} could not start: {e}", program.display());
            let error = failure(brain_codes::BRAIN_START_FAILED, &message);
            return (Ended::Failed(error), None);
        }
    };
    if std::mem::replace(started_once, true) {
        shared.state().restarts += 1;
    }
    info!(
        "brain: started {} brain --stdio, pid {}",
        program.display(),
        brain.pid
    );
    if let Err(ended) = greet(&mut brain, stopping).await {
        end(&mut brain).await;
        return (ended, None);
    }
    let ready = Instant::now();
    let mut argv = vec![program.display().to_string()];
    argv.extend(args.iter().map(|&arg| arg.to_owned()));
    let record = Record::keep(shared.home.clone(), brain.pid, argv);
    {
        let mut state = shared.state();
        state.pid = Some(brain.pid);
        state.error = None;
    }
    let ended = serve(shared, &mut brain, queue, stopping, env, &values).await;
    record.leave();
    shared.state().pid = None;
    (ended, Some(ready))
}

/// The hello exchange: `Err` with why it failed, the brain still to be ended. A line that
/// is not JSON before the hello, such as something printed as the brain started, is
/// logged and passed over; a JSON line that is not a hello of this protocol is a brain of
/// another release.
async fn greet(brain: &mut Process, stopping: &mut watch::Receiver<bool>) -> Result<(), Ended> {
    let hello = BrainInput::hello(VERSION).map_err(|e| unresponsive(&e.to_string()))?;
    if write(brain, &hello).await.is_err() {
        let said = end_of(&mut brain.exit_watch()).await;
        return Err(exited(&format!("it ended before its hello ({said})")));
    }
    let deadline = Instant::now() + HELLO_WAIT;
    let frame = loop {
        let frame = tokio::select! {
            frame = timeout_at(deadline, brain.frames.recv()) => frame,
            () = stopped(stopping) => return Err(Ended::Stopping),
        };
        match frame {
            Ok(Some(Err(unread))) if !unread.json => {
                warning!("brain: before its hello it wrote {unread}: passed over");
            }
            frame => break frame,
        }
    };
    match frame {
        Err(_) => Err(unresponsive(&format!(
            "it said no hello within {} s",
            HELLO_WAIT.as_secs()
        ))),
        Ok(None) => {
            let said = end_of(&mut brain.exit_watch()).await;
            Err(exited(&format!("it ended before its hello ({said})")))
        }
        Ok(Some(Ok(BrainOutput::Hello {
            protocol,
            pid,
            rlx_version,
        }))) if protocol == BRAIN_PROTOCOL => {
            info!(
                "brain: pid {} (python {pid}), rlx {rlx_version}, brain protocol {protocol}",
                brain.pid
            );
            Ok(())
        }
        Ok(Some(Ok(BrainOutput::Hello {
            protocol,
            rlx_version,
            ..
        }))) => Err(incompatible(&format!(
            "rlx {rlx_version} answered brain protocol {protocol}, and this rlxd {VERSION} \
             offered {BRAIN_PROTOCOL}: install the rlx and rlxd of one release"
        ))),
        Ok(Some(Ok(BrainOutput::Refused { message, .. }))) => Err(incompatible(&format!(
            "the brain refused this rlxd {VERSION}: {message}"
        ))),
        Ok(Some(Ok(other))) => Err(incompatible(&format!(
            "it answered the hello with {other:?}"
        ))),
        Ok(Some(Err(unread))) => Err(incompatible(&format!(
            "it answered the hello with {unread}: is it of another release than this \
             rlxd {VERSION}?"
        ))),
    }
}

/// The brain's record in the registry (4.1), kept by a task of its own: written once the
/// brain said its hello, beaten every [`registry::HEARTBEAT`] whatever the brain is doing
/// (a hook may run for two minutes), written again at a beat when it could not be, and
/// left as the brain ends.
struct Record {
    held: Arc<Mutex<Option<Registration>>>,
    task: JoinHandle<()>,
}

impl Record {
    fn keep(home: Home, pid: u32, argv: Vec<String>) -> Self {
        let first = tokio::task::block_in_place(|| register(&home, pid, &argv));
        let held = Arc::new(Mutex::new(first));
        let beating = Arc::clone(&held);
        let task = tokio::spawn(async move {
            let mut beat = tokio::time::interval(registry::HEARTBEAT);
            beat.reset();
            loop {
                beat.tick().await;
                tokio::task::block_in_place(|| {
                    let mut held = beating.lock().unwrap_or_else(PoisonError::into_inner);
                    match held.as_mut() {
                        Some(registration) => {
                            if let Err(e) = registration.beat() {
                                warning!("brain: its heartbeat could not be written: {e}");
                            }
                        }
                        None => *held = register(&home, pid, &argv),
                    }
                });
            }
        });
        Self { held, task }
    }

    /// Stops the beat and leaves the registry.
    fn leave(self) {
        self.task.abort();
        let registration = self
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(registration) = registration {
            tokio::task::block_in_place(|| registration.leave());
        }
    }
}

/// The brain's record in the registry, or `None`: it runs unregistered until a beat
/// registers it, as rlxd's own registration does.
fn register(home: &Home, pid: u32, argv: &[String]) -> Option<Registration> {
    let cwd = Some(home.root().display().to_string());
    let registered =
        Registration::register_process(home, daemon_kinds::BRAIN, pid, argv.to_vec(), false, cwd);
    match registered {
        Ok(registration) => {
            info!("brain: registered as {}", registration.id());
            Some(registration)
        }
        Err(e) => {
            warning!("brain: unregistered, trying again at the next beat: {e}");
            None
        }
    }
}

/// Serves requests until the brain ends, breaks the protocol, overruns, rlxd stops, or the
/// app's environment changes from `started_with`, the values the brain started with.
async fn serve(
    shared: &Shared,
    brain: &mut Process,
    queue: &mut mpsc::UnboundedReceiver<Job>,
    stopping: &mut watch::Receiver<bool>,
    env: &mut watch::Receiver<EnvValues>,
    started_with: &EnvValues,
) -> Ended {
    let mut exit = brain.exit_watch();
    let mut next_id: u64 = 0;
    loop {
        let next = tokio::select! {
            biased;
            () = stopped(stopping) => Next::Stop,
            said = exit.wait() => Next::Ended(said),
            frame = brain.frames.recv() => Next::Unasked(frame),
            // Here, and so only between requests: one running ends first.
            Ok(()) = env.changed() => Next::EnvChanged,
            Some(job) = queue.recv() => Next::Job(job),
        };
        match next {
            Next::Stop => {
                stop(brain).await;
                return Ended::Stopping;
            }
            Next::Ended(said) => return exited(&format!("it ended ({said})")),
            Next::EnvChanged => {
                if *env.borrow_and_update() == *started_with {
                    continue;
                }
                info!("brain: the app's environment changed (4.3): starting it again with it");
                stop(brain).await;
                return Ended::EnvChanged;
            }
            Next::Unasked(frame) => {
                if frame.is_none()
                    && let Ok(said) = timeout(STOP_GRACE, exit.wait()).await
                {
                    // Its stdout closes as it ends, and can be seen first.
                    return exited(&format!("it ended ({said})"));
                }
                let what = match frame {
                    None => "its stdout closed".to_owned(),
                    Some(Ok(frame)) => format!("it said {frame:?} unasked"),
                    Some(Err(unread)) => format!("it said {unread}"),
                };
                end(brain).await;
                return unresponsive(&format!("{what}, and was stopped"));
            }
            Next::Job(job) => {
                if !job.take() {
                    job.answer(Err(not_delivered("its accept window ended in the queue")));
                    continue;
                }
                next_id += 1;
                match run_job(shared, brain, job, next_id, stopping).await {
                    Ok(()) => {}
                    Err(Ended::Stopping) => {
                        stop(brain).await;
                        return Ended::Stopping;
                    }
                    Err(ended) => {
                        end(brain).await;
                        return ended;
                    }
                }
            }
        }
    }
}

/// What woke an idle brain's supervisor.
enum Next {
    /// rlxd stops.
    Stop,
    /// The brain ended, so.
    Ended(String),
    /// It wrote a frame no request asked for, or its stdout ended.
    Unasked(Option<process::Frame>),
    /// The app's environment changed.
    EnvChanged,
    /// A request.
    Job(Job),
}

/// Runs request `id`, answering `job` whatever happens. `Err` when the brain must be
/// ended, or rlxd stops.
async fn run_job(
    shared: &Shared,
    brain: &mut Process,
    job: Job,
    id: u64,
    stopping: &mut watch::Receiver<bool>,
) -> Result<(), Ended> {
    let params = job.claim.take_params();
    let request = match BrainInput::request(id, job.method.clone(), params) {
        Ok(request) => request,
        Err(e) => {
            let error = failure(codes::INVALID_PARAMS, &e.to_string());
            job.answer(Err(CallError::NotDelivered(error)));
            return Ok(());
        }
    };
    let mut exit = brain.exit_watch();
    let ack_by = Instant::now() + ACK_WAIT;
    // Not seen accepted yet: whatever goes wrong now, `refuse` reads what the brain wrote
    // before it decides.
    let written = match timeout_at(ack_by, write(brain, &request)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(unresponsive(&format!(
            "a request could not be written: {e}"
        ))),
        Err(_) => Err(unresponsive("it did not read a request")),
    };
    if let Err(ended) = written {
        return refuse(shared, brain, job, id, ended).await;
    }
    let accepted = tokio::select! {
        frame = timeout_at(ack_by, brain.frames.recv()) => match frame {
            Ok(Some(Ok(BrainOutput::Accepted { id: said }))) if said == id => Ok(()),
            Ok(Some(Ok(frame))) => Err(unresponsive(&format!(
                "it answered request {id} with {frame:?}, not accepted"
            ))),
            Ok(Some(Err(unread))) => Err(unresponsive(&format!("it said {unread}"))),
            Ok(None) => {
                let said = end_of(&mut exit).await;
                Err(exited(&format!("it ended before it accepted a request ({said})")))
            }
            Err(_) => Err(unresponsive(&format!(
                "it did not accept a request within {} ms",
                ACK_WAIT.as_millis()
            ))),
        },
        said = exit.wait() => Err(exited(&format!("it ended ({said})"))),
    };
    if let Err(ended) = accepted {
        return refuse(shared, brain, job, id, ended).await;
    }
    // Accepted: from here on, the request may have run, and is never refused.
    let reply = tokio::select! {
        frame = timeout_at(job.deadline, brain.frames.recv()) => frame,
        said = exit.wait() => {
            let seen = drain(brain, id, true).await;
            answer_after_end(shared, job, seen, &said);
            return Err(exited(&format!("it ended ({said})")));
        }
        () = stopped(stopping) => {
            // rlxd stops: the request has a little longer to end.
            match timeout(STOP_GRACE, brain.frames.recv()).await {
                Ok(Some(Ok(BrainOutput::Reply { id: said, ok, result, error }))) if said == id => {
                    shared.state().handled += 1;
                    job.answer(reply_of(ok, result, error));
                }
                _ => job.answer(Err(CallError::Failed(failure(
                    brain_codes::BRAIN_EXITED,
                    "rlxd stopped while the brain ran it",
                )))),
            }
            return Err(Ended::Stopping);
        }
    };
    let ended = match reply {
        Ok(Some(Ok(BrainOutput::Reply {
            id: said,
            ok,
            result,
            error,
        }))) if said == id => {
            shared.state().handled += 1;
            job.answer(reply_of(ok, result, error));
            return Ok(());
        }
        Ok(Some(Ok(frame))) => unresponsive(&format!("it answered request {id} with {frame:?}")),
        Ok(Some(Err(unread))) => unresponsive(&format!("it said {unread}")),
        Ok(None) => {
            let said = end_of(&mut exit).await;
            answer_after_end(shared, job, Drained::Accepted(None), &said);
            return Err(exited(&format!("it ended running a request ({said})")));
        }
        Err(_) => {
            job.answer(Err(CallError::TimedOut));
            return Err(unresponsive("a request ran past its deadline"));
        }
    };
    if let Ended::Failed(error) = &ended {
        job.answer(Err(CallError::Failed(error.clone())));
    }
    Err(ended)
}

/// Answers a job rlxd did not see accepted, once the brain is gone: killed and reaped
/// first, then every frame it had written read. An `accepted` among them means the request
/// may have run: its reply if there is one, else a failure (the brain ended) or
/// [`CallError::TimedOut`] (it stalled), never a fallback. Otherwise it never ran, and the
/// caller hears [`CallError::NotDelivered`]. Passes on why the brain ended.
async fn refuse(
    shared: &Shared,
    brain: &mut Process,
    job: Job,
    id: u64,
    ended: Ended,
) -> Result<(), Ended> {
    end(brain).await;
    let seen = drain(brain, id, false).await;
    let why = match &ended {
        Ended::Failed(error) => error.message.clone(),
        Ended::Stopping => "rlxd is stopping".to_owned(),
        Ended::EnvChanged => "the app's environment changed".to_owned(),
    };
    if matches!(seen, Drained::Accepted(Some(_))) {
        shared.state().handled += 1;
    }
    let never = not_delivered(&format!("the brain did not accept it: {why}"));
    // Accepted, then: an end while running it is a failure; a stall is an overrun.
    let unanswered = match &ended {
        Ended::Failed(error) if error.code == brain_codes::BRAIN_EXITED => {
            CallError::Failed(failure(
                brain_codes::BRAIN_EXITED,
                &format!("the brain ended running it: {why}"),
            ))
        }
        _ => CallError::TimedOut,
    };
    job.answer(settle(seen, never, unanswered));
    Err(ended)
}

/// What an ended brain had written about request `id`.
#[derive(Debug)]
enum Drained {
    /// It had accepted it (or answered it, which says the same); then its reply, if any.
    Accepted(Option<Result<Data, CallError>>),
    /// Nothing about it.
    Nothing,
}

/// Reads every frame an ended brain wrote, until its stdout's end, and what they say of
/// request `id` (`accepted` already, when rlxd saw that).
async fn drain(brain: &mut Process, id: u64, accepted: bool) -> Drained {
    let mut frames = Vec::new();
    while let Ok(Some(frame)) = timeout(STOP_GRACE, brain.frames.recv()).await {
        frames.push(frame);
    }
    seen_in(frames, id, accepted)
}

/// What `frames` say of request `id`: whether it was accepted (`accepted` already, or an
/// `accepted` or a reply among them), and its reply.
fn seen_in(frames: Vec<process::Frame>, id: u64, mut accepted: bool) -> Drained {
    let mut reply = None;
    for frame in frames {
        match frame {
            Ok(BrainOutput::Accepted { id: said }) if said == id => accepted = true,
            Ok(BrainOutput::Reply {
                id: said,
                ok,
                result,
                error,
            }) if said == id => {
                accepted = true;
                reply = Some(reply_of(ok, result, error));
            }
            _ => {}
        }
    }
    if accepted {
        Drained::Accepted(reply)
    } else {
        Drained::Nothing
    }
}

/// What a request comes to once its brain ended before rlxd heard all of it: its reply if
/// the brain wrote one, `unanswered` if it accepted the request and wrote no reply, and
/// `never` if it never accepted it.
fn settle(seen: Drained, never: CallError, unanswered: CallError) -> Result<Data, CallError> {
    match seen {
        Drained::Accepted(Some(reply)) => reply,
        Drained::Accepted(None) => Err(unanswered),
        Drained::Nothing => Err(never),
    }
}

/// Answers an accepted job whose brain ended: its reply if it wrote one, else a failure.
fn answer_after_end(shared: &Shared, job: Job, seen: Drained, said: &str) {
    if matches!(seen, Drained::Accepted(Some(_))) {
        shared.state().handled += 1;
    }
    let failed = CallError::Failed(failure(
        brain_codes::BRAIN_EXITED,
        &format!("the brain ended running it ({said})"),
    ));
    let never = CallError::Failed(failure(
        brain_codes::BRAIN_EXITED,
        &format!("the brain ended running it ({said})"),
    ));
    job.answer(settle(seen, never, failed));
}

fn reply_of(ok: bool, result: Option<Data>, error: Option<ErrorBody>) -> Result<Data, CallError> {
    if ok {
        Ok(result.unwrap_or_default())
    } else {
        Err(CallError::Failed(error.unwrap_or_else(|| {
            failure(brain_codes::BRAIN_FAILED, "the brain failed with no error")
        })))
    }
}

/// Writes `frame` as one line on the brain's stdin.
async fn write(brain: &mut Process, frame: &BrainInput) -> std::io::Result<()> {
    let Some(stdin) = brain.stdin.as_mut() else {
        return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
    };
    let mut line = serde_json::to_vec(frame).map_err(std::io::Error::other)?;
    line.push(b'\n');
    stdin.write_all(&line).await?;
    stdin.flush().await
}

/// Ends a brain now: its tree killed, and its end awaited.
async fn end(brain: &mut Process) {
    brain.stdin = None;
    brain.kill();
    let _ = timeout(STOP_GRACE, brain.exit()).await;
}

/// Stops a brain as rlxd stops: its stdin closed, which ends it, then its tree signalled,
/// then killed, each after [`STOP_GRACE`].
async fn stop(brain: &mut Process) {
    brain.stdin = None;
    if timeout(STOP_GRACE, brain.exit()).await.is_ok() {
        // Its own end: what it left in its tree goes too.
        brain.kill();
        info!("brain: stopped");
        return;
    }
    brain.terminate();
    if timeout(STOP_GRACE, brain.exit()).await.is_err() {
        warning!(
            "brain: still running {} s after a SIGTERM: killed",
            STOP_GRACE.as_secs()
        );
    }
    end(brain).await;
    info!("brain: stopped");
}

/// How a brain whose stdout ended has ended, waiting [`STOP_GRACE`] at most: its stdout
/// can end before it does.
async fn end_of(exit: &mut process::Exit) -> String {
    timeout(STOP_GRACE, exit.wait())
        .await
        .unwrap_or_else(|_| "its stdout closed".to_owned())
}

/// Resolves once rlxd stops.
async fn stopped(stopping: &mut watch::Receiver<bool>) {
    let _ = stopping.wait_for(|stop| *stop).await;
}

fn exited(message: &str) -> Ended {
    Ended::Failed(failure(brain_codes::BRAIN_EXITED, message))
}

fn unresponsive(message: &str) -> Ended {
    Ended::Failed(failure(brain_codes::BRAIN_UNRESPONSIVE, message))
}

fn incompatible(message: &str) -> Ended {
    let error = failure(brain_codes::BRAIN_INCOMPATIBLE, message);
    let detail: Data = json!({"rlxd": BRAIN_PROTOCOLS})
        .as_object()
        .cloned()
        .unwrap_or_default();
    match error.clone().with_detail(detail) {
        Ok(with_detail) => Ended::Failed(with_detail),
        Err(_) => Ended::Failed(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: serde_json::Value) -> process::Frame {
        Ok(serde_json::from_value(value).unwrap())
    }

    fn never() -> CallError {
        not_delivered("never")
    }

    /// The fallback line, decided from what a killed brain had written (review M1): an
    /// `accepted` among its frames, even one rlxd read too late, means the request may
    /// have run.
    #[test]
    fn a_request_the_brain_wrote_accepted_for_is_never_refused() {
        let accepted = frame(json!({"type": "accepted", "id": 7}));
        let replied = frame(json!({"type": "reply", "id": 7, "ok": true, "result": {"a": 1}}));
        let outcome = settle(
            seen_in(vec![accepted], 7, false),
            never(),
            CallError::TimedOut,
        );
        assert!(matches!(outcome, Err(CallError::TimedOut)), "{outcome:?}");
        let both = vec![frame(json!({"type": "accepted", "id": 7})), replied];
        let outcome = settle(seen_in(both, 7, false), never(), CallError::TimedOut);
        assert_eq!(outcome.unwrap()["a"], 1);
        // A reply with no `accepted` before it still says the request ran.
        let alone = vec![frame(json!({"type": "reply", "id": 7, "ok": false,
            "error": {"code": "c", "message": "m"}}))];
        let outcome = settle(seen_in(alone, 7, false), never(), CallError::TimedOut);
        assert!(matches!(outcome, Err(CallError::Failed(_))), "{outcome:?}");
    }

    #[test]
    fn a_request_the_brain_never_wrote_accepted_for_was_never_delivered() {
        for frames in [
            vec![],
            vec![frame(json!({"type": "accepted", "id": 6}))],
            vec![
                Err(process::Unread {
                    said: "noise".to_owned(),
                    json: false,
                }),
                frame(json!({"type": "reply", "id": 8, "ok": true, "result": {}})),
            ],
        ] {
            let outcome = settle(seen_in(frames, 7, false), never(), CallError::TimedOut);
            assert!(
                matches!(outcome, Err(CallError::NotDelivered(_))),
                "{outcome:?}"
            );
        }
        // rlxd had seen it accepted already: an end with nothing more is no fallback.
        let outcome = settle(seen_in(vec![], 7, true), never(), CallError::TimedOut);
        assert!(matches!(outcome, Err(CallError::TimedOut)), "{outcome:?}");
    }

    #[test]
    fn the_backoff_doubles_from_a_quarter_second_to_its_cap() {
        let waits: Vec<u128> = (1..=10).map(|n| backoff(n).as_millis()).collect();
        assert_eq!(
            waits,
            [250, 500, 1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000]
        );
        assert_eq!(backoff(u32::MAX), BACKOFF_CAP);
    }
}
