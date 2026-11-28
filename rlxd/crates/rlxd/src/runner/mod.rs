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
//! - **The end.** `run.done`; `run.needs_you {reason: "question"}` when the output matches
//!   `needs_you_when`, after `needs_you.raised` on the `needs_you` topic; or `run.failed
//!   {reason}`: `error` when Claude Code fails, crashes, or can't be found or started,
//!   `timeout` when `timeout_s` runs out, `cancelled` on `run.cancel`, and
//!   `engine_restarted` when rlxd stops with the run in flight, or is found to have
//!   stopped so at its next start ([`Runner::new`]). Each failure keeps the assistant text
//!   the run got out (`partial_text`).
//! - **The lifecycle** (5.4): `run.resume` goes on with a run that has no live process as
//!   a new attempt; `run.get` and `run.list` read what rlxd holds, which its records on
//!   disk hold too, so they survive a restart; `run.events` replays a run's log from disk
//!   ([`Replay`]) and follows it while it is live; `space.erase` deletes a space with its
//!   runs' sessions and records.
//! - **Events** go through one funnel ([`Runner::publish`]), under the run's own lock and
//!   no other: the run numbers its own events. An event that changes the run's state
//!   (`run.started`, each end) is made durable before it is published: the log so far is
//!   fsynced and the run's record rewritten with the event. Any other event is published,
//!   then appended, and fsynced with the run's next state event or every
//!   [`UNSYNCED_MAX`]. So an app never sees an end that a restart would not find, the log
//!   holds every event in `run_seq` order, and a slow disk under one run holds up no
//!   other run and no other topic: the bus's lock is held only in memory, and the
//!   runner's never across a write.
//! - **The connector gate** (5.5, [`gate`]): a run with `needs_connectors` makes up to
//!   three tries, each its own Claude Code, and starts no turn until each named
//!   connector is `connected`. Each `system/init` taken as a run's truth updates the
//!   connectors' cache ([`crate::connectors`]).
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
    Data, ErrorBody, Event, FailReason, Mount, NeedsYouClearedData, NeedsYouRaisedData,
    NeedsYouReason, RunCancelData, RunDoneData, RunEventsData, RunEventsParams, RunFailedData,
    RunGetData, RunId, RunKind, RunListData, RunListParams, RunNeedsYouData, RunQueuedData,
    RunRateLimitedData, RunRef, RunResumeData, RunResumeParams, RunStartParams, RunStartedData,
    RunState, RunSummary, RunTextData, RunToolResultData, SpaceEraseData, codes, run_types, topics,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin};
use tokio::sync::watch;

use crate::bus::{self, Bus};
use crate::connectors::Connectors;
use crate::log::{info, warning};
use crate::{claude, clock, spaces};

mod argv;
mod call;
mod connector_gate;
mod gate;
#[cfg(unix)]
pub(crate) mod guard;
pub(crate) mod jcs;
mod probe;
mod process;
mod remove;
mod store;
mod stream;

use connector_gate::{Gate, Verdict};
pub use probe::probe_deny;
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

/// The scheduler job a run was fired for (7.6): its id, its `loud`, which rlxd only
/// echoes on the run's `run.done` and `run.needs_you`, and whether it is a watch job,
/// whose pinned call's result is `job.watch_result`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobTag {
    pub job_id: String,
    pub loud: bool,
    pub watch: bool,
}

/// What a run a scheduler job fired did that the scheduler acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunNote {
    /// A `rate_limit_event`: until `resets_at` (RFC 3339), in `status` (Claude Code's
    /// word: `allowed_warning` lets the run go on, `rejected` means the limit is hit).
    RateLimited {
        job_id: String,
        run_id: RunId,
        resets_at: Option<String>,
        status: Option<String>,
    },
    /// The run's attempt ended in `state`.
    Ended {
        job_id: String,
        run_id: RunId,
        state: RunState,
    },
    /// A `connectors.call` run found no route to a call with no model turn (a Claude
    /// Code without `mcp_call_check_permissions_v1`): its watch fire falls back (5.3).
    /// Noted before the run's `Ended`.
    NoRoute { job_id: String, run_id: RunId },
    /// A watch run's pinned call, allowed, gave its result (7.6): `content` as
    /// `run.tool_result` carries it, bounded.
    WatchResult {
        job_id: String,
        run_id: RunId,
        tool: String,
        input_sha256: String,
        content: Value,
        is_error: bool,
        truncated: bool,
    },
}

pub use gate::Ask;
pub(crate) use gate::claude_server_name;
#[cfg(feature = "debug-methods")]
pub use gate::{HOOK_KEY_ENV, output as hook_output};

/// The runner of one rlxd.
pub struct Runner {
    home: Home,
    bus: Arc<Bus>,
    /// The connectors' cache, which each run's `system/init` updates.
    connectors: Arc<Connectors>,
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
    /// Test builds only: a space whose `run.start`s write their records this long, and so
    /// stay being started (`RLXD_TEST_SLOW_START=<space>:<ms>`).
    slow_start: Option<(String, Duration)>,
    /// Test builds only: a space whose erases take this long before they remove anything
    /// (`RLXD_TEST_SLOW_ERASE=<space>:<ms>`).
    slow_erase: Option<(String, Duration)>,
    /// Test builds only: a space whose runs' state events wait this long, once published,
    /// before their lines are appended to the log (`RLXD_TEST_SLOW_APPEND=<space>:<ms>`).
    slow_append: Option<(String, Duration)>,
    /// Test builds only: how long `run.cancel` waits before it answers, once the attempt
    /// it cancelled has ended (`RLXD_TEST_CANCEL_REPLY_DELAY_MS`): room for a resume.
    cancel_reply_delay: Option<Duration>,
    /// Test builds only: how long `run.resume` waits with the run held for it, before it
    /// decides (`RLXD_TEST_RESUME_DELAY_MS`): room for an erase or another resume.
    resume_delay: Option<Duration>,
    runs: Mutex<Runs>,
    /// Set once rlxd stops: no run starts from then on.
    stopping: AtomicBool,
    /// Held while the connectors' probe runs: one at a time.
    probing: tokio::sync::Mutex<()>,
    /// The probe's tree while it runs, for rlxd's stop to end.
    probe_tree: Mutex<Option<Arc<Tree>>>,
    /// The app gate's connections, as the gate counts them ([`Runner::follow_apps`]).
    apps: std::sync::OnceLock<watch::Receiver<u32>>,
    /// Test builds only: bumped to end every hook connection waiting on the gate
    /// (`debug.drop_hook_connections`).
    #[cfg(feature = "debug-methods")]
    hooks_dropped: watch::Sender<u64>,
    /// Test builds only: how many hook connections wait on the gate now.
    #[cfg(feature = "debug-methods")]
    hooks_waiting: watch::Sender<u64>,
    /// Test builds only: rlxd itself as each run's hook, and the subcommand it runs as
    /// (`rlxd __test-hook pre-tool-use`, which asks `debug.pre_tool_use`), until
    /// rlxd-hook's own pre-tool-use is wired (`RLXD_TEST_HOOK`).
    test_hook: Option<(PathBuf, &'static str)>,
    /// Test builds only: the space whose runs' ledger writes fail
    /// (`RLXD_TEST_FAIL_LEDGER`).
    fail_ledger: Option<String>,
    /// Where notes on runs scheduler jobs fired go ([`Runner::notes`]).
    notes: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<RunNote>>,
}

#[derive(Default)]
struct Runs {
    by_id: HashMap<RunId, Arc<Run>>,
    pools: HashMap<RunKind, Pool>,
    /// Runs being started (their records written) or resumed (decided under their own
    /// locks), outside this lock, by id, with their spaces: an erase counts them live.
    reserved: HashMap<RunId, String>,
    /// Spaces being erased, outside this lock: no run starts or resumes in one.
    erasing: std::collections::HashSet<String>,
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
    /// The job's `loud`, for a run a scheduler job fired.
    loud: Option<bool>,
    /// Whether a watch job fired the run: then its pinned call's result goes to the
    /// scheduler as `job.watch_result`. Not kept across a restart, which ends the run.
    watch_job: bool,
    /// A `connectors.call` run's own facts ([`call`]): its attempt is one call on a
    /// control channel, and it is never resumed.
    call: Option<call::CallRun>,
    client: String,
    dir: RunDir,
    inner: Mutex<RunInner>,
    /// Changes with the run's state: what `run.cancel` and `run.events` wait on.
    state: watch::Sender<RunState>,
    /// The last `run_seq` in the run's log on disk: what `run.events` follows.
    logged: watch::Sender<u64>,
    /// Where the run's log ends after its last append that succeeded: how far a follower
    /// reads, so it never reads what an append that failed wrote before it was cut back.
    /// Set before `logged`.
    logged_len: std::sync::atomic::AtomicU64,
    /// What `run.list` shows of the run, as last published: its own lock, held for a copy
    /// and never over the disk, so a listing never waits on a run's lock.
    view: Mutex<View>,
    /// How each of its attempts ended, the latest last (at most [`ENDS_KEPT`]): what a
    /// `run.cancel` answers from, the attempt it cancelled's own end, whatever a resume did
    /// since.
    ends: watch::Sender<Vec<(u64, RunState)>>,
}

/// How many attempts' ends a run keeps for `run.cancel`.
const ENDS_KEPT: usize = 16;

/// A run as `run.list` shows it.
#[derive(Clone)]
struct View {
    state: RunState,
    attempt: u64,
    started_at: String,
    ended_at: Option<String>,
    last_run_seq: u64,
}

impl View {
    fn of(inner: &RunInner) -> Self {
        Self {
            state: inner.state,
            attempt: inner.attempt,
            started_at: inner.started_at.clone(),
            ended_at: inner.ended_at.clone(),
            last_run_seq: inner.last_run_seq,
        }
    }
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
    /// The event log, open to append while the run is in flight.
    events: Option<std::fs::File>,
    /// This attempt's assistant text: its last [`PARTIAL_TEXT`] bytes.
    text: String,
    /// The run's last event, as published.
    last_line: Option<bus::Line>,
    /// The next attempt's first user message, when it is not the prompt: a resume's.
    reply: Option<String>,
    /// The `needs_you.raised` the run has out, to clear when it goes on or ends.
    needs_you: Option<String>,
    /// The connectors its `needs_you {reason: "auth"}` waits on (5.5): it is cleared,
    /// too, once each is `connected` again. Empty when it has none out.
    auth: Vec<String>,
    /// Events appended to the log since its last fsync.
    unsynced: u32,
    /// The last `run_seq` in the run's log: an event whose append failed is not counted,
    /// so `run.events` never waits for it.
    logged: u64,
    /// Whether some attempt reached `run.started` with its session kept: only then is
    /// there a session for `run.resume`.
    session_started: bool,
    /// The gate's ledger of the run's calls (5.1), as `run.json` keeps it.
    calls: Vec<rlxd_proto::RunCall>,
    /// Whether the run's pinned call was allowed once: every later call is denied, in any
    /// attempt, after a restart too.
    pinned_call_used: bool,
    /// The running attempt's hook key ([`gate`]): in memory only, never logged or written.
    hook_key: Option<String>,
    /// What the attempt's `system/init` listed: its tools, its MCP servers and each
    /// tool's `readOnlyHint`, which rlxd's rules decide by. Empty until the init comes.
    listed: gate::Listed,
    /// The run's last state event, as its record keeps it until its log line is fsynced
    /// (`last_event`): every record written keeps it, the gate's too.
    last_state_event: Option<Value>,
    /// The calls the gate is deciding or decided in this rlxd, by `call_id`.
    gate: HashMap<String, gate::Status>,
    /// The call whose denial the app ended the run on (`run.decide` with `stop: true`).
    stopped: Option<String>,
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

/// What `run.cancel` did, under the run's lock.
enum Cancel {
    /// Ended the run, queued or waiting for the user, as it now is.
    Ended(RunState),
    /// Killed attempt `n`'s tree, whose end comes.
    Ends(u64),
}

/// How an attempt ended.
enum End {
    Done(RunDoneData),
    NeedsYou(RunNeedsYouData, RunDoneData),
    Failed(RunFailedData),
}

/// How one try of an attempt ended.
enum Tried {
    /// The attempt's end.
    End(End),
    /// A gated try that was not confirmed (see [`Streamed::Missed`]).
    Missed {
        connectors: Vec<rlxd_proto::ConnectorStatus>,
        stop: bool,
        init: Option<stream::Init>,
    },
}

/// Writes the first user message to Claude Code.
async fn send(stdin: &mut ChildStdin, first: &str) -> std::io::Result<()> {
    stdin.write_all(first.as_bytes()).await?;
    stdin.flush().await
}

/// What a read of Claude Code's output that failed says.
fn read_error(e: &stream::ReadError) -> String {
    match e {
        stream::ReadError::TooLong => format!(
            "line_too_long: Claude Code wrote a stream-json line longer than {} bytes, which \
             rlxd does not read",
            stream::MAX_LINE
        ),
        stream::ReadError::Io(e) => format!("Claude Code's output could not be read: {e}"),
    }
}

/// What `resume` says a Claude Code told to go on, when the app sends no reply.
const CONTINUE: &str = "Continue.";

/// What reading an attempt's stream came to.
enum Streamed {
    /// Its `result`, and its stdin, still open.
    Result(stream::Outcome, ChildStdin),
    /// A gated try that was not confirmed: the named connectors not connected, whether
    /// another try may find them (`stop` when not), and the `system/init` it read.
    Missed {
        connectors: Vec<rlxd_proto::ConnectorStatus>,
        stop: bool,
        init: Option<stream::Init>,
    },
    /// rlxd ended it, for this reason.
    Ended(FailReason),
    /// It could not go on: why.
    Broken(String),
    /// Claude Code's output ended before a `result`.
    Eof,
}

impl Runner {
    /// The runner of `home`, publishing on `bus`. It must be made inside rlxd's runtime.
    ///
    /// It reads every run an earlier rlxd recorded, so `run.list` and `run.get` know them.
    /// A run that was queued or running then is ended `failed {reason:
    /// "engine_restarted"}`, after its process tree, if it still runs, is killed: its
    /// leader's pid is killed only when the process with that pid started when the
    /// record says, so a pid the OS has given another process since is left alone.
    pub fn new(home: Home, bus: Arc<Bus>, connectors: Arc<Connectors>) -> Arc<Self> {
        let rlxd = std::env::current_exe().ok();
        // An erase holds a folder open per level it is down.
        #[cfg(unix)]
        if let Some(limit) = process::raise_fd_limit() {
            info!("open files: up to {limit}");
        }
        let runner = Arc::new(Self {
            home,
            bus,
            connectors,
            spawner: Spawner::start(),
            hook: argv::hook_beside(rlxd.as_deref()),
            launch_delay: test_delay("RLXD_TEST_LAUNCH_DELAY_MS"),
            slow_persist: test_slow_in("RLXD_TEST_SLOW_PERSIST"),
            slow_start: test_slow_in("RLXD_TEST_SLOW_START"),
            slow_erase: test_slow_in("RLXD_TEST_SLOW_ERASE"),
            slow_append: test_slow_in("RLXD_TEST_SLOW_APPEND"),
            cancel_reply_delay: test_delay("RLXD_TEST_CANCEL_REPLY_DELAY_MS"),
            resume_delay: test_delay("RLXD_TEST_RESUME_DELAY_MS"),
            runs: Mutex::new(Runs::default()),
            stopping: AtomicBool::new(false),
            probing: tokio::sync::Mutex::new(()),
            probe_tree: Mutex::new(None),
            apps: std::sync::OnceLock::new(),
            #[cfg(feature = "debug-methods")]
            hooks_dropped: watch::Sender::new(0),
            #[cfg(feature = "debug-methods")]
            hooks_waiting: watch::Sender::new(0),
            test_hook: test_hook(rlxd.as_deref()),
            fail_ledger: test_space("RLXD_TEST_FAIL_LEDGER"),
            notes: std::sync::OnceLock::new(),
        });
        runner.load();
        let told = Arc::downgrade(&runner);
        runner.connectors.on_connected(Box::new(move |connected| {
            if let Some(runner) = told.upgrade() {
                runner.signed_in(connected);
            }
        }));
        runner
    }

