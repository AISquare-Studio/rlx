//! The headless runner (handoff 0.10.3, 2.1, 5.4 and 7.5): Claude Code runs with no tmux
//! and no PTY, streamed as run events, each in a space (2.5), each with a durable log.
//!
//! - **`run.start`** checks the params ([`Runner::start`]), records the run, and either
//!   starts it or, over its kind's pool, queues it (`run.queued {position}`). Pools are
//!   per kind (`RunKind::pool`), so a kind never waits behind another.
//! - **An attempt** resolves Claude Code to an absolute path, makes the space, writes the
//!   attempt's own settings (the gate's hook, 5.1), starts Claude Code in a process tree
//!   of its own ([`process`]) with the exact command line of [`argv`], sends the prompt as
//!   a stream-json user message, and keeps stdin open for the whole run. Each stream-json
//!   line becomes run events ([`stream`]). At the `result`, stdin is closed, and the
//!   tree is killed if it has not exited [`EXIT_GRACE`] later.
//! - **The end.** `run.done`, or `run.failed {reason}`: `error` when Claude Code fails,
//!   crashes, or can't be found or started, `timeout` when `timeout_s` runs out,
//!   `cancelled` on `run.cancel`, and `engine_restarted` when rlxd stops with the run in
//!   flight. Each failure keeps the assistant text the run got out (`partial_text`).
//! - **Events** go through one funnel ([`Runner::publish`]), under the run's own lock and
//!   no other: the run numbers its own events. An event that changes the run's state
//!   (`run.started`, each end) is made durable before it is published: the log so far is
//!   fsynced and the run's record rewritten with the event. Any other event is published,
//!   then appended, and fsynced with the run's next state event or every
//!   [`UNSYNCED_MAX`]. So an app never sees an end that a restart would not find, the log
//!   holds every event in `run_seq` order, and a slow disk under one run holds up no
//!   other run and no other topic: the bus's lock is held only in memory, and the
//!   runner's never across a write.
//! - **Claude Code's environment** is rlxd's own ([`run_env`] is where the app's `PATH`
//!   and config directories plug in, 4.3). `RLXD_CLAUDE_BIN` is read from rlxd's own
//!   environment only, when each attempt starts; no client names the program.
//!
//! Locks: the runner's (its runs and pools), then a run's, never the other way round.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, UNIX_EPOCH};

use rlxd_client::Home;
use rlxd_proto::{
    Data, ErrorBody, FailReason, Mount, RunCancelData, RunDoneData, RunFailedData, RunId, RunKind,
    RunQueuedData, RunRateLimitedData, RunRef, RunStartParams, RunStartedData, RunState,
    RunTextData, RunToolResultData, codes, run_types,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin};
use tokio::sync::watch;

use crate::bus::{self, Bus};
use crate::log::{info, warning};
use crate::{claude, clock, spaces};

mod argv;
#[cfg(unix)]
pub(crate) mod guard;
mod process;
mod store;
mod stream;

use process::{Launch, Spawner, Tree};
use store::{Record, RunDir};
use stream::Line;

/// How long Claude Code has to exit after its `result`, once its stdin is closed, before
/// its tree is killed.
const EXIT_GRACE: Duration = Duration::from_secs(10);

/// How much of Claude Code's stderr a failure quotes: its last 4 KiB.
const STDERR_TAIL: usize = 4 << 10;

/// The most of a run's assistant text kept for `run.failed`'s `partial_text`: its last
/// 1 MiB.
const PARTIAL_TEXT: usize = 1 << 20;

/// How long `run.cancel` waits for a running run to end before it answers.
const CANCEL_WAIT: Duration = Duration::from_secs(10);

/// The runner of one rlxd.
pub struct Runner {
    home: Home,
    bus: Arc<Bus>,
    spawner: Spawner,
    /// `rlxd-hook` beside this rlxd, which each run's settings name, or why it can't be
    /// named: then `run.start` refuses `hook_not_found`.
    hook: Result<PathBuf, String>,
    /// Test builds only: how long a place given back waits before its next run starts,
    /// to widen the window a cancel can land in (`RLXD_TEST_LAUNCH_DELAY_MS`).
    launch_delay: Option<Duration>,
    /// Test builds only: a space whose runs' state events wait this long before they are
    /// made durable, to show a slow disk under one run holds up nothing else
    /// (`RLXD_TEST_SLOW_PERSIST=<space>:<ms>`).
    slow_persist: Option<(String, Duration)>,
    runs: Mutex<Runs>,
    /// Set once rlxd stops: no run starts from then on.
    stopping: AtomicBool,
}

#[derive(Default)]
struct Runs {
    by_id: HashMap<RunId, Arc<Run>>,
    pools: HashMap<RunKind, Pool>,
    /// Run ids taken by a `run.start` whose record is being written, outside this lock.
    reserved: std::collections::HashSet<RunId>,
}

