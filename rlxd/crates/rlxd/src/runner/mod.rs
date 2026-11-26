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
//! - **Events** go through one funnel ([`Runner::emit`]), under the run's own lock: the
//!   bus numbers each, the line is appended to the run's log on disk ([`store`]), and only
//!   then is it published. Every event but a partial text chunk is fsynced first, and an
//!   event that changes the run's state (`run.started` and each end) also has the run's
//!   record rewritten first. So an app never sees an event, an end above all, that a
//!   restart would not find, and the log holds every event in `run_seq` order.
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
    /// `rlxd-hook` beside this rlxd, which each run's settings name.
    hook: PathBuf,
    runs: Mutex<Runs>,
    /// Set once rlxd stops: no run starts from then on.
    stopping: AtomicBool,
}

#[derive(Default)]
struct Runs {
    by_id: HashMap<RunId, Arc<Run>>,
    pools: HashMap<RunKind, Pool>,
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
}

/// How durable an event is before it is published.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Durability {
    /// Appended to the log, durable with the next event that is fsynced: partial text.
    Appended,
    /// Appended and fsynced before it is published.
    Synced,
    /// Appended and fsynced, and the run's record rewritten, before it is published: an
    /// event that changes the run's state, so an app never sees an end that a restart
    /// would not find.
    State,
}

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
            runs: Mutex::new(Runs::default()),
            stopping: AtomicBool::new(false),
        })
    }

    fn runs(&self) -> MutexGuard<'_, Runs> {
        self.runs.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `status`'s `runs`: how many run, and how many wait.
    pub fn counts(&self) -> (u64, u64) {
        let runs = self.runs();
        let count = |state| {
            let n = runs
                .by_id
                .values()
                .filter(|r| r.inner().state == state)
                .count();
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
        let id = params.run_id.clone();
        let dir = RunDir::of(&self.home, &id);
        let mut runs = self.runs();
        if runs.by_id.contains_key(&id) || dir.exists() {
            return Err(refusal(
                codes::RUN_EXISTS,
                format!("rlxd already knows a run {id}: pick a new run_id"),
            ));
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
            return Err(refusal(
                codes::RUN_START_FAILED,
                format!("the run's record could not be written: {e}"),
            ));
        }
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
        let runs: Vec<Arc<Run>> = self.runs().by_id.values().cloned().collect();
        for run in runs {
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
    fn launch(self: &Arc<Self>, run: &Arc<Run>) {
        {
            let mut inner = run.inner();
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
        match data {
            Ok(Value::Object(data)) => self.publish(run, inner, r#type, data, Durability::State),
            other => warning!("run {}: {type} did not print: {other:?}", run.params.run_id),
        }
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
            self.launch(&next);
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
            let settings = argv::settings(params, attempt, &self.hook);
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
                // Claude Code reads stdin for another turn: closing it ends it.
                let _ = stdin.shutdown().await;
                drop(stdin);
                if tokio::time::timeout(EXIT_GRACE, child.wait())
                    .await
                    .is_err()
                {
                    tree.kill();
                }
                outcome_of(run, outcome)
            }
            Streamed::Ended(reason) => {
                tree.kill();
                let _ = child.wait().await;
                ended_early(run, reason)
            }
            Streamed::Broken(message) => {
                tree.kill();
                let _ = child.wait().await;
                failed(message)
            }
            Streamed::Eof => {
                let status = tokio::time::timeout(EXIT_GRACE, child.wait()).await;
                tree.kill();
                let ending = run.inner().ending;
                if let Some(reason) = ending {
                    return ended_early(run, reason);
                }
                let said = stderr.await.unwrap_or_default();
                let how = match status {
                    Ok(Ok(status)) => format!("exited ({status})"),
                    _ => "closed its output".to_owned(),
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

    /// Logs a run event, then publishes it: the funnel every run event goes through. A
    /// partial text chunk is appended, any other event fsynced too, and `run.started`
    /// also rewrites the run's record, before it is published.
    fn emit(&self, run: &Run, inner: &mut RunInner, r#type: &str, data: &impl Serialize) {
        match serde_json::to_value(data) {
            Ok(Value::Object(data)) => {
                let durability = if r#type == run_types::STARTED {
                    Durability::State
                } else if r#type == run_types::TEXT
                    && data.get("partial") == Some(&Value::Bool(true))
                {
                    Durability::Appended
                } else {
                    Durability::Synced
                };
                self.publish(run, inner, r#type, data, durability);
            }
            other => warning!(
                "run {}: {type} did not print as an object: {other:?}",
                run.params.run_id
            ),
        }
    }

    /// Publishes `data` as `run`'s next event, once its line is in the run's log as
    /// `durability` says: durable before visible.
    fn publish(
        &self,
        run: &Run,
        inner: &mut RunInner,
        r#type: &str,
        data: Data,
        durability: Durability,
    ) {
        let id = run.params.run_id.as_str();
        if !inner.seeded {
            self.bus.seed_run(id, inner.last_run_seq);
            inner.seeded = true;
        }
        let scope = bus::Run {
            id,
            kind: run.params.kind.as_str(),
            attempt: inner.attempt,
            job_id: run.job_id.as_deref(),
        };
        let persist = |numbered: bus::Numbered, line: &bus::Line| {
            inner.last_run_seq = numbered.run_seq;
            let sync = durability != Durability::Appended;
            if let Some(events) = inner.events.as_mut()
                && let Err(e) = store::append(events, line, sync)
            {
                warning!(
                    "run {id}: {type} (run_seq {}) was not logged: {e}",
                    numbered.run_seq
                );
            }
            if durability == Durability::State {
                let record = run.record(inner);
                if let Err(e) = run.dir.write_record(&record) {
                    warning!("run {id}: its record was not written: {e}");
                }
            }
        };
        if let Err(e) = blocking(|| self.bus.publish_run_durably(scope, r#type, data, persist)) {
            warning!("run {id}: {type} was not published: {e}");
        }
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