    /// Reads the runs on disk, ending those an earlier rlxd left in flight.
    fn load(&self) {
        let Ok(entries) = std::fs::read_dir(store::runs_dir(&self.home)) else {
            return;
        };
        let boot = process::boot_id();
        let mut restarted = Vec::new();
        let mut put_back = Vec::new();
        for entry in entries.flatten() {
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|name| RunId::new(name).ok())
            else {
                continue;
            };
            let dir = RunDir::of(&self.home, &id);
            let mut record = match dir.read_record() {
                Ok(record) if record.run_id == id && record.version <= store::RECORD_VERSION => {
                    record
                }
                Ok(record) if record.run_id != id => {
                    warning!("run {id}: its record names another run; it is left as it is");
                    continue;
                }
                Ok(record) => {
                    warning!(
                        "run {id}: its record is version {}, newer than this rlxd reads; it is \
                         left as it is",
                        record.version
                    );
                    continue;
                }
                Err(e) => {
                    warning!("run {id}: its record could not be read ({e}); it is left as it is");
                    continue;
                }
            };
            // A record written in another boot: its processes are gone, and its last
            // unsynced events may be too.
            let rebooted = matches!((&record.boot, &boot), (Some(then), Some(now)) if !process::same_boot(then, now));
            // Swept only in the boot it ran in, known to be: in another, its pid and group
            // id may be any process's, and on Linux its start time counts from another boot.
            let this_boot = matches!((&record.boot, &boot), (Some(then), Some(now)) if process::same_boot(then, now));
            if record.state.is_live()
                && this_boot
                && let Some(process) = &record.process
                && let Some(started) = &process.started
                && process::sweep(process.pid, started, id.as_str())
            {
                info!("run {id}: ended what an earlier rlxd left of its process group");
            }
            if let Err(e) = store::repair(&dir.events()) {
                warning!("run {id}: its log could not be repaired: {e}");
            }
            let tail = store::tail(&dir.events()).unwrap_or_else(|e| {
                warning!("run {id}: its log could not be read: {e}");
                store::Tail::default()
            });
            // An end the log holds and the record doesn't: the crash came between them.
            if record.state.is_live()
                && let Some(adopted) = store::adopt_end(&tail, record.attempt, &mut record)
            {
                info!("run {id}: took its logged {adopted} into its record");
                if let Err(e) = dir.write_record(&record) {
                    warning!("run {id}: its record was not written: {e}");
                }
            }
            let log_len = std::fs::metadata(dir.events()).map_or(0, |log| log.len());
            let live = record.state.is_live();
            let text = if live {
                store::attempt_text(&dir.events(), record.attempt)
            } else {
                String::new()
            };
            // A state event the record holds and the log lost (a crash between the two):
            // it is put back, numbered as before.
            let lost = record
                .last_event
                .as_ref()
                .filter(|event| {
                    event["run_seq"]
                        .as_u64()
                        .is_some_and(|n| n > tail.last_run_seq)
                })
                .cloned();
            let mut last_run_seq = match &lost {
                Some(event) => event["run_seq"].as_u64().unwrap_or(1) - 1,
                None => record.last_run_seq.max(tail.last_run_seq),
            };
            // Up to UNSYNCED_MAX events a client saw may be gone with the old boot: what
            // comes next is numbered past them, so no run_seq is given twice. A lost state
            // event is put back at its own run_seq first.
            let skip = if live && rebooted {
                u64::from(UNSYNCED_MAX)
            } else {
                0
            };
            if lost.is_none() {
                last_run_seq += skip;
            }
            // A failed run's sign-in needs-you stays out until its connectors are back.
            let auth = if record.state == RunState::Failed {
                record.auth.clone()
            } else {
                Vec::new()
            };
            let needs_you = (record.state == RunState::NeedsYou || !auth.is_empty())
                .then(|| format!("run:{id}:{}", record.attempt));
            let run = Arc::new(Run {
                job_id: record.job_id.clone(),
                loud: record.loud,
                watch_job: false,
                call: record.connectors_call.then(call::CallRun::ended),
                client: record.client.clone(),
                dir,
                inner: Mutex::new(RunInner {
                    state: record.state,
                    attempt: record.attempt,
                    started_at: record.started_at.clone(),
                    ended_at: record.ended_at.clone(),
                    last_run_seq,
                    session: record.session.clone(),
                    session_started: record.session_started,
                    structured_output: record.structured_output.clone(),
                    process: None,
                    tree: None,
                    ending: None,
                    placed: false,
                    seeded: false,
                    events: None,
                    text,
                    last_line: tail.last_line.map(bus::Line::from),
                    reply: None,
                    needs_you,
                    auth,
                    unsynced: 0,
                    logged: tail.last_run_seq,
                    // A call still executing when its rlxd stopped may have run: unknown.
                    calls: record
                        .calls
                        .iter()
                        .cloned()
                        .map(|mut call| {
                            if call.state == rlxd_proto::CallState::Executing {
                                call.state = rlxd_proto::CallState::Unknown;
                            }
                            call
                        })
                        .collect(),
                    pinned_call_used: record.pinned_call_used,
                    hook_key: None,
                    listed: gate::Listed::default(),
                    last_state_event: record.last_event.clone(),
                    gate: HashMap::new(),
                    stopped: None,
                }),
                state: watch::Sender::new(record.state),
                logged: watch::Sender::new(tail.last_run_seq),
                logged_len: std::sync::atomic::AtomicU64::new(log_len),
                view: Mutex::new(View {
                    state: record.state,
                    attempt: record.attempt,
                    started_at: record.started_at.clone(),
                    ended_at: record.ended_at.clone(),
                    last_run_seq,
                }),
                ends: watch::Sender::new(if record.state.is_live() {
                    Vec::new()
                } else {
                    vec![(record.attempt, record.state)]
                }),
                params: record.params,
            });
            if let Some(event) = lost {
                put_back.push((Arc::clone(&run), event, skip));
            }
            if live {
                restarted.push(Arc::clone(&run));
            }
            self.runs().by_id.insert(id, run);
        }
        for (run, event, skip) in put_back {
            let mut inner = run.inner();
            let r#type = event["type"].as_str().unwrap_or_default().to_owned();
            let Some(Value::Object(data)) = event.get("data").cloned() else {
                continue;
            };
            info!(
                "run {}: put back its {type}, which its log had lost",
                run.params.run_id
            );
            let _ = self.publish(&run, &mut inner, &r#type, data, Durability::Logged);
            inner.last_run_seq += skip;
        }
        for run in restarted {
            let mut inner = run.inner();
            let failed = RunFailedData::new(
                FailReason::EngineRestarted,
                "rlxd stopped while the run was in flight; run.resume goes on with it",
                inner.text.clone(),
            );
            self.end(&run, &mut inner, End::Failed(failed));
        }
    }