#[derive(Default)]
struct Pool {
    /// Runs holding a place.
    active: usize,
    /// Runs waiting for one, the next first.
    waiting: VecDeque<Arc<Run>>,
}

/// One run rlxd knows.
struct Run {
    params: RunStartParams,
    /// The scheduler job that fired the run (7.6), which only rlxd sets.
    job_id: Option<String>,
    client: String,
    dir: RunDir,
    inner: Mutex<RunInner>,
    /// Changes with the run's state: what `run.cancel` waits on.
    state: watch::Sender<RunState>,
}

struct RunInner {
    state: RunState,
    attempt: u64,
    started_at: String,
    ended_at: Option<String>,
    last_run_seq: u64,
    session: Option<String>,
    structured_output: Option<Value>,
    process: Option<store::Process>,
    /// The running attempt's process tree, to kill.
    tree: Option<Arc<Tree>>,
    /// Why rlxd is ending the running attempt: a cancel, the timeout, or rlxd stopping.
    ending: Option<FailReason>,
    /// Whether the run holds a place in its pool.
    placed: bool,
    /// Whether the bus numbers this run on from `last_run_seq`.
    seeded: bool,
    /// The event log, open to append.
    events: Option<std::fs::File>,
    /// This attempt's assistant text: its last [`PARTIAL_TEXT`] bytes.
    text: String,
    /// Events appended to the log since its last fsync.
    unsynced: u32,
}

/// How durable an event is before it is published.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Durability {
    /// Appended to the log once published, and fsynced later ([`UNSYNCED_MAX`]).
    Logged,
    /// Made durable, the log so far fsynced and the run's record rewritten with it,
    /// before it is published: an event that changes the run's state, so an app never
    /// sees an end that a restart would not find.
    State,
}

/// The most events of a run appended to its log without an fsync. After an OS crash, at
/// most this many of a run's last events are lost, and the next rlxd numbers past them
/// (5.4).
pub(crate) const UNSYNCED_MAX: u32 = 64;

/// How an attempt ended.
enum End {
    Done(RunDoneData),
    Failed(RunFailedData),
}

/// What reading an attempt's stream came to.
enum Streamed {
    /// Its `result`, and its stdin, still open.
    Result(stream::Outcome, ChildStdin),
    /// rlxd ended it, for this reason.
    Ended(FailReason),
    /// It could not go on: why.
    Broken(String),
    /// Claude Code's output ended before a `result`.
    Eof,
}

impl Runner {
    /// The runner of `home`, publishing on `bus`. It must be made inside rlxd's runtime.
    pub fn new(home: Home, bus: Arc<Bus>) -> Arc<Self> {
        let rlxd = std::env::current_exe().ok();
        Arc::new(Self {
            home,
            bus,
            spawner: Spawner::start(),
            hook: argv::hook_beside(rlxd.as_deref()),
            launch_delay: test_delay("RLXD_TEST_LAUNCH_DELAY_MS"),
            slow_persist: test_slow_persist(),
            runs: Mutex::new(Runs::default()),
            stopping: AtomicBool::new(false),
        })
    }

    fn runs(&self) -> MutexGuard<'_, Runs> {
        self.runs.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Every run rlxd knows now, to read with the runner's lock let go.
    fn snapshot(&self) -> Vec<Arc<Run>> {
        self.runs().by_id.values().cloned().collect()
    }

    /// `status`'s `runs`: how many run, and how many wait.
    pub fn counts(&self) -> (u64, u64) {
        // Each run's state is read from its watch, with no lock of the run's or the
        // runner's held: a run's own lock may be held over a slow disk, and status never
        // waits on it.
        let runs = self.snapshot();
        let states: Vec<RunState> = runs.iter().map(|r| *r.state.borrow()).collect();
        let count = |state| {
            let n = states.iter().filter(|s| **s == state).count();
            u64::try_from(n).unwrap_or(u64::MAX)
        };
        (count(RunState::Running), count(RunState::Queued))
    }