    /// Notes on the runs scheduler jobs fire, from now on: for the scheduler, once.
    pub fn notes(&self) -> tokio::sync::mpsc::UnboundedReceiver<RunNote> {
        let (send, notes) = tokio::sync::mpsc::unbounded_channel();
        if self.notes.set(send).is_err() {
            warning!("runner: a second follower of job runs' notes is ignored");
        }
        notes
    }

    fn note(&self, note: RunNote) {
        if let Some(notes) = self.notes.get() {
            let _ = notes.send(note);
        }
    }

    /// Whether run `id` is queued or running, without its lock: for a job's overlap.
    pub fn live(&self, id: &RunId) -> bool {
        self.state(id).is_some_and(RunState::is_live)
    }

    /// Run `id`'s state now, without its lock; `None` for a run rlxd doesn't know.
    pub fn state(&self, id: &RunId) -> Option<RunState> {
        self.runs().by_id.get(id).map(|run| *run.state.borrow())
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
    /// enforces it, 5.1), `run_exists`, or `run_start_failed` when rlxd is stopping or the
    /// run's record can't be written.
    pub fn start(
        self: &Arc<Self>,
        params: RunStartParams,
        client: &str,
        job: Option<JobTag>,
    ) -> Result<RunRef, ErrorBody> {
        check_start(&params)?;
        let (job_id, loud, watch_job) = match job {
            Some(job) => (Some(job.job_id), Some(job.loud), job.watch),
            None => (None, None, false),
        };
        self.start_checked(params, client, job_id, loud, watch_job, None)
    }

    /// [`Runner::start`], its params checked; `call` for a `connectors.call` run.
    fn start_checked(
        self: &Arc<Self>,
        params: RunStartParams,
        client: &str,
        job_id: Option<String>,
        loud: Option<bool>,
        watch_job: bool,
        call: Option<call::CallRun>,
    ) -> Result<RunRef, ErrorBody> {
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
            if runs.erasing.contains(&params.space) {
                return Err(refusal(
                    codes::RUN_START_FAILED,
                    format!(
                        "the space {:?} is being erased: start the run once it is",
                        params.space
                    ),
                ));
            }
            if runs.by_id.contains_key(&id) || runs.reserved.contains_key(&id) {
                return Err(refusal(
                    codes::RUN_EXISTS,
                    format!("rlxd already knows a run {id}: pick a new run_id"),
                ));
            }
            runs.reserved.insert(id.clone(), params.space.clone());
        }
        // A folder rlxd didn't read at its start is a run too: the disk is looked at with
        // the id reserved, and no lock held.
        if blocking(|| dir.exists()) {
            self.runs().reserved.remove(&id);
            return Err(refusal(
                codes::RUN_EXISTS,
                format!("rlxd already knows a run {id}: pick a new run_id"),
            ));
        }
        let started_at = clock::now();
        let run = Arc::new(Run {
            job_id,
            loud,
            watch_job,
            call,
            client: client.to_owned(),
            dir,
            inner: Mutex::new(RunInner {
                state: RunState::Queued,
                attempt: 1,
                started_at: started_at.clone(),
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
                last_line: None,
                reply: None,
                needs_you: None,
                auth: Vec::new(),
                unsynced: 0,
                logged: 0,
                session_started: false,
                calls: Vec::new(),
                pinned_call_used: false,
                hook_key: None,
                listed: gate::Listed::default(),
                last_state_event: None,
                gate: HashMap::new(),
                stopped: None,
            }),
            state: watch::Sender::new(RunState::Queued),
            logged: watch::Sender::new(0),
            logged_len: std::sync::atomic::AtomicU64::new(0),
            view: Mutex::new(View {
                state: RunState::Queued,
                attempt: 1,
                started_at,
                ended_at: None,
                last_run_seq: 0,
            }),
            ends: watch::Sender::new(Vec::new()),
            params,
        });
        let created = blocking(|| {
            wait_if(self.slow_start.as_ref(), &run.params.space);
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
        if runs.erasing.contains(&run.params.space) {
            // The space began to be erased while the record was written: back out.
            drop(runs);
            let _ = std::fs::remove_dir_all(run.dir.path());
            return Err(refusal(
                codes::RUN_START_FAILED,
                format!(
                    "the space {:?} is being erased: start the run once it is",
                    run.params.space
                ),
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
        let mut ends = run.ends.subscribe();
        {
            // Out of its queue first, under the runner's lock, so no place goes to it. No
            // run's lock is taken under it: a run's may be held over a slow disk. (A run
            // not waiting is not in the queue, and stays out of it.)
            let mut runs = self.runs();
            if let Some(pool) = runs.pools.get_mut(&run.params.kind) {
                pool.waiting.retain(|waiting| !Arc::ptr_eq(waiting, &run));
            }
        }
        // The run's lock may be held over a slow disk: wait for it as blocking work, so no
        // other task waits behind this one on its worker.
        let decided = blocking(|| {
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
                    let ended = inner.state;
                    drop(inner);
                    self.release_if(placed, run.params.kind);
                    Ok(Cancel::Ended(ended))
                }
                RunState::Running => {
                    inner.ending.get_or_insert(FailReason::Cancelled);
                    // A tree not started yet sees `ending` before it starts.
                    if let Some(tree) = &inner.tree {
                        tree.kill();
                    }
                    Ok(Cancel::Ends(inner.attempt))
                }
                state @ (RunState::Done | RunState::Failed) => Err(state),
            }
        });
        let attempt = match decided {
            Err(state) => return Ok(RunCancelData::new(id.clone(), false, state)),
            Ok(Cancel::Ended(state)) => {
                return Ok(RunCancelData::new(
                    id.clone(),
                    state == RunState::Failed,
                    state,
                ));
            }
            Ok(Cancel::Ends(attempt)) => attempt,
        };
        // The answer is how the attempt it cancelled ended, whatever a resume did since.
        let of_it = |ends: &Vec<(u64, RunState)>| {
            ends.iter()
                .rev()
                .find(|(ended, _)| *ended == attempt)
                .map(|(_, state)| *state)
        };
        let _ =
            tokio::time::timeout(CANCEL_WAIT, ends.wait_for(|ends| of_it(ends).is_some())).await;
        if let Some(delay) = self.cancel_reply_delay {
            tokio::time::sleep(delay).await;
        }
        let ended = of_it(&ends.borrow());
        Ok(match ended {
            Some(state) => RunCancelData::new(id.clone(), state == RunState::Failed, state),
            // Not ended within the wait: as it is now.
            None => RunCancelData::new(id.clone(), false, *run.state.borrow()),
        })
    }

    /// `run.resume`: a run with no live process goes on as a new attempt, with the same
    /// `run_id` and `--resume <run_id>`, its own settings file, and the app's reply (or
    /// "Continue.") as its first message. It takes its place in its pool, as a start does.
    /// Refused: `run_alive` for a queued or running run, `not_resumable` for a
    /// `send_executor` (a send is never retried), `no_session` for a session-less run.
    pub fn resume(self: &Arc<Self>, params: RunResumeParams) -> Result<RunResumeData, ErrorBody> {
        if self.stopping.load(Ordering::SeqCst) {
            return Err(refusal(
                codes::RUN_START_FAILED,
                "rlxd is stopping: resume the run once it is back".to_owned(),
            ));
        }
        let run = self.find(&params.run_id)?;
        let id = params.run_id;
        {
            // The run is held for this resume under the runner's lock, and no run's lock
            // is taken under it: an erase of its space counts it live meanwhile, and a
            // second resume is refused.
            let mut runs = self.runs();
            // An erase may have taken the run between find() and here: then it is gone.
            if !runs
                .by_id
                .get(&id)
                .is_some_and(|known| Arc::ptr_eq(known, &run))
            {
                return Err(refusal(
                    codes::UNKNOWN_RUN,
                    format!("rlxd knows no run {id}"),
                ));
            }
            if runs.erasing.contains(&run.params.space) {
                return Err(refusal(
                    codes::RUN_START_FAILED,
                    format!("the space {:?} is being erased", run.params.space),
                ));
            }
            if runs.reserved.contains_key(&id) {
                return Err(refusal(
                    codes::RUN_ALIVE,
                    format!("run {id} is being resumed already"),
                ));
            }
            runs.reserved.insert(id.clone(), run.params.space.clone());
        }
        if let Some(delay) = self.resume_delay {
            std::thread::sleep(delay);
        }
        let resumed = self.resume_under_its_lock(&run, params.reply);
        let kind = run.params.kind;
        let queued = {
            let mut runs = self.runs();
            runs.reserved.remove(&id);
            if resumed.is_err() {
                None
            } else {
                let pool = runs.pools.entry(kind).or_default();
                if pool.active < kind.pool() {
                    pool.active += 1;
                    None
                } else {
                    pool.waiting.push_back(Arc::clone(&run));
                    Some(u32::try_from(pool.waiting.len()).unwrap_or(u32::MAX))
                }
            }
        };
        let attempt = resumed?;
        info!("run {id}: resumed, attempt {attempt}");
        match queued {
            None => self.launch(&run),
            Some(position) => {
                let mut inner = run.inner();
                // Unless a cancel ended it meanwhile: its place in the queue then goes on
                // when its turn comes (launch()).
                if inner.state == RunState::Queued {
                    let record = run.record(&inner);
                    if let Err(e) = blocking(|| run.dir.write_record(&record)) {
                        warning!("run {id}: its record was not written: {e}");
                    }
                    self.emit(
                        &run,
                        &mut inner,
                        run_types::QUEUED,
                        &RunQueuedData::new(position),
                    );
                }
            }
        }
        Ok(RunResumeData::new(id, attempt))
    }

    /// `run.resume`'s checks and its new attempt, under the run's own lock alone: the
    /// attempt's number, or the refusal.
    fn resume_under_its_lock(&self, run: &Run, reply: Option<String>) -> Result<u64, ErrorBody> {
        let id = &run.params.run_id;
        let mut inner = run.inner();
        if inner.state.is_live() {
            return Err(refusal(
                codes::RUN_ALIVE,
                format!(
                    "run {id} is {}: only a run with no live process resumes",
                    inner.state
                ),
            ));
        }
        if run.call.is_some() {
            return Err(refusal(
                codes::NOT_RESUMABLE,
                format!(
                    "run {id} is a connectors.call, which is never resumed: make the call again"
                ),
            ));
        }
        if run.params.kind == RunKind::SendExecutor {
            return Err(refusal(
                codes::NOT_RESUMABLE,
                format!(
                    "run {id} is a send_executor, which is never resumed: a send is never retried"
                ),
            ));
        }
        if run.params.persists() && !inner.session_started {
            return Err(refusal(
                codes::NO_SESSION,
                format!(
                    "run {id} never reached run.started, so Claude Code holds no session \
                     of it to resume: start a new run"
                ),
            ));
        }
        if !run.params.persists() {
            return Err(refusal(
                codes::NO_SESSION,
                format!(
                    "run {id} kept no session (persist_session: false), so there is none to resume"
                ),
            ));
        }
        inner.attempt += 1;
        inner.ending = None;
        inner.reply = reply;
        inner.ended_at = None;
        inner.state = RunState::Queued;
        run.state.send_replace(RunState::Queued);
        self.clear_needs_you(&mut inner);
        run.show(&inner);
        Ok(inner.attempt)
    }

    /// `run.get`: one run, from what rlxd holds of it, which its record and log on disk
    /// hold too, so it survives a restart.
    pub fn get(&self, id: &RunId) -> Result<RunGetData, ErrorBody> {
        let run = self.find(id)?;
        let inner = run.inner();
        let mut data = RunGetData::new(
            id.clone(),
            run.params.kind,
            inner.state,
            inner.started_at.clone(),
        );
        data.attempt = inner.attempt;
        data.space.clone_from(&run.params.space);
        data.client.clone_from(&run.client);
        data.job_id.clone_from(&run.job_id);
        data.ended_at.clone_from(&inner.ended_at);
        data.finished_at.clone_from(&inner.ended_at);
        data.last_run_seq = inner.last_run_seq;
        data.last_event = inner
            .last_line
            .as_ref()
            .and_then(|line| serde_json::from_slice::<Event>(line).ok());
        data.session.clone_from(&inner.session);
        data.structured_output.clone_from(&inner.structured_output);
        // The gate's ledger: what the app reads of a send after any end (5.1).
        data.calls.clone_from(&inner.calls);
        Ok(data)
    }

    /// `run.list`: every run rlxd knows, live or ended, those from before a restart too,
    /// the latest started first, of `states` and `job_id` when given, at most `limit`.
    pub fn list(&self, params: &RunListParams) -> RunListData {
        // Each run's view is read with the runner's lock let go, and not its own lock,
        // which may be held over a slow disk.
        let mut summaries: Vec<RunSummary> = self
            .snapshot()
            .iter()
            .filter(|run| params.job_id.is_none() || run.job_id == params.job_id)
            .filter_map(|run| {
                let view = run.view();
                if let Some(states) = &params.states
                    && !states.contains(&view.state)
                {
                    return None;
                }
                let mut summary = RunSummary::new(
                    run.params.run_id.clone(),
                    run.params.kind,
                    view.state,
                    view.started_at,
                );
                summary.ended_at = view.ended_at;
                summary.last_run_seq = view.last_run_seq;
                summary.job_id.clone_from(&run.job_id);
                summary.client.clone_from(&run.client);
                summary.attempt = view.attempt;
                summary.space.clone_from(&run.params.space);
                Some(summary)
            })
            .collect();
        summaries.sort_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then_with(|| a.run_id.cmp(&b.run_id))
        });
        if let Some(limit) = params.limit {
            summaries.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        RunListData::new(summaries)
    }

    /// `run.events`: where a replay of the run's log on disk starts. It never reads the
    /// bus's buffer, so a run the buffer has dropped replays whole.
    pub fn replay(&self, params: &RunEventsParams) -> Result<Replay, ErrorBody> {
        let run = self.find(&params.run_id)?;
        Ok(Replay {
            reader: store::Reader::new(run.dir.events()),
            from: params.from_run_seq,
            through: 0,
            logged: run.logged.subscribe(),
            states: run.state.subscribe(),
            run,
        })
    }

    /// `space.erase`: deletes the space's folder, the Claude Code session files of its
    /// runs, and rlxd's records of them (2.5).
    ///
    /// - Refused, removing nothing: `runs_live` while a run in the space is queued or
    ///   running; `link_refused`, with `detail.paths`, when a link (a junction on Windows)
    ///   is on the way: `<home>/spaces`, the space itself, a config directory, its
    ///   `projects`, or the space's project folder there.
    /// - A link inside the space goes as a link, never followed, and is named in `links`.
    /// - Session files are `projects/<encoded space dir>/<run_id>.jsonl` under each config
    ///   directory rlx knows ([`config_dirs`]), the space dir encoded as Claude Code does
    ///   ([`project_folder`]), for its resolved path and the path as given. No other
    ///   project folder is touched.
    /// - A run whose files can't all be removed keeps its record, and the erase fails
    ///   `erase_incomplete` with `detail {removed, failed}`; erasing again finishes it.
    /// - The runner's lock is held only to decide; every removal happens outside it, while
    ///   the space is marked as being erased, so no run starts in it meanwhile.
    pub fn erase(&self, name: &str) -> Result<SpaceEraseData, ErrorBody> {
        let root = spaces::root(&self.home);
        let space = root.join(name);
        let ours = {
            let mut runs = self.runs();
            if runs.erasing.contains(name) {
                return Err(refusal(
                    codes::ERASE_IN_PROGRESS,
                    format!("the space {name:?} is being erased already: wait for that erase"),
                ));
            }
            let ours: Vec<Arc<Run>> = runs
                .by_id
                .values()
                .filter(|run| run.params.space == name)
                .cloned()
                .collect();
            let mut live: Vec<String> = ours
                .iter()
                .filter(|run| run.state.borrow().is_live())
                .map(|run| run.params.run_id.to_string())
                .collect();
            // A run being started in the space counts as live.
            live.extend(
                runs.reserved
                    .iter()
                    .filter(|(_, space)| *space == name)
                    .map(|(id, _)| id.to_string()),
            );
            if !live.is_empty() {
                let message = format!(
                    "the space {name:?} has runs in flight ({}): cancel them, or wait for \
                     them, first",
                    live.join(", ")
                );
                return Err(with_detail(
                    refusal(codes::RUNS_LIVE, message),
                    json!({"run_ids": live}),
                ));
            }
            runs.erasing.insert(name.to_owned());
            ours
        };
        let erased = blocking(|| {
            wait_if(self.slow_erase.as_ref(), name);
            self.erase_files(&root, &space, &ours)
        });
        let mut runs = self.runs();
        runs.erasing.remove(name);
        let (removed, links, failed, gone) = erased?;
        for run in &gone {
            runs.by_id.remove(&run.params.run_id);
        }
        drop(runs);
        for run in &gone {
            let mut inner = run.inner();
            self.clear_needs_you(&mut inner);
        }
        if !failed.is_empty() {
            let message = format!(
                "the space {name:?} was erased only in part: {} paths could not be removed; \
                 erase it again once they can be",
                failed.len()
            );
            return Err(with_detail(
                refusal(codes::ERASE_INCOMPLETE, message),
                json!({"removed": removed, "failed": failed, "links": links}),
            ));
        }
        info!("space {name:?} erased: {} paths removed", removed.len());
        Ok(SpaceEraseData::new(removed, links))
    }

    /// The removals of `space.erase`, with no lock held: what it removed, the links inside
    /// the space it removed as links, what failed, and the runs now gone. Every folder on
    /// the way is opened once without following a link, and checked and removed through
    /// what was opened ([`remove`]): `<home>` and each config directory are roots, taken
    /// as they resolve; `spaces/`, the space, `projects/` and the space's project folder
    /// must not be links. The config directories are only gone into when the space has
    /// runs, whose sessions there are removed.
    #[allow(clippy::type_complexity)]
    fn erase_files(
        &self,
        root: &std::path::Path,
        space: &std::path::Path,
        ours: &[Arc<Run>],
    ) -> Result<(Vec<String>, Vec<String>, Vec<Value>, Vec<Arc<Run>>), ErrorBody> {
        use std::ffi::OsStr;
        let name = remove::name_of(space).to_owned();
        // The space as Claude Code saw it: resolved, before anything is removed.
        let resolved = spaces::resolved(space)
            .or_else(|_| spaces::resolved(root).map(|root| root.join(&name)))
            .unwrap_or_else(|_| space.to_path_buf());
        let mut folders = vec![project_folder(&resolved)];
        let given = project_folder(space);
        if !folders.contains(&given) {
            folders.push(given);
        }
        let internal = |e: std::io::Error| {
            refusal(
                codes::INTERNAL,
                format!("the space could not be erased: {e}"),
            )
        };
        let mut linked: Vec<String> = Vec::new();
        // The way to the space: <home> (a root), then spaces/, held.
        let spaces_held = match remove::walk(self.home.root(), &[OsStr::new("spaces")]) {
            Ok(Ok(held)) => held,
            Ok(Err(link)) => {
                linked.push(link.display().to_string());
                None
            }
            Err(e) => return Err(internal(e)),
        };
        if let Some(held) = &spaces_held
            && let Some(spaces_dir) = held.last()
            && matches!(
                spaces_dir.child(&name).map_err(internal)?,
                remove::Opened::Link | remove::Opened::Mount
            )
        {
            linked.push(spaces_dir.path().join(&name).display().to_string());
        }
        // The space's project folder in each config directory, held: only when the space
        // has runs, whose session files are there.
        let mut projects_held: Vec<Vec<remove::Dir>> = Vec::new();
        if !ours.is_empty() {
            for config in config_dirs() {
                for folder in &folders {
                    match remove::walk(&config, &[OsStr::new("projects"), OsStr::new(folder)]) {
                        Ok(Ok(Some(held))) => projects_held.push(held),
                        Ok(Ok(None)) => {}
                        Ok(Err(link)) => linked.push(link.display().to_string()),
                        Err(e) => return Err(internal(e)),
                    }
                }
            }
        }
        linked.sort();
        linked.dedup();
        if !linked.is_empty() {
            let message = format!(
                "erasing the space would go through links or mount points, which rlxd does \
                 not follow ({}); nothing was removed",
                linked.join(", ")
            );
            return Err(with_detail(
                refusal(codes::LINK_REFUSED, message),
                json!({"paths": linked}),
            ));
        }
        let mut removed = Vec::new();
        let mut links: Vec<String> = Vec::new();
        let mut failed: Vec<Value> = Vec::new();
        let fail = |failed: &mut Vec<Value>, path: &std::path::Path, e: &std::io::Error| {
            failed.push(json!({"path": path.display().to_string(), "error": e.to_string()}));
        };
        // Each run's session files, through the held project folders.
        let mut kept: Vec<&Arc<Run>> = Vec::new();
        for run in ours {
            let file = std::ffi::OsString::from(format!("{}.jsonl", run.params.run_id));
            let before = failed.len();
            for held in &projects_held {
                let Some(project) = held.last() else {
                    continue;
                };
                let path = project.path().join(&file);
                match project.remove_file(&file) {
                    Ok(true) => removed.push(path.display().to_string()),
                    Ok(false) => {}
                    Err(e) => fail(&mut failed, &path, &e),
                }
            }
            if failed.len() > before {
                kept.push(run);
            }
        }
        // The space, through the held spaces/ folder.
        if let Some(held) = &spaces_held
            && let Some(spaces_dir) = held.last()
            && !matches!(spaces_dir.child(&name), Ok(remove::Opened::Missing))
        {
            // Named under the space's resolved path, as removed[] and space.ensure name it.
            let held_space = spaces_dir.path().join(&name);
            let shown = |path: &std::path::Path| {
                path.strip_prefix(&held_space)
                    .map_or_else(|_| path.to_path_buf(), |rest| resolved.join(rest))
            };
            let mut found = Vec::new();
            match spaces_dir.remove_tree(&name, &mut found) {
                Ok(()) => removed.push(resolved.display().to_string()),
                Err(e) => fail(&mut failed, &shown(&e.path), &e.error),
            }
            links.extend(found.iter().map(|path| shown(path).display().to_string()));
        }
        // rlxd's records of its runs: its own folder.
        let mut gone = Vec::new();
        for run in ours {
            if kept.iter().any(|k| Arc::ptr_eq(k, run)) {
                // Its record stays, so erasing again finds the run and finishes.
                continue;
            }
            match std::fs::remove_dir_all(run.dir.path()) {
                Ok(()) => {
                    removed.push(run.dir.path().display().to_string());
                    gone.push(Arc::clone(run));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => gone.push(Arc::clone(run)),
                Err(e) => fail(&mut failed, run.dir.path(), &e),
            }
        }
        drop(projects_held);
        drop(spaces_held);
        Ok((removed, links, failed, gone))
    }

    /// Ends every run in flight as rlxd stops: no run starts from then on, each process
    /// tree is killed, and each run, queued or running, ends `failed {reason:
    /// "engine_restarted"}`, which `run.resume` goes on from.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(tree) = self
            .probe_tree
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            tree.kill();
        }
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
            run.show(&inner);
            let record = run.record(&inner);
            if let Err(e) = blocking(|| run.dir.write_record(&record)) {
                warning!("run {}: its record was not written: {e}", run.params.run_id);
            }
        }
        let runner = Arc::clone(self);
        let run = Arc::clone(run);
        tokio::spawn(async move {
            let end = if run.call.is_some() {
                runner.call_attempt(&run).await
            } else {
                runner.attempt(&run).await
            };
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
        let mut question = None;
        let mut auth = Vec::new();
        let (state, r#type, data) = match end {
            End::Done(done) => {
                inner.session.clone_from(&done.session);
                inner.structured_output.clone_from(&done.structured_output);
                (RunState::Done, run_types::DONE, serde_json::to_value(&done))
            }
            End::NeedsYou(needs, done) => {
                inner.session.clone_from(&done.session);
                inner.structured_output.clone_from(&done.structured_output);
                question = Some(needs.clone());
                (
                    RunState::NeedsYou,
                    run_types::NEEDS_YOU,
                    serde_json::to_value(&needs),
                )
            }
            End::Failed(failed) => {
                auth = failed
                    .connectors
                    .iter()
                    .flatten()
                    .filter(|c| c.status == rlxd_proto::ConnectorState::NeedsAuth.as_str())
                    .map(|c| c.id.clone())
                    .collect::<Vec<_>>();
                (
                    RunState::Failed,
                    run_types::FAILED,
                    serde_json::to_value(&failed),
                )
            }
        };
        // A needs-you that was out is settled by any end, a new needs-you included.
        self.clear_needs_you(inner);
        // A call still open as the run ends is denied; one still executing may have run
        // (5.1). The attempt's hook key goes, so no hook of it is answered from now on.
        self.settle_open(run, inner);
        for call in &mut inner.calls {
            if call.state == rlxd_proto::CallState::Executing {
                call.state = rlxd_proto::CallState::Unknown;
            }
        }
        inner.hook_key = None;
        inner.state = state;
        inner.ended_at = Some(clock::now());
        inner.tree = None;
        inner.process = None;
        if let Some(needs) = question {
            // Raised before the run's own event, so a capture that stops at
            // run.needs_you holds it (7.3).
            let id = format!("run:{}:{}", run.params.run_id, inner.attempt);
            let reason = match needs.reason {
                NeedsYouReason::Question => "question",
                NeedsYouReason::Denied => "denied",
            };
            let text = needs
                .question
                .clone()
                .unwrap_or_else(|| "The run needs you.".to_owned());
            let raised = NeedsYouRaisedData::run(&id, run.params.run_id.as_str(), reason, text);
            self.publish_needs_you(run_types::NEEDS_YOU_RAISED, &raised);
            inner.needs_you = Some(id);
        }
        if !auth.is_empty() {
            // A connector's sign-in lapsed (5.5). Raised before run.failed, as a question
            // is, and kept in the run's record. The user signs it in again
            // (`connectors.login`, or Claude Code itself), and once rlxd sees each
            // connector `connected` again (the login's end, or a later system/init) it is
            // cleared ([`Runner::signed_in`]); a resume or the space's erase clears it
            // too. A run that never reached run.started has no session to resume
            // (`no_session`): a new run is the way on.
            let id = format!("run:{}:{}", run.params.run_id, inner.attempt);
            let text = format!(
                "Sign in again to {}: its sign-in has lapsed.",
                auth.join(", ")
            );
            let raised = NeedsYouRaisedData::run(&id, run.params.run_id.as_str(), "auth", text);
            self.publish_needs_you(run_types::NEEDS_YOU_RAISED, &raised);
            inner.needs_you = Some(id);
            inner.auth = auth;
        }
        let published = match data {
            Ok(Value::Object(data)) => self.publish(run, inner, r#type, data, Durability::State),
            other => Err(format!("it did not print as an object: {other:?}")),
        };
        if let Err(why) = published {
            // A run always gets its end: a small one, when its own can't be published. What
            // the unpublished end said is not kept.
            inner.session = None;
            inner.structured_output = None;
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
        inner.events = None;
        let state = inner.state;
        self.bus.end_run(run.params.run_id.as_str());
        inner.seeded = false;
        run.state.send_replace(state);
        run.show(inner);
        run.ended_as(inner.attempt, state);
        if let Some(job_id) = &run.job_id {
            self.note(RunNote::Ended {
                job_id: job_id.clone(),
                run_id: run.params.run_id.clone(),
                state,
            });
        }
        info!("run {}: {state}", run.params.run_id);
        std::mem::take(&mut inner.placed)
    }

    /// Clears the `needs_you.raised` the run has out, if it has one.
    fn clear_needs_you(&self, inner: &mut RunInner) {
        inner.auth.clear();
        if let Some(id) = inner.needs_you.take() {
            self.publish_needs_you(run_types::NEEDS_YOU_CLEARED, &NeedsYouClearedData::new(id));
        }
    }

    /// Clears each failed run's `needs_you {reason: "auth"}` whose connectors are all
    /// among `connected` (5.5): their sign-in is back. Called with no run's lock held.
    fn signed_in(&self, connected: &[String]) {
        for run in self.snapshot() {
            let mut inner = run.inner();
            if inner.auth.is_empty() || !inner.auth.iter().all(|id| connected.contains(id)) {
                continue;
            }
            info!(
                "run {}: its connectors are signed in again; its needs-you is cleared",
                run.params.run_id
            );
            self.clear_needs_you(&mut inner);
            let record = run.record(&inner);
            if let Err(e) = blocking(|| run.dir.write_record(&record)) {
                warning!("run {}: its record was not written: {e}", run.params.run_id);
            }
        }
    }

    /// Publishes `data` on the `needs_you` topic.
    fn publish_needs_you(&self, r#type: &str, data: &impl Serialize) {
        let published = match serde_json::to_value(data) {
            Ok(Value::Object(data)) => self
                .bus
                .publish(topics::NEEDS_YOU, r#type, data)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            other => Err(format!("it did not print as an object: {other:?}")),
        };
        if let Err(e) = published {
            warning!("{type} was not published: {e}");
        }
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
            let settings = match &self.test_hook {
                Some((rlxd, via)) => argv::settings_via(params, attempt, rlxd, Some(via)),
                None => argv::settings(params, attempt, hook),
            };
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
        let gate = self.gate_of(params);
        let deadline = params
            .timeout_s
            .map(|s| Instant::now() + Duration::from_secs(s));
        // What a missed try may add to the session, to take back: a resumed attempt's
        // session as it was before its first try.
        let kept = gate
            .as_ref()
            .map(|_| self.session_kept(run, &space, attempt));
        let mut tries = 0;
        loop {
            tries += 1;
            let mut confirmed = false;
            let end = match self
                .try_once(
                    run,
                    attempt,
                    &program,
                    &space,
                    &settings,
                    gate.as_ref(),
                    deadline,
                    &mut confirmed,
                )
                .await
            {
                Tried::End(end) => {
                    // A gated try that ended before its connectors were confirmed (a
                    // cancel, the timeout, rlxd stopping, a broken read, Claude Code's
                    // exit) is taken back as a miss is: on route 2 it may have written
                    // the undelivered message to the session.
                    if !confirmed && let Some(kept) = &kept {
                        blocking(|| self.forget_try(run, kept));
                    }
                    end
                }
                Tried::Missed {
                    connectors,
                    stop,
                    init,
                } => {
                    info!(
                        "run {}: try {tries} of {}: {}",
                        params.run_id,
                        connector_gate::TRIES,
                        connector_gate::failure_message(&connectors, tries)
                    );
                    if let Some(kept) = &kept {
                        blocking(|| self.forget_try(run, kept));
                    }
                    if !stop && tries < connector_gate::TRIES {
                        if let Some(reason) = self
                            .back_off(run, connector_gate::backoff(tries), deadline)
                            .await
                        {
                            return ended_early(run, reason);
                        }
                        continue;
                    }
                    if let Some(init) = init.filter(|_| reads_connectors(params)) {
                        blocking(|| self.connectors.observe(init.observed()));
                    }
                    End::Failed(
                        RunFailedData::new(
                            FailReason::Connectors,
                            connector_gate::failure_message(&connectors, tries),
                            "",
                        )
                        .with_connectors(connectors),
                    )
                }
            };
            let ending = run.inner().ending;
            return match ending {
                // rlxd ended the run as its result came: rlxd's reason stands.
                Some(reason) if !matches!(&end, End::Failed(f) if f.reason == reason) => {
                    ended_early(run, reason)
                }
                _ => end,
            };
        }
    }

    /// The gate of a run with `needs_connectors`, on this rlxd's route; `None` for a run
    /// without.
    fn gate_of(&self, params: &RunStartParams) -> Option<Gate> {
        if params.needs_connectors.is_empty() {
            return None;
        }
        let times = self.connectors.gate_times();
        Some(Gate {
            named: params.needs_connectors.clone(),
            route_1: self.connectors.route_1(),
            wait: times.wait,
            settle: times.settle,
        })
    }

    /// The session files a gated attempt's tries write to, each with its length before
    /// the first try when the attempt resumes one ([`Kept`]). None for a run whose
    /// session isn't kept.
    fn session_kept(&self, run: &Run, space: &std::path::Path, attempt: u64) -> Kept {
        if !run.params.persists() {
            return Kept::default();
        }
        let file = format!("{}.jsonl", run.params.run_id);
        let files = config_dirs()
            .into_iter()
            .map(|dir| {
                let path = dir.join("projects").join(project_folder(space)).join(&file);
                let length = std::fs::symlink_metadata(&path)
                    .ok()
                    .filter(|m| m.file_type().is_file())
                    .map(|m| m.len());
                (path, length)
            })
            .collect();
        Kept {
            resumed: attempt > 1,
            files,
        }
    }

    /// After a gated try that was ended unconfirmed (a miss, or any other end before its
    /// connectors were confirmed): what it wrote to its session is taken back, so the
    /// next try (or a later resume) has nothing the run didn't deliver. A first attempt's
    /// session started with the try, so its file goes. A resumed attempt's file is the
    /// run's whole session: it is never removed, only cut back to its length before the
    /// attempt's first try, and left as it is (with a warning) when it is shorter than
    /// that or is no file.
    fn forget_try(&self, run: &Run, kept: &Kept) {
        for (path, before) in &kept.files {
            let now = std::fs::symlink_metadata(path)
                .ok()
                .filter(|m| m.file_type().is_file())
                .map(|m| m.len());
            let forgotten = match (kept.resumed, before, now) {
                (_, _, None) => Ok(()),
                (false, _, Some(_)) => std::fs::remove_file(path),
                (true, Some(before), Some(now)) if now > *before => std::fs::OpenOptions::new()
                    .write(true)
                    .open(path)
                    .and_then(|file| file.set_len(*before)),
                (true, Some(before), Some(now)) if now < *before => Err(std::io::Error::other(
                    "it is shorter than before the attempt, so it is left as it is",
                )),
                (true, Some(_), Some(_)) => Ok(()),
                (true, None, Some(_)) => Err(std::io::Error::other(
                    "it was not there before the attempt, so it is left as it is",
                )),
            };
            if let Err(e) = forgotten {
                warning!(
                    "run {}: the last try's session was not taken back from {}: {e}",
                    run.params.run_id,
                    path.display()
                );
            }
        }
    }

    /// Waits `wait` between a gated run's tries: the reason when a cancel, rlxd stopping
    /// or the timeout ends the run meanwhile.
    async fn back_off(
        &self,
        run: &Run,
        wait: Duration,
        deadline: Option<Instant>,
    ) -> Option<FailReason> {
        let until = Instant::now() + wait;
        loop {
            if let Some(reason) = run.inner().ending {
                return Some(reason);
            }
            let now = Instant::now();
            if deadline.is_some_and(|at| now >= at) {
                return Some(*run.inner().ending.get_or_insert(FailReason::Timeout));
            }
            if now >= until {
                return None;
            }
            let step = (until - now).min(Duration::from_millis(50));
            tokio::time::sleep(step).await;
        }
    }

    /// One Claude Code of an attempt: started, streamed, and ended. `confirmed` says,
    /// after, whether its connectors were confirmed (always, ungated).
    #[allow(clippy::too_many_arguments, reason = "one try's facts, each its own")]
    async fn try_once(
        &self,
        run: &Arc<Run>,
        attempt: u64,
        program: &std::path::Path,
        space: &std::path::Path,
        settings: &std::path::Path,
        gate: Option<&Gate>,
        deadline: Option<Instant>,
        confirmed: &mut bool,
    ) -> Tried {
        let params = &run.params;
        let failed = |message: String| {
            Tried::End(End::Failed(RunFailedData::new(
                FailReason::Error,
                message,
                run.inner().text.clone(),
            )))
        };
        let ending = run.inner().ending;
        if let Some(reason) = ending {
            return Tried::End(ended_early(run, reason));
        }
        let mut env = run_env();
        // Every process of the run inherits it: the start sweep's way to know them.
        env.push((process::RUN_ID_ENV.into(), params.run_id.as_str().into()));
        // The attempt's hook key: its hooks inherit it, and the gate wants it back (5.1).
        let Some(key) = gate::new_key() else {
            return failed("the attempt's hook key could not be made: no randomness".to_owned());
        };
        env.push((gate::HOOK_KEY_ENV.into(), key.as_str().into()));
        {
            let mut inner = run.inner();
            inner.hook_key = Some(key);
            inner.listed = gate::Listed::default();
            inner.stopped = None;
        }
        let launch = Launch {
            program: program.to_owned(),
            args: argv::args(params, attempt, settings),
            cwd: space.to_owned(),
            env,
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
            .stream(
                run,
                attempt,
                stdin,
                BufReader::new(stdout),
                &tree,
                gate,
                deadline,
                confirmed,
            )
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
            Streamed::Missed {
                connectors,
                stop,
                init,
            } => {
                tree.kill();
                tree.reap(&mut child, None).await;
                let mut inner = run.inner();
                inner.tree = None;
                inner.process = None;
                drop(inner);
                return Tried::Missed {
                    connectors,
                    stop,
                    init,
                };
            }
            Streamed::Ended(reason) => {
                tree.kill();
                tree.reap(&mut child, None).await;
                ended_early(run, reason)
            }
            Streamed::Broken(message) => {
                tree.kill();
                tree.reap(&mut child, None).await;
                return failed(message);
            }
            Streamed::Eof => {
                let mut status = tree.reap(&mut child, Some(EXIT_GRACE)).await;
                if status.is_none() {
                    tree.kill();
                    status = tree.reap(&mut child, None).await;
                }
                let ending = run.inner().ending;
                if let Some(reason) = ending {
                    return Tried::End(ended_early(run, reason));
                }
                let said = stderr.await.unwrap_or_default();
                let how = how_it_ended(status, &tree);
                let mut message = format!("Claude Code {how} before its result");
                if !said.trim().is_empty() {
                    message.push_str(": ");
                    message.push_str(said.trim());
                }
                return failed(message);
            }
        };
        let (ending, stopped) = {
            let mut inner = run.inner();
            (inner.ending, inner.stopped.take())
        };
        Tried::End(match ending {
            // rlxd ended the run as its result came: rlxd's reason stands.
            Some(reason) if !matches!(&end, End::Failed(f) if f.reason == reason) => {
                ended_early(run, reason)
            }
            // The app ended the run as it denied a call: the user is wanted (5.1).
            None if stopped.is_some() => {
                let call_id = stopped.unwrap_or_default();
                let session = params.persists().then(|| params.run_id.to_string());
                let mut done = RunDoneData::new(session, "", rlxd_proto::Usage::default());
                done.loud = run.loud;
                let mut needs = RunNeedsYouData::denied(call_id);
                needs.loud = run.loud;
                End::NeedsYou(needs, done)
            }
            _ => end,
        })
    }

    /// Sends the prompt, then reads the stream until its `result`, its end, or the
    /// timeout. A cancel kills the tree, which ends the stream.
    ///
    /// A gated try ([`connector_gate`]) settles first on route 2, or holds the prompt on route 1
    /// until a `system/init` shows each named connector connected. Until its connectors
    /// are confirmed, nothing it reads is published, and a miss ends the read
    /// ([`Streamed::Missed`]). `delivered` is set once they are (at once, ungated).
    #[allow(clippy::too_many_arguments, reason = "one try's facts, each its own")]
    async fn stream<R: AsyncBufRead + Unpin>(
        &self,
        run: &Run,
        attempt: u64,
        mut stdin: ChildStdin,
        mut stdout: R,
        tree: &Tree,
        gate: Option<&Gate>,
        deadline: Option<Instant>,
        delivered: &mut bool,
    ) -> Streamed {
        let params = &run.params;
        let try_started = Instant::now();
        // A resumed attempt starts with the app's reply, or is told to go on. A try after
        // a gated miss sends what the missed one would have.
        let text = {
            let inner = run.inner();
            match &inner.reply {
                Some(reply) => reply.clone(),
                None if attempt > 1 => CONTINUE.to_owned(),
                None => params.prompt.clone(),
            }
        };
        let prompt = json!({"type": "user", "message": {"role": "user",
            "content": [{"type": "text", "text": text}]},
            "parent_tool_use_id": null, "session_id": params.run_id.as_str()});
        let mut first = prompt.to_string();
        first.push('\n');
        let timeout = move || async move {
            match deadline {
                Some(at) => tokio::time::sleep_until(at.into()).await,
                None => std::future::pending().await,
            }
        };
        let mut line = Vec::new();
        if let Some(gate) = gate.filter(|gate| !gate.route_1) {
            // Route 2: stdin open, nothing sent, for the settle. Claude Code writes
            // nothing before the first message; what it might is not read as the run's.
            let settled = tokio::time::Instant::from(
                try_started + gate.settle + connector_gate::START_ALLOWANCE,
            );
            loop {
                let read = tokio::select! {
                    read = stream::next_line(&mut stdout, &mut line, stream::MAX_LINE) => read,
                    () = tokio::time::sleep_until(settled) => break,
                    () = timeout() => {
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
                    Err(e) => return Streamed::Broken(read_error(&e)),
                }
            }
        }
        let mut sent = false;
        if !gate.is_some_and(|gate| gate.route_1) {
            if let Err(e) = send(&mut stdin, &first).await {
                return Streamed::Broken(format!(
                    "the prompt could not be sent to Claude Code: {e}"
                ));
            }
            sent = true;
        }
        let mut confirmed = gate.is_none();
        *delivered = confirmed;
        if confirmed {
            run.inner().reply = None;
        }
        // Route 1: the last states a try saw before its wait ran out.
        let mut last: Option<(Vec<rlxd_proto::ConnectorStatus>, stream::Init)> = None;
        let mut tools: HashMap<String, String> = HashMap::new();
        let mut started = false;
        loop {
            let waited = async {
                match gate.filter(|gate| gate.route_1 && !confirmed) {
                    Some(gate) => tokio::time::sleep_until((try_started + gate.wait).into()).await,
                    None => std::future::pending().await,
                }
            };
            let read = tokio::select! {
                read = stream::next_line(&mut stdout, &mut line, stream::MAX_LINE) => read,
                () = timeout() => {
                    let reason = *run.inner().ending.get_or_insert(FailReason::Timeout);
                    tree.kill();
                    return Streamed::Ended(reason);
                }
                () = waited => {
                    let (connectors, init) = match last.take() {
                        Some((connectors, init)) => (connectors, Some(init)),
                        None => (gate.map(Gate::none_seen).unwrap_or_default(), None),
                    };
                    return Streamed::Missed { connectors, stop: false, init };
                }
            };
            let ending = run.inner().ending;
            if let Some(reason) = ending {
                return Streamed::Ended(reason);
            }
            match read {
                Ok(Some(())) => {}
                Ok(None) => return Streamed::Eof,
                Err(e) => return Streamed::Broken(read_error(&e)),
            }
            for parsed in stream::parse(&line) {
                let parsed = match (parsed, confirmed, gate) {
                    (parsed, true, _) | (parsed, _, None) => parsed,
                    (Line::Init(init), false, Some(gate)) => {
                        match gate.verdict(&init.mcp_servers) {
                            Verdict::Connected => {
                                confirmed = true;
                                *delivered = true;
                                // Delivered from here on: a later attempt is a resume's.
                                run.inner().reply = None;
                                if !sent {
                                    if let Err(e) = send(&mut stdin, &first).await {
                                        return Streamed::Broken(format!(
                                            "the prompt could not be sent to Claude Code: {e}"
                                        ));
                                    }
                                    sent = true;
                                }
                                Line::Init(init)
                            }
                            Verdict::Stopped(connectors) => {
                                return Streamed::Missed {
                                    connectors,
                                    stop: true,
                                    init: Some(init),
                                };
                            }
                            Verdict::Missed(connectors) if gate.route_1 => {
                                last = Some((connectors, init));
                                continue;
                            }
                            Verdict::Missed(connectors) => {
                                return Streamed::Missed {
                                    connectors,
                                    stop: false,
                                    init: Some(init),
                                };
                            }
                        }
                    }
                    (Line::Result(_), false, Some(_)) => {
                        return Streamed::Broken(
                            "Claude Code ended its turn before its system/init named the \
                             run's connectors"
                                .to_owned(),
                        );
                    }
                    // Nothing of a try is the run's before its connectors are confirmed.
                    (_, false, Some(_)) => continue,
                };
                // The init to take as the truth of the connectors, once the run has it.
                let mut seen = None;
                let mut inner = run.inner();
                match parsed {
                    Line::Init(init) if !started => {
                        started = true;
                        // A no_tools run loads none, and says nothing of them.
                        if reads_connectors(params) {
                            seen = Some(init.clone());
                        }
                        inner.listed = gate::Listed {
                            tools: init.tools.iter().cloned().collect(),
                            servers: init
                                .mcp_servers
                                .iter()
                                .map(|server| gate::Server {
                                    name: server.name.clone(),
                                    source: init.sources.get(&server.name).cloned(),
                                })
                                .collect(),
                            hints: init.hints.clone(),
                        };
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
                        // The ledger's outcome for an allowed call (5.1), before its event.
                        let ran = self.tool_ran(run, &mut inner, &call_id, is_error);
                        let (content, truncated) = stream::bounded(content);
                        // A watch job's pinned call: its raw result goes to the scheduler,
                        // as job.watch_result (7.6). A run job's never does, whatever
                        // its template's kind.
                        if let (Some(ran), Some(job_id), Some(pinned)) =
                            (ran, &run.job_id, &run.params.pinned_call)
                            && run.watch_job
                            && run.params.kind == RunKind::Watch
                            && ran.tool == pinned.tool
                            && ran.input_sha256 == pinned.input_sha256
                        {
                            self.note(RunNote::WatchResult {
                                job_id: job_id.clone(),
                                run_id: run.params.run_id.clone(),
                                tool: ran.tool,
                                input_sha256: ran.input_sha256,
                                content: content.clone(),
                                is_error,
                                truncated,
                            });
                        }
                        let tool = tools.get(&call_id).cloned().unwrap_or_default();
                        let data = RunToolResultData::new(call_id, tool, is_error, content)
                            .truncated(truncated);
                        self.emit(run, &mut inner, run_types::TOOL_RESULT, &data);
                    }
                    Line::RateLimit { resets_at, status } => {
                        let resets_at =
                            resets_at.map(|s| clock::format(UNIX_EPOCH + Duration::from_secs(s)));
                        let data = RunRateLimitedData::new(resets_at.clone(), status.clone());
                        self.emit(run, &mut inner, run_types::RATE_LIMITED, &data);
                        if let Some(job_id) = &run.job_id {
                            self.note(RunNote::RateLimited {
                                job_id: job_id.clone(),
                                run_id: run.params.run_id.clone(),
                                resets_at,
                                status,
                            });
                        }
                    }
                    Line::Result(outcome) => return Streamed::Result(outcome, stdin),
                    Line::Init(_) | Line::Partial(_) | Line::Other => {}
                }
                drop(inner);
                // Into the connectors' cache, its file written with no run's lock held, and
                // only after the gate has what the init listed: a hook that asks about the
                // turn's first call never waits on the cache (5.1).
                if let Some(init) = seen {
                    blocking(|| self.connectors.observe(init.observed()));
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
                    if run.params.persists() {
                        inner.session_started = true;
                    }
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
            // Durable: the run's new state is what everything that reads it sees from now
            // on, before anyone hears of the event. A client acting on run.done never
            // finds the run live (space.erase, status, run.list).
            run.state.send_replace(inner.state);
            let mut view = View::of(inner);
            view.last_run_seq = run_seq;
            run.set_view(view);
            if !inner.state.is_live() {
                run.ended_as(inner.attempt, inner.state);
            }
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
        inner.last_line = Some(bus::Line::clone(&line));
        run.show(inner);
        let logged = blocking(|| {
            if durability == Durability::State {
                wait_if(self.slow_append.as_ref(), &run.params.space);
            }
            log_line(run, inner, run_seq, r#type, &line)
        });
        if let Some(end) = logged {
            inner.logged = run_seq;
            run.logged_len.store(end, Ordering::SeqCst);
            run.logged.send_replace(run_seq);
        }
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
        wait_if(self.slow_persist.as_ref(), &run.params.space);
        if let Some(events) = inner.events.as_mut()
            && let Err(e) = events.sync_data()
        {
            warning!("run {id}: its log could not be fsynced: {e}");
        }
        inner.unsynced = 0;
        inner.last_state_event = Some(json!({"run_seq": run_seq, "type": r#type, "data": data}));
        let mut record = run.record(inner);
        record.last_run_seq = run_seq;
        if let Err(e) = run.dir.write_record(&record) {
            warning!("run {id}: its record was not written: {e}");
        }
    }
}

/// Appends the published event `run_seq`'s line to the run's log, fsyncing it after
/// [`UNSYNCED_MAX`] unsynced appends: where the log ends then, or `None` when it wasn't
/// appended.
fn log_line(
    run: &Run,
    inner: &mut RunInner,
    run_seq: u64,
    r#type: &str,
    line: &[u8],
) -> Option<u64> {
    let id = run.params.run_id.as_str();
    if inner.events.is_none() {
        match run.dir.open_events() {
            Ok(events) => inner.events = Some(events),
            Err(e) => warning!("run {id}: its log could not be opened: {e}"),
        }
    }
    let events = inner.events.as_mut()?;
    inner.unsynced += 1;
    let sync = inner.unsynced >= UNSYNCED_MAX;
    match store::append(events, line, sync) {
        Ok(end) => {
            if sync {
                inner.unsynced = 0;
            }
            Some(end)
        }
        Err(e) => {
            warning!("run {id}: {type} (run_seq {run_seq}) was not logged: {e}");
            None
        }
    }
}

impl Run {
    /// Shows `inner`, the run as it is now, in `run.list`.
    fn show(&self, inner: &RunInner) {
        self.set_view(View::of(inner));
    }

    /// Shows `view` in `run.list`.
    fn set_view(&self, view: View) {
        *self.view.lock().unwrap_or_else(|p| p.into_inner()) = view;
    }

    /// Notes that `attempt` ended `state` (once: an end published again, as a failure, is
    /// noted again).
    fn ended_as(&self, attempt: u64, state: RunState) {
        self.ends.send_if_modified(|ends| {
            if ends.last() == Some(&(attempt, state)) {
                return false;
            }
            ends.push((attempt, state));
            if ends.len() > ENDS_KEPT {
                ends.remove(0);
            }
            true
        });
    }

    /// The run as `run.list` shows it.
    fn view(&self) -> View {
        self.view.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The run's lock. It may be held over a slow disk (a state event is fsynced under
    /// it), so when it is taken, the wait is blocking work: no tokio worker waits on it.
    fn inner(&self) -> MutexGuard<'_, RunInner> {
        match self.inner.try_lock() {
            Ok(inner) => inner,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                blocking(|| self.inner.lock().unwrap_or_else(|p| p.into_inner()))
            }
        }
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
            loud: self.loud,
            last_event: inner.last_state_event.clone(),
            boot: process::boot_id(),
            session_started: inner.session_started,
            calls: inner.calls.clone(),
            pinned_call_used: inner.pinned_call_used,
            auth: inner.auth.clone(),
            connectors_call: self.call.is_some(),
        }
    }
}

/// A `run.events` stream: the run's log on disk, read from where it was asked, then
/// followed while the run is live.
pub struct Replay {
    run: Arc<Run>,
    reader: store::Reader,
    from: u64,
    /// The last `run_seq` read, sent or not.
    through: u64,
    logged: watch::Receiver<u64>,
    states: watch::Receiver<RunState>,
}

impl Replay {
    /// The next lines to send, a few MiB at a time: none once it has caught up with the
    /// log.
    ///
    /// A batch of lines all before `from_run_seq` is read past, not returned empty: an empty
    /// answer always means the log has been read to its end.
    pub fn read(&mut self) -> Vec<bus::Line> {
        loop {
            let limit = self.run.logged_len.load(Ordering::SeqCst);
            let lines = match self.reader.read(limit) {
                Ok(lines) => lines,
                Err(e) => {
                    warning!(
                        "run {}: its log could not be read: {e}",
                        self.run.params.run_id
                    );
                    return Vec::new();
                }
            };
            if lines.is_empty() {
                return Vec::new();
            }
            let mut sent = Vec::new();
            for line in lines {
                let Some(run_seq) = serde_json::from_slice::<Value>(&line)
                    .ok()
                    .and_then(|event| event["data"]["run_seq"].as_u64())
                else {
                    continue;
                };
                self.through = self.through.max(run_seq);
                if run_seq >= self.from {
                    sent.push(bus::Line::from(line));
                }
            }
            if !sent.is_empty() {
                return sent;
            }
        }
    }

    /// Whether the stream is over: the run has no live process, and every event in its
    /// log has been read, or its log is gone (an erase).
    pub fn done(&self) -> bool {
        let inner = self.run.inner();
        !inner.state.is_live() && (self.through >= inner.logged || self.reader.gone())
    }

    /// Waits until the run logs more, or its state changes.
    pub async fn changed(&mut self) {
        tokio::select! {
            _ = self.logged.changed() => {}
            _ = self.states.changed() => {}
        }
    }

    /// The reply that ends the stream.
    pub fn reply(&self) -> RunEventsData {
        RunEventsData::new(self.run.params.run_id.clone(), self.through)
    }
}

/// Whether a run's `system/init` is the truth of the connectors: a `no_tools` run loads
/// none (`--strict-mcp-config`), so it says nothing of them.
fn reads_connectors(params: &RunStartParams) -> bool {
    params.mount != Mount::NoTools
}

/// A gated attempt's session files ([`Runner::session_kept`]).
#[derive(Debug, Default)]
struct Kept {
    /// Whether the attempt resumes the run's session (`--resume`).
    resumed: bool,
    /// Each file a try may write, with its length before the attempt's first try where
    /// it was a file then.
    files: Vec<(PathBuf, Option<u64>)>,
}

/// The Claude Code config directories whose sessions `space.erase` deletes:
/// `CLAUDE_CONFIG_DIR` from rlxd's environment, and the default `~/.claude`.
///
/// TODO(manager): rlx knows every account's config directory (Python's
/// `services/claude_accounts.py`); once the brain (#30) serves that list, take it from
/// there, so an erase reaches every account's sessions.
fn config_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(home) = std::env::home_dir() {
        let default = home.join(".claude");
        if !dirs.contains(&default) {
            dirs.push(default);
        }
    }
    dirs
}

/// How Claude Code ended, for `run.failed`'s message: as its guard wrote it to its pipe
/// (macOS and Linux), else as the exit status of rlxd's child says.
fn how_it_ended(status: Option<std::process::ExitStatus>, tree: &process::Tree) -> String {
    #[cfg(unix)]
    {
        if let Some(ended) = tree.ended_how() {
            return format!("exited ({ended})");
        }
        if let Some(status) = status {
            // The guard ended before Claude Code did, and took the run's group with it.
            return format!("was ended with its guard ({status})");
        }
    }
    #[cfg(not(unix))]
    let _ = tree;
    match status {
        Some(status) => format!("exited ({status})"),
        None => "closed its output".to_owned(),
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
    done.loud = run.loud;
    match needs_you(&run.params, &done) {
        Some(mut needs) => {
            needs.loud = run.loud;
            End::NeedsYou(needs, done)
        }
        None => End::Done(done),
    }
}

/// Whether a finished run needs the user, as its `needs_you_when` says: its output,
/// `structured_output` or else the result's text read as JSON, holds `equals` at
/// `pointer`. The question is the text at `question_pointer`.
fn needs_you(params: &RunStartParams, done: &RunDoneData) -> Option<RunNeedsYouData> {
    let when = params.needs_you_when.as_ref()?;
    let parsed;
    let output = match &done.structured_output {
        Some(output) => output,
        None => {
            parsed = serde_json::from_str::<Value>(done.result.trim()).ok()?;
            &parsed
        }
    };
    if output.pointer(&when.pointer) != Some(&when.equals) {
        return None;
    }
    let question = when
        .question_pointer
        .as_deref()
        .and_then(|pointer| output.pointer(pointer))
        .map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        });
    Some(RunNeedsYouData::question(question))
}

/// The environment a run's Claude Code gets beyond rlxd's own, which it inherits. The
/// app's `PATH`, `CLAUDE_CONFIG_DIR` and `CLAUDE_SECURESTORAGE_CONFIG_DIR` (4.3) plug in
/// here; until then there is nothing to add.
pub(crate) fn run_env() -> Vec<(OsString, OsString)> {
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

/// Test builds only: with `RLXD_TEST_HOOK` set, rlxd itself (`rlxd`) as each run's hook,
/// run as `rlxd __test-hook`.
#[cfg(feature = "debug-methods")]
fn test_hook(rlxd: Option<&std::path::Path>) -> Option<(PathBuf, &'static str)> {
    std::env::var_os("RLXD_TEST_HOOK")
        .filter(|on| !on.is_empty())
        .and(rlxd.map(|rlxd| (rlxd.to_path_buf(), "__test-hook")))
}

#[cfg(not(feature = "debug-methods"))]
fn test_hook(_rlxd: Option<&std::path::Path>) -> Option<(PathBuf, &'static str)> {
    None
}

/// Test builds only: the space the variable `name` names.
#[cfg(feature = "debug-methods")]
fn test_space(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|space| !space.is_empty())
}

#[cfg(not(feature = "debug-methods"))]
fn test_space(_name: &str) -> Option<String> {
    None
}

/// Test builds only: `<name>=<space>:<ms>`, a space and how long something of it waits.
#[cfg(feature = "debug-methods")]
fn test_slow_in(name: &str) -> Option<(String, Duration)> {
    let value = std::env::var(name).ok()?;
    let (space, ms) = value.split_once(':')?;
    Some((space.to_owned(), Duration::from_millis(ms.parse().ok()?)))
}

#[cfg(not(feature = "debug-methods"))]
fn test_slow_in(_name: &str) -> Option<(String, Duration)> {
    None
}

/// Waits as `slow` says when `space` is its space: a test build's slow disk.
fn wait_if(slow: Option<&(String, Duration)>, space: &str) {
    if let Some((slowed, delay)) = slow
        && slowed == space
    {
        std::thread::sleep(*delay);
    }
}

/// `error`, with `detail` when it takes it.
fn with_detail(error: ErrorBody, detail: Value) -> ErrorBody {
    match detail {
        Value::Object(detail) => error.clone().with_detail(detail).unwrap_or(error),
        _ => error,
    }
}

/// The project folder Claude Code keeps the sessions of a working directory in:
/// `path` with each character but an ASCII letter or digit as `-` (one per UTF-16 unit,
/// as Claude Code counts them).
pub(crate) fn project_folder(path: &std::path::Path) -> String {
    let mut folder = String::new();
    for c in path.to_string_lossy().chars() {
        if c.is_ascii_alphanumeric() {
            folder.push(c);
        } else {
            for _ in 0..c.len_utf16() {
                folder.push('-');
            }
        }
    }
    folder
}

/// What `run.start` refuses of its params before it looks at anything else, with its own
/// codes: `kind_mount_mismatch` and `pinned_call_required`. A `needs_connectors` passes:
/// the gate (5.5) serves it at each start. The scheduler checks a run job's template with it at
/// `job.put`, so a template `run.start` would refuse is refused then, not at each fire.
pub fn check_start(params: &RunStartParams) -> Result<(), ErrorBody> {
    if params.kind.needs_no_tools() && params.mount != Mount::NoTools {
        return Err(refusal(
            codes::KIND_MOUNT_MISMATCH,
            format!(
                "a {} run takes only mount {{\"kind\": \"no_tools\"}}",
                params.kind
            ),
        ));
    }
    if !params.needs_connectors.is_empty() && params.mount == Mount::NoTools {
        return Err(refusal(
            codes::KIND_MOUNT_MISMATCH,
            "a run with needs_connectors takes a mount that loads them: mount \
             {\"kind\": \"no_tools\"} loads none"
                .to_owned(),
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
    Ok(())
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