    /// `run.start`: the run is recorded, and started or queued. Refused with
    /// `kind_mount_mismatch`, `pinned_call_required` (only that it is there: the gate
    /// enforces it, 5.1), `not_implemented` for a non-empty `needs_connectors` (until
    /// 5.5), `run_exists`, or `run_start_failed` when rlxd is stopping or the run's record
    /// can't be written.
    pub fn start(
        self: &Arc<Self>,
        params: RunStartParams,
        client: &str,
        job_id: Option<String>,
    ) -> Result<RunRef, ErrorBody> {
        if params.kind.needs_no_tools() && params.mount != Mount::NoTools {
            return Err(refusal(
                codes::KIND_MOUNT_MISMATCH,
                format!(
                    "a {} run takes only mount {{\"kind\": \"no_tools\"}}",
                    params.kind
                ),
            ));
        }
        if params.kind.needs_pinned_call() && params.pinned_call.is_none() {
            return Err(refusal(
                codes::PINNED_CALL_REQUIRED,
                format!(
                    "a {} run needs pinned_call {{tool, input_sha256}}",
                    params.kind
                ),
            ));
        }
        if !params.needs_connectors.is_empty() {
            return Err(refusal(
                codes::NOT_IMPLEMENTED,
                "needs_connectors is not served yet: the connector gate (handoff 0.10.3, \
                 5.5) is a later item. Leave it empty."
                    .to_owned(),
            ));
        }
        if self.stopping.load(Ordering::SeqCst) {
            return Err(refusal(
                codes::RUN_START_FAILED,
                "rlxd is stopping: start the run once it is back".to_owned(),
            ));
        }
        if let Err(why) = &self.hook {
            return Err(refusal(
                codes::HOOK_NOT_FOUND,
                format!(
                    "{why}; a run's gate hook is always rlxd-hook by its absolute path, \
                     never a name looked up on PATH"
                ),
            ));
        }
        let id = params.run_id.clone();
        let dir = RunDir::of(&self.home, &id);
        {
            // The id is taken here; the record is written outside the runner's lock.
            let mut runs = self.runs();
            if runs.by_id.contains_key(&id) || runs.reserved.contains(&id) || dir.exists() {
                return Err(refusal(
                    codes::RUN_EXISTS,
                    format!("rlxd already knows a run {id}: pick a new run_id"),
                ));
            }
            runs.reserved.insert(id.clone());
        }
        let run = Arc::new(Run {
            job_id,
            client: client.to_owned(),
            dir,
            inner: Mutex::new(RunInner {
                state: RunState::Queued,
                attempt: 1,
                started_at: clock::now(),
                ended_at: None,
                last_run_seq: 0,
                session: None,
                structured_output: None,
                process: None,
                tree: None,
                ending: None,
                placed: false,
                seeded: false,
                events: None,
                text: String::new(),
                unsynced: 0,
            }),
            state: watch::Sender::new(RunState::Queued),
            params,
        });
        let created = blocking(|| {
            run.dir.create()?;
            let mut inner = run.inner();
            inner.events = Some(run.dir.open_events()?);
            run.dir.write_record(&run.record(&inner))
        });
        if let Err(e) = created {
            let _ = std::fs::remove_dir_all(run.dir.path());
            self.runs().reserved.remove(&id);
            return Err(refusal(
                codes::RUN_START_FAILED,
                format!("the run's record could not be written: {e}"),
            ));
        }
        let mut runs = self.runs();
        runs.reserved.remove(&id);
        runs.by_id.insert(id.clone(), Arc::clone(&run));
        let kind = run.params.kind;
        let pool = runs.pools.entry(kind).or_default();
        let queued = if pool.active < kind.pool() {
            pool.active += 1;
            None
        } else {
            pool.waiting.push_back(Arc::clone(&run));
            Some(u32::try_from(pool.waiting.len()).unwrap_or(u32::MAX))
        };
        drop(runs);
        info!("run {id}: a {kind} run, from {client}");
        match queued {
            None => self.launch(&run),
            Some(position) => {
                let mut inner = run.inner();
                self.emit(
                    &run,
                    &mut inner,
                    run_types::QUEUED,
                    &RunQueuedData::new(position),
                );
            }
        }
        Ok(RunRef::new(id))
    }

    /// `run.cancel`: a queued run leaves its queue, and a running one has its process tree
    /// killed; both end `failed {reason: "cancelled"}` with their partial text, and so
    /// does one that waits for the user. A run that has ended is left as it was.
    pub async fn cancel(self: &Arc<Self>, id: &RunId) -> Result<RunCancelData, ErrorBody> {
        let run = self.find(id)?;
        let mut changes = run.state.subscribe();
        {
            // Out of its queue first, under the runner's lock, so no place goes to it.
            let mut runs = self.runs();
            let queued = run.inner().state == RunState::Queued;
            if queued && let Some(pool) = runs.pools.get_mut(&run.params.kind) {
                pool.waiting.retain(|waiting| !Arc::ptr_eq(waiting, &run));
            }
        }
        let running = {
            let mut inner = run.inner();
            match inner.state {
                state @ (RunState::Queued | RunState::NeedsYou) => {
                    let message = if state == RunState::Queued {
                        "the run was cancelled while it waited for its turn"
                    } else {
                        "the run was cancelled while it waited for the user"
                    };
                    let failed =
                        RunFailedData::new(FailReason::Cancelled, message, inner.text.clone());
                    let placed = self.end(&run, &mut inner, End::Failed(failed));
                    drop(inner);
                    self.release_if(placed, run.params.kind);
                    false
                }
                RunState::Running => {
                    inner.ending.get_or_insert(FailReason::Cancelled);
                    // A tree not started yet sees `ending` before it starts.
                    if let Some(tree) = &inner.tree {
                        tree.kill();
                    }
                    true
                }
                state @ (RunState::Done | RunState::Failed) => {
                    return Ok(RunCancelData::new(id.clone(), false, state));
                }
            }
        };
        if running {
            let _ = tokio::time::timeout(CANCEL_WAIT, changes.wait_for(|s| !s.is_live())).await;
        }
        let now = run.inner().state;
        Ok(RunCancelData::new(id.clone(), now == RunState::Failed, now))
    }

    /// Ends every run in flight as rlxd stops: no run starts from then on, each process
    /// tree is killed, and each run, queued or running, ends `failed {reason:
    /// "engine_restarted"}`, which `run.resume` goes on from.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        for run in self.snapshot() {
            let mut inner = run.inner();
            if !inner.state.is_live() {
                continue;
            }
            inner.ending.get_or_insert(FailReason::EngineRestarted);
            if let Some(tree) = &inner.tree {
                tree.kill();
            }
            let failed = RunFailedData::new(
                FailReason::EngineRestarted,
                "rlxd stopped while the run was in flight; run.resume goes on with it",
                inner.text.clone(),
            );
            // No place is given back: nothing starts while rlxd stops.
            self.end(&run, &mut inner, End::Failed(failed));
        }
    }

    /// The run `id`, or `unknown_run`.
    fn find(&self, id: &RunId) -> Result<Arc<Run>, ErrorBody> {
        self.runs()
            .by_id
            .get(id)
            .cloned()
            .ok_or_else(|| refusal(codes::UNKNOWN_RUN, format!("rlxd knows no run {id}")))
    }

    /// Starts `run`'s attempt, which holds a place in its pool.
    ///
    /// The run may have been cancelled after it left its queue and before this: then it
    /// is not started, and the place goes to the next run instead. So no Claude Code ever
    /// starts for a run that has ended.
    fn launch(self: &Arc<Self>, run: &Arc<Run>) {
        {
            let mut inner = run.inner();
            if inner.state != RunState::Queued || inner.ending.is_some() {
                drop(inner);
                info!(
                    "run {}: ended before its turn came; its place goes on",
                    run.params.run_id
                );
                self.release_if(true, run.params.kind);
                return;
            }
            inner.placed = true;
            inner.state = RunState::Running;
            inner.text.clear();
            run.state.send_replace(RunState::Running);
            let record = run.record(&inner);
            if let Err(e) = blocking(|| run.dir.write_record(&record)) {
                warning!("run {}: its record was not written: {e}", run.params.run_id);
            }
        }
        let runner = Arc::clone(self);
        let run = Arc::clone(run);
        tokio::spawn(async move {
            let end = runner.attempt(&run).await;
            let mut inner = run.inner();
            let placed = runner.end(&run, &mut inner, end);
            drop(inner);
            runner.release_if(placed, run.params.kind);
        });
    }

    /// Ends `run` as `end` says, unless it has ended already: its last event and its
    /// record. Whether it held a place in its pool, which the caller gives back
    /// ([`Runner::release_if`]) once it has let the run's lock go.
    fn end(&self, run: &Run, inner: &mut RunInner, end: End) -> bool {
        if !(inner.state.is_live() || inner.state == RunState::NeedsYou) {
            return false;
        }
        let (state, r#type, data) = match end {
            End::Done(done) => {
                inner.session.clone_from(&done.session);
                inner.structured_output.clone_from(&done.structured_output);
                (RunState::Done, run_types::DONE, serde_json::to_value(&done))
            }
            End::Failed(failed) => (
                RunState::Failed,
                run_types::FAILED,
                serde_json::to_value(&failed),
            ),
        };
        inner.state = state;
        inner.ended_at = Some(clock::now());
        inner.tree = None;
        inner.process = None;
        let published = match data {
            Ok(Value::Object(data)) => self.publish(run, inner, r#type, data, Durability::State),
            other => Err(format!("it did not print as an object: {other:?}")),
        };
        if let Err(why) = published {
            // A run always gets its end: a small one, when its own can't be published.
            warning!(
                "run {}: {type} was not published ({why}); it ends failed",
                run.params.run_id
            );
            inner.state = RunState::Failed;
            let failed = RunFailedData::new(
                FailReason::Error,
                format!("end_not_published: the run's {type} could not be published: {why}"),
                "",
            );
            if let Ok(Value::Object(data)) = serde_json::to_value(&failed)
                && let Err(why) =
                    self.publish(run, inner, run_types::FAILED, data, Durability::State)
            {
                warning!(
                    "run {}: its end was not published either: {why}",
                    run.params.run_id
                );
            }
        }
        let state = inner.state;
        self.bus.end_run(run.params.run_id.as_str());
        inner.seeded = false;
        run.state.send_replace(state);
        info!("run {}: {state}", run.params.run_id);
        std::mem::take(&mut inner.placed)
    }

    /// When `placed`, gives a place in `kind`'s pool back, to the next run waiting if
    /// there is one. Called with no run's lock held.
    fn release_if(self: &Arc<Self>, placed: bool, kind: RunKind) {
        if !placed {
            return;
        }
        let next = {
            let mut runs = self.runs();
            let pool = runs.pools.entry(kind).or_default();
            pool.active = pool.active.saturating_sub(1);
            if self.stopping.load(Ordering::SeqCst) {
                None
            } else {
                let next = pool.waiting.pop_front();
                if next.is_some() {
                    pool.active += 1;
                }
                next
            }
        };
        if let Some(next) = next {
            match self.launch_delay {
                Some(delay) => {
                    let runner = Arc::clone(self);
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        runner.launch(&next);
                    });
                }
                None => self.launch(&next),
            }
        }
    }

    /// One attempt of `run`, from finding Claude Code to its end.
    async fn attempt(self: &Arc<Self>, run: &Arc<Run>) -> End {
        let params = &run.params;
        let attempt = run.inner().attempt;
        let failed = |message: String| {
            End::Failed(RunFailedData::new(
                FailReason::Error,
                message,
                run.inner().text.clone(),
            ))
        };
        let Some(program) = claude::resolve().and_then(|p| std::path::absolute(p).ok()) else {
            return failed(format!(
                "Claude Code was not found: there is no `claude` on rlxd's PATH, and {} \
                 names none",
                claude::BIN_ENV
            ));
        };
        let prepared = blocking(|| -> std::io::Result<(PathBuf, PathBuf)> {
            let space = spaces::ensure(&self.home, &params.space)?;
            let hook = self
                .hook
                .as_ref()
                .map_err(|why| std::io::Error::other(why.clone()))?;
            let settings = argv::settings(params, attempt, hook);
            let settings = run.dir.write_settings(attempt, &settings)?;
            Ok((space, settings))
        });
        let (space, settings) = match prepared {
            Ok(prepared) => prepared,
            Err(e) => return failed(format!("the run could not be set up: {e}")),
        };
        let ending = run.inner().ending;
        if let Some(reason) = ending {
            return ended_early(run, reason);
        }
        let launch = Launch {
            program: program.clone(),
            args: argv::args(params, attempt, &settings),
            cwd: space,
            env: run_env(),
        };
        let (mut child, tree) = match self.spawner.spawn(launch).await {
            Ok(spawned) => spawned,
            Err(e) => {
                return failed(format!(
                    "Claude Code ({}) could not start: {e}",
                    program.display()
                ));
            }
        };
        let tree = Arc::new(tree);
        {
            let mut inner = run.inner();
            inner.process = Some(store::Process {
                pid: tree.pid,
                started: tree.started.clone(),
            });
            inner.tree = Some(Arc::clone(&tree));
            let record = run.record(&inner);
            if let Err(e) = blocking(|| run.dir.write_record(&record)) {
                warning!("run {}: its record was not written: {e}", params.run_id);
            }
            if inner.ending.is_some() {
                tree.kill();
            }
        }
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            tree.kill();
            return failed("Claude Code's stdio was not piped".to_owned());
        };
        let stderr = tokio::spawn(tail(stderr));
        let streamed = self
            .stream(run, attempt, stdin, BufReader::new(stdout), &tree)
            .await;
        let end = match streamed {
            Streamed::Result(outcome, mut stdin) => {
                // Claude Code reads stdin for another turn: closing it ends it. Whatever
                // is left of the tree goes with it (the guard), or is killed after the
                // grace.
                let _ = stdin.shutdown().await;
                drop(stdin);
                if tree.reap(&mut child, Some(EXIT_GRACE)).await.is_none() {
                    tree.kill();
                    tree.reap(&mut child, None).await;
                }
                outcome_of(run, outcome)
            }
            Streamed::Ended(reason) => {
                tree.kill();
                tree.reap(&mut child, None).await;
                ended_early(run, reason)
            }
            Streamed::Broken(message) => {
                tree.kill();
                tree.reap(&mut child, None).await;
                failed(message)
            }
            Streamed::Eof => {
                let mut status = tree.reap(&mut child, Some(EXIT_GRACE)).await;
                if status.is_none() {
                    tree.kill();
                    status = tree.reap(&mut child, None).await;
                }
                let ending = run.inner().ending;
                if let Some(reason) = ending {
                    return ended_early(run, reason);
                }
                let said = stderr.await.unwrap_or_default();
                let how = match status {
                    Some(status) => format!("exited ({status})"),
                    None => "closed its output".to_owned(),
                };
                let mut message = format!("Claude Code {how} before its result");
                if !said.trim().is_empty() {
                    message.push_str(": ");
                    message.push_str(said.trim());
                }
                failed(message)
            }
        };
        let ending = run.inner().ending;
        match ending {
            // rlxd ended the run as its result came: rlxd's reason stands.
            Some(reason) if !matches!(&end, End::Failed(f) if f.reason == reason) => {
                ended_early(run, reason)
            }
            _ => end,
        }
    }

    /// Sends the prompt, then reads the stream until its `result`, its end, or the
    /// timeout. A cancel kills the tree, which ends the stream.
    async fn stream<R: AsyncBufRead + Unpin>(
        &self,
        run: &Run,
        attempt: u64,
        mut stdin: ChildStdin,
        mut stdout: R,
        tree: &Tree,
    ) -> Streamed {
        let params = &run.params;
        let prompt = json!({"type": "user", "message": {"role": "user",
            "content": [{"type": "text", "text": params.prompt}]},
            "parent_tool_use_id": null, "session_id": params.run_id.as_str()});
        let mut first = prompt.to_string();
        first.push('\n');
        let sent = async {
            stdin.write_all(first.as_bytes()).await?;
            stdin.flush().await
        };
        if let Err(e) = sent.await {
            return Streamed::Broken(format!("the prompt could not be sent to Claude Code: {e}"));
        }
        let deadline = params
            .timeout_s
            .map(|s| Instant::now() + Duration::from_secs(s));
        let mut line = Vec::new();
        let mut tools: HashMap<String, String> = HashMap::new();
        let mut started = false;
        loop {
            let timeout = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending().await,
                }
            };
            let read = tokio::select! {
                read = stream::next_line(&mut stdout, &mut line, stream::MAX_LINE) => read,
                () = timeout => {
                    let reason = *run.inner().ending.get_or_insert(FailReason::Timeout);
                    tree.kill();
                    return Streamed::Ended(reason);
                }
            };
            let ending = run.inner().ending;
            if let Some(reason) = ending {
                return Streamed::Ended(reason);
            }
            match read {
                Ok(Some(())) => {}
                Ok(None) => return Streamed::Eof,
                Err(stream::ReadError::TooLong) => {
                    return Streamed::Broken(format!(
                        "line_too_long: Claude Code wrote a stream-json line longer than {} \
                         bytes, which rlxd does not read",
                        stream::MAX_LINE
                    ));
                }
                Err(stream::ReadError::Io(e)) => {
                    return Streamed::Broken(format!(
                        "Claude Code's output could not be read: {e}"
                    ));
                }
            }
            for parsed in stream::parse(&line) {
                let mut inner = run.inner();
                match parsed {
                    Line::Init(init) if !started => {
                        started = true;
                        let data = RunStartedData::new(init.cwd, init.model, init.claude_version)
                            .with_tools(init.tools, init.mcp_servers)
                            .resumed(attempt > 1);
                        self.emit(run, &mut inner, run_types::STARTED, &data);
                    }
                    Line::Text(text) => {
                        keep_text(&mut inner.text, &text);
                        for piece in stream::pieces(&text) {
                            let data = RunTextData::new(piece, false);
                            self.emit(run, &mut inner, run_types::TEXT, &data);
                        }
                    }
                    Line::Partial(text) if params.partial => {
                        for piece in stream::pieces(&text) {
                            let data = RunTextData::new(piece, true);
                            self.emit(run, &mut inner, run_types::TEXT, &data);
                        }
                    }
                    Line::ToolUse { id, name } => {
                        if tools.len() < 4096 {
                            tools.insert(id, name);
                        }
                    }
                    Line::ToolResult {
                        call_id,
                        is_error,
                        content,
                    } => {
                        let (content, truncated) = stream::bounded(content);
                        let tool = tools.get(&call_id).cloned().unwrap_or_default();
                        let data = RunToolResultData::new(call_id, tool, is_error, content)
                            .truncated(truncated);
                        self.emit(run, &mut inner, run_types::TOOL_RESULT, &data);
                    }
                    Line::RateLimit { resets_at, status } => {
                        let resets_at =
                            resets_at.map(|s| clock::format(UNIX_EPOCH + Duration::from_secs(s)));
                        let data = RunRateLimitedData::new(resets_at, status);
                        self.emit(run, &mut inner, run_types::RATE_LIMITED, &data);
                    }
                    Line::Result(outcome) => return Streamed::Result(outcome, stdin),
                    Line::Init(_) | Line::Partial(_) | Line::Other => {}
                }
            }
        }
    }

    /// Publishes a run event: the funnel every run event but an end goes through.
    /// `run.started` changes the run's state, so it is made durable first ([`Runner::
    /// publish`]); any other event is logged once it is published.
    fn emit(&self, run: &Run, inner: &mut RunInner, r#type: &str, data: &impl Serialize) {
        match serde_json::to_value(data) {
            Ok(Value::Object(data)) => {
                let durability = if r#type == run_types::STARTED {
                    Durability::State
                } else {
                    Durability::Logged
                };
                // A failure is logged where it happens; the run goes on.
                let _ = self.publish(run, inner, r#type, data, durability);
            }
            other => warning!(
                "run {}: {type} did not print as an object: {other:?}",
                run.params.run_id
            ),
        }
    }

    /// Publishes `data` as `run`'s next event, numbered by the run itself, under the
    /// run's own lock (`inner`) and no other: the bus's lock is held only to append it in
    /// memory, and the runner's not at all, so a slow disk under one run holds up no
    /// other run and no other topic.
    ///
    /// - A [`Durability::State`] event (`run.started`, `run.needs_you`, each end) is made
    ///   durable before anyone sees it: the log so far is fsynced, then the run's record
    ///   is rewritten with the new state and the event itself (`last_event`), then it is
    ///   published, then its line is appended to the log. A crash between the record and
    ///   the line leaves the event in the record, from which the next rlxd puts it back.
    /// - Any other event is published, then appended, and fsynced with the run's next
    ///   state event or every [`UNSYNCED_MAX`] appends, whichever comes first.
    fn publish(
        &self,
        run: &Run,
        inner: &mut RunInner,
        r#type: &str,
        data: Data,
        durability: Durability,
    ) -> Result<(), String> {
        let id = run.params.run_id.as_str();
        if !inner.seeded {
            self.bus.seed_run(id, inner.last_run_seq);
            inner.seeded = true;
        }
        let run_seq = inner.last_run_seq + 1;
        if durability == Durability::State {
            blocking(|| self.persist_state(run, inner, run_seq, r#type, &data));
        }
        let scope = bus::Run {
            id,
            kind: run.params.kind.as_str(),
            attempt: inner.attempt,
            job_id: run.job_id.as_deref(),
        };
        let (_, line) = self
            .bus
            .publish_run_as(scope, run_seq, r#type, data)
            .map_err(|e| {
                warning!("run {id}: {type} was not published: {e}");
                e.to_string()
            })?;
        inner.last_run_seq = run_seq;
        blocking(|| log_line(run, inner, run_seq, r#type, &line));
        Ok(())
    }

    /// Makes `type`, the run's event `run_seq`, durable before it is published: fsyncs the
    /// log so far, then rewrites the record with the run's new state and the event.
    fn persist_state(
        &self,
        run: &Run,
        inner: &mut RunInner,
        run_seq: u64,
        r#type: &str,
        data: &Data,
    ) {
        let id = run.params.run_id.as_str();
        if let Some((space, delay)) = &self.slow_persist
            && *space == run.params.space
        {
            std::thread::sleep(*delay);
        }
        if let Some(events) = inner.events.as_mut()
            && let Err(e) = events.sync_data()
        {
            warning!("run {id}: its log could not be fsynced: {e}");
        }
        inner.unsynced = 0;
        let mut record = run.record(inner);
        record.last_run_seq = run_seq;
        record.last_event = Some(json!({"run_seq": run_seq, "type": r#type, "data": data}));
        if let Err(e) = run.dir.write_record(&record) {
            warning!("run {id}: its record was not written: {e}");
        }
    }
}

/// Appends the published event `run_seq`'s line to the run's log, fsyncing it after
/// [`UNSYNCED_MAX`] unsynced appends.
fn log_line(run: &Run, inner: &mut RunInner, run_seq: u64, r#type: &str, line: &[u8]) {
    let id = run.params.run_id.as_str();
    if inner.events.is_none() {
        match run.dir.open_events() {
            Ok(events) => inner.events = Some(events),
            Err(e) => warning!("run {id}: its log could not be opened: {e}"),
        }
    }
    let Some(events) = inner.events.as_mut() else {
        return;
    };
    inner.unsynced += 1;
    let sync = inner.unsynced >= UNSYNCED_MAX;
    match store::append(events, line, sync) {
        Ok(()) => {
            if sync {
                inner.unsynced = 0;
            }
        }
        Err(e) => warning!("run {id}: {type} (run_seq {run_seq}) was not logged: {e}"),
    }
}

impl Run {
    fn inner(&self) -> MutexGuard<'_, RunInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn record(&self, inner: &RunInner) -> Record {
        Record {
            version: store::RECORD_VERSION,
            run_id: self.params.run_id.clone(),
            kind: self.params.kind,
            params: self.params.clone(),
            client: self.client.clone(),
            state: inner.state,
            attempt: inner.attempt,
            started_at: inner.started_at.clone(),
            ended_at: inner.ended_at.clone(),
            last_run_seq: inner.last_run_seq,
            session: inner.session.clone(),
            structured_output: inner.structured_output.clone(),
            process: inner.process.clone(),
            job_id: self.job_id.clone(),
            last_event: None,
            boot: process::boot_id(),
            calls: Vec::new(),
        }
    }
}

/// The end of an attempt rlxd ended itself, for `reason`.
fn ended_early(run: &Run, reason: FailReason) -> End {
    let message = match reason {
        FailReason::Cancelled => "the run was cancelled".to_owned(),
        FailReason::Timeout => format!(
            "the run took longer than its timeout_s, {} s",
            run.params.timeout_s.unwrap_or_default()
        ),
        FailReason::EngineRestarted => {
            "rlxd stopped while the run was in flight; run.resume goes on with it".to_owned()
        }
        _ => "rlxd ended the run".to_owned(),
    };
    End::Failed(RunFailedData::new(
        reason,
        message,
        run.inner().text.clone(),
    ))
}

/// The run's end, from its `result`.
fn outcome_of(run: &Run, outcome: stream::Outcome) -> End {
    let text = run.inner().text.clone();
    if !outcome.success {
        let mut message = format!("Claude Code's result was an error ({})", outcome.subtype);
        let said: Vec<&str> = outcome
            .errors
            .iter()
            .map(String::as_str)
            .chain((!outcome.result.is_empty()).then_some(outcome.result.as_str()))
            .collect();
        if !said.is_empty() {
            message.push_str(": ");
            message.push_str(&said.join("; "));
        }
        return End::Failed(RunFailedData::new(FailReason::Error, message, text));
    }
    let structured = outcome
        .structured_output
        .as_ref()
        .map_or(0, |output| output.to_string().len());
    if outcome.result.len() > stream::MAX_CONTENT || structured > stream::MAX_CONTENT {
        let message = format!(
            "output_too_large: the result is {} bytes and its structured output {structured}, \
             and run.done carries at most {} of each",
            outcome.result.len(),
            stream::MAX_CONTENT
        );
        return End::Failed(RunFailedData::new(FailReason::Error, message, text));
    }
    let session = run.params.persists().then(|| run.params.run_id.to_string());
    let mut done = RunDoneData::new(session, outcome.result, outcome.usage)
        .with_turns(outcome.duration_ms, outcome.num_turns);
    done.structured_output = outcome.structured_output;
    done.cost_usd = outcome.cost_usd;
    End::Done(done)
}

/// The environment a run's Claude Code gets beyond rlxd's own, which it inherits. The
/// app's `PATH`, `CLAUDE_CONFIG_DIR` and `CLAUDE_SECURESTORAGE_CONFIG_DIR` (4.3) plug in
/// here; until then there is nothing to add.
fn run_env() -> Vec<(OsString, OsString)> {
    Vec::new()
}

/// Adds `text` to `kept`, which holds at most the last [`PARTIAL_TEXT`] bytes.
fn keep_text(kept: &mut String, text: &str) {
    kept.push_str(text);
    if kept.len() > PARTIAL_TEXT {
        let mut cut = kept.len() - PARTIAL_TEXT;
        while !kept.is_char_boundary(cut) {
            cut += 1;
        }
        kept.drain(..cut);
    }
}

/// The last [`STDERR_TAIL`] bytes `stderr` wrote, as text.
async fn tail(mut stderr: ChildStderr) -> String {
    let mut kept: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 4096];
    while let Ok(n) = stderr.read(&mut buffer).await {
        if n == 0 {
            break;
        }
        kept.extend_from_slice(&buffer[..n]);
        if kept.len() > STDERR_TAIL {
            kept.drain(..kept.len() - STDERR_TAIL);
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

/// Test builds only: the delay `name` sets in rlxd's environment, in milliseconds. A
/// release build reads no such variable.
#[cfg(feature = "debug-methods")]
fn test_delay(name: &str) -> Option<Duration> {
    let ms: u64 = std::env::var(name).ok()?.parse().ok()?;
    Some(Duration::from_millis(ms))
}

#[cfg(not(feature = "debug-methods"))]
fn test_delay(_name: &str) -> Option<Duration> {
    None
}

/// Test builds only: `RLXD_TEST_SLOW_PERSIST=<space>:<ms>`.
#[cfg(feature = "debug-methods")]
fn test_slow_persist() -> Option<(String, Duration)> {
    let value = std::env::var("RLXD_TEST_SLOW_PERSIST").ok()?;
    let (space, ms) = value.split_once(':')?;
    Some((space.to_owned(), Duration::from_millis(ms.parse().ok()?)))
}

#[cfg(not(feature = "debug-methods"))]
fn test_slow_persist() -> Option<(String, Duration)> {
    None
}

/// An error body for a refused runner call.
fn refusal(code: &str, message: String) -> ErrorBody {
    ErrorBody::new(code, message).expect("a runner refusal has a code and a message")
}

/// Runs `work`, which writes and fsyncs files, as blocking work on rlxd's multi-thread
/// runtime, so no other task waits on the disk; directly anywhere else.
fn blocking<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_text_keeps_the_last_mebibyte_on_a_character_boundary() {
        let mut kept = String::new();
        keep_text(&mut kept, &"a".repeat(PARTIAL_TEXT - 1));
        keep_text(&mut kept, "éé");
        assert!(kept.len() <= PARTIAL_TEXT);
        assert!(kept.ends_with("éé"));
    }
}
