//! `connectors.call` (handoff 0.10.3, 5.3; spike #5, route a): one read call of a
//! connector's tool through Claude Code's own MCP client, with no model turn.
//!
//! The call is a run like any other ([`Runner::start_call`]): kind `watch`, mounting the one
//! connector, pinned to the call (its tool, and its input's RFC 8785 SHA-256), with its
//! record, its events, its ledger and its pool. Only its attempt differs ([`Runner::
//! call_attempt`]): Claude Code is started on a control channel ([`argv::call_args`]), and
//! no user line is ever written to it, so it makes no model turn.
//!
//! - **`initialize`** first. Unless its reply's `capabilities` lists
//!   [`CHECK_PERMISSIONS`], the run fails (`error`, naming it) and the call is `no_route`:
//!   an older Claude Code ignores `check_permissions` and runs the call with no check.
//! - **`mcp_status`**, polled until the connector is `connected` (`needs-auth` or `failed`
//!   end the run `failed {reason: "connectors"}`, as does its wait running out). Its rows
//!   are what the gate's rules decide by: each tool, named as Claude Code names it, and its
//!   read-only hint, which `mcp_status` gives as `readOnly: true` or nothing.
//! - **One `mcp_call`**, always with `check_permissions: true` (without it, Claude Code
//!   runs the call past every check). Claude Code asks `can_use_tool`, which goes to the
//!   gate's own path ([`Runner::decide_call`]): rlxd's rules, then the app, then the
//!   ledger. An allow answers with `updatedInput` = the pinned input, always, never what
//!   the request carried; a deny with its reason. A call that comes back with no
//!   `can_use_tool` first is a failure, and its content is dropped.
//! - **Its result** goes to the ledger (`ok` or `error`), to `run.tool_result`, and for a
//!   watch job to the scheduler; the run ends `run.done {num_turns: 0}`, or `run.failed
//!   {reason: "denied"}` on a deny.
//!
//! Everything read is bounded: each line ([`MAX_LINE`]), each wait (for `initialize`'s
//! reply, the connector, the `can_use_tool`, the result: [`Times`]), and the run as a
//! whole (its `timeout_s`, their sum and the app's `deadline_ms`). Each write must be taken
//! within its wait too. Any of them running out, a line too long, or anything Claude Code
//! does that a no-turn call never does (a model turn, a garbled reply), kills the process
//! tree.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rlxd_proto::{
    ConnectorStatus, ConnectorsCallData, ConnectorsCallParams, DecidedBy, Decision, ErrorBody,
    FailReason, McpServer, Mount, PinnedCall, RunDoneData, RunFailedData, RunId, RunKind, RunRef,
    RunStartParams, RunStartedData, RunState, RunToolResultData, codes, run_types,
};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufRead, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::time::Instant;

use super::connector_gate::{Gate, Verdict};
use super::gate::{self, Call, Via};
use super::{
    End, JobTag, Run, RunNote, Runner, argv, blocking, check_start, ended_early, jcs, process,
    refusal, run_env, store, stream, tail, with_detail,
};
use crate::log::{info, warning};
use crate::{claude, spaces};

/// The `initialize` capability without which a Claude Code runs `mcp_call` unchecked.
pub const CHECK_PERMISSIONS: &str = "mcp_call_check_permissions_v1";

/// The longest line rlxd reads of a call's control channel. A tool's content is kept up
/// to [`stream::MAX_CONTENT`]; a reply four times that is refused as it comes.
pub const MAX_LINE: usize = 4 << 20;

/// How often `mcp_status` is asked while the connector connects.
const STATUS_EVERY: Duration = Duration::from_millis(250);

/// How long Claude Code has to exit once its stdin is closed after the result.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// The run's prompt, which is never sent: no user line is ever written.
const PROMPT: &str = "connectors.call: one call, with no model turn";

/// The request ids rlxd writes.
const INIT_ID: &str = "rlxd-initialize";
const CALL_ID: &str = "rlxd-mcp-call";

/// How long each step of a call may take.
#[derive(Clone, Copy, Debug)]
pub struct Times {
    /// For `initialize`'s reply (the spike saw 0.17 s).
    pub init: Duration,
    /// For the connector to be `connected` (the spike saw about 8 s).
    pub connect: Duration,
    /// For the `can_use_tool`, once the call is sent (the spike saw 2.3 s).
    pub ask: Duration,
    /// For the call's result, once it is allowed.
    pub result: Duration,
}

impl Times {
    /// The times: test builds take every one from `RLXD_TEST_CALL_WAIT_MS`.
    pub fn get() -> Self {
        match test_wait() {
            Some(wait) => Self {
                init: wait,
                connect: wait,
                ask: wait,
                result: wait,
            },
            None => Self {
                init: Duration::from_secs(15),
                connect: Duration::from_secs(30),
                ask: Duration::from_secs(30),
                result: Duration::from_secs(60),
            },
        }
    }

    /// The run's `timeout_s`: every step's time and the app's `deadline_ms`, and a
    /// little more, so a step's own wait, not the run's, says which step ran out.
    fn timeout_s(&self, deadline_ms: u64) -> u64 {
        let steps = self.init + self.connect + self.ask + self.result;
        steps.as_secs() + deadline_ms.div_ceil(1000) + 5
    }
}

/// A `connectors.call` run's own facts.
pub(super) struct CallRun {
    /// The pinned input: what the call is made with, and what an allow answers.
    input: Map<String, Value>,
    /// How the call came out, once it has.
    outcome: Mutex<Option<Outcome>>,
}

impl CallRun {
    /// A call run rlxd read back from its record: it never runs again.
    pub(super) fn ended() -> Self {
        Self {
            input: Map::new(),
            outcome: Mutex::new(None),
        }
    }

    fn set(&self, outcome: Outcome) {
        *self.outcome.lock().unwrap_or_else(|p| p.into_inner()) = Some(outcome);
    }

    fn outcome(&self) -> Option<Outcome> {
        self.outcome
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// How a call came out.
#[derive(Clone, Debug)]
enum Outcome {
    /// It ran: its content, bounded, and whether the tool said it failed.
    Ran { content: Value, is_error: bool },
    /// The gate denied it.
    Denied {
        call_id: String,
        by: DecidedBy,
        reason: String,
    },
    /// This Claude Code is no route to a call with no model turn: why.
    NoRoute(String),
}

/// What driving the control channel came to.
enum Driven {
    /// The call ran.
    Ran,
    /// The gate denied it.
    Denied,
    /// No route: why.
    NoRoute(String),
    /// The connector was not connected.
    Connectors(Vec<ConnectorStatus>),
    /// rlxd ended the run, for this reason.
    Ended(FailReason),
    /// It could not go on: why. The tree is killed.
    Broken(String),
}

impl Runner {
    /// Whether this rlxd serves `connectors.call` (capability `connectors.call`).
    pub fn calls_connectors(&self) -> bool {
        self.connectors.call_route()
    }

    /// Starts the `connectors.call` run `params` asks for, fired by `job` when a scheduler
    /// job fired it: recorded, and started or queued, as `run.start` does. Refused as
    /// `run.start` refuses, and `invalid_params` for a tool that is not the connector's.
    pub fn start_call(
        self: &Arc<Self>,
        params: ConnectorsCallParams,
        client: &str,
        job: Option<JobTag>,
    ) -> Result<RunRef, ErrorBody> {
        let prefix = crate::connectors::tool_prefix(&params.connector);
        if params
            .tool
            .strip_prefix(&prefix)
            .is_none_or(|name| name.is_empty())
        {
            return Err(refusal(
                codes::INVALID_PARAMS,
                format!(
                    "tool: {:?} is not a tool of the connector {:?}: its tools are named \
                     {prefix}<tool>",
                    params.tool, params.connector
                ),
            ));
        }
        let input = Value::Object(params.input.clone());
        let mut start = RunStartParams::new(params.run_id, RunKind::Watch, PROMPT);
        start.mount = Mount::Connectors {
            connectors: vec![params.connector],
        };
        start.pinned_call = Some(PinnedCall {
            tool: params.tool,
            input_sha256: jcs::sha256(&input),
        });
        start.persist_session = Some(false);
        start.timeout_s = Some(Times::get().timeout_s(start.deadline_ms));
        check_start(&start)?;
        let (job_id, loud, watch_job) = match job {
            Some(job) => (Some(job.job_id), Some(job.loud), job.watch),
            None => (None, None, false),
        };
        let call = CallRun {
            input: params.input,
            outcome: Mutex::new(None),
        };
        self.start_checked(start, client, job_id, loud, watch_job, Some(call))
    }

    /// `connectors.call`: the call's run, started and waited for, and its reply. A deny is
    /// `denied` (`detail.by`, `detail.reason`, `detail.call_id`); a Claude Code that is no
    /// route `no_route`; any other end `call_failed` (`detail.reason`, the run's).
    pub async fn connectors_call(
        self: &Arc<Self>,
        params: ConnectorsCallParams,
        client: &str,
    ) -> Result<ConnectorsCallData, ErrorBody> {
        let run_id = params.run_id.clone();
        let started = {
            let runner = Arc::clone(self);
            let client = client.to_owned();
            blocking(move || runner.start_call(params, &client, None))
        };
        started?;
        let run = self.find(&run_id)?;
        let mut ends = run.ends.subscribe();
        let state = loop {
            if let Some((_, state)) = ends.borrow_and_update().last().copied() {
                break state;
            }
            if ends.changed().await.is_err() {
                break run.view().state;
            }
        };
        let outcome = run.call.as_ref().and_then(CallRun::outcome);
        reply(&run, &run_id, state, outcome)
    }

    /// A `connectors.call` run's attempt: Claude Code on a control channel, one gated
    /// call, and its end (the module's doc).
    pub(super) async fn call_attempt(self: &Arc<Self>, run: &Arc<Run>) -> End {
        let failed = |message: String| {
            End::Failed(RunFailedData::new(
                FailReason::Error,
                message,
                String::new(),
            ))
        };
        let Some(call) = run.call.as_ref() else {
            return failed("the run is not a connectors.call".to_owned());
        };
        let params = &run.params;
        let Some(program) = claude::resolve().and_then(|p| std::path::absolute(p).ok()) else {
            return failed(format!(
                "Claude Code was not found: there is no `claude` on rlxd's PATH, and {} \
                 names none",
                claude::BIN_ENV
            ));
        };
        let space = match blocking(|| spaces::ensure(&self.home, &params.space)) {
            Ok(space) => space,
            Err(e) => return failed(format!("the run could not be set up: {e}")),
        };
        let ending = run.inner().ending;
        if let Some(reason) = ending {
            return ended_early(run, reason);
        }
        let times = Times::get();
        let timeout_s = params
            .timeout_s
            .unwrap_or_else(|| times.timeout_s(params.deadline_ms));
        let deadline = Instant::now() + Duration::from_secs(timeout_s);
        let mut env = run_env();
        env.push((process::RUN_ID_ENV.into(), params.run_id.as_str().into()));
        {
            // No hook key: no hook decides anything of this run.
            let mut inner = run.inner();
            inner.hook_key = None;
            inner.listed = gate::Listed::default();
            inner.stopped = None;
        }
        let launch = process::Launch {
            program: program.clone(),
            args: argv::call_args(),
            cwd: space.clone(),
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
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            tree.kill();
            return failed("Claude Code's stdio was not piped".to_owned());
        };
        let stderr = tokio::spawn(tail(stderr));
        let mut channel = Channel {
            run,
            stdin: &mut stdin,
            stdout: BufReader::new(stdout),
            line: Vec::new(),
            deadline,
        };
        let driven = self.drive(run, call, &mut channel, &space, times).await;
        drop(channel);
        match &driven {
            Driven::Ran | Driven::Denied | Driven::NoRoute(_) | Driven::Connectors(_) => {
                // Done with it: closing stdin ends it, and what is left of the tree is
                // killed after the grace.
                let _ = stdin.shutdown().await;
                drop(stdin);
                if tree.reap(&mut child, Some(EXIT_GRACE)).await.is_none() {
                    tree.kill();
                    tree.reap(&mut child, None).await;
                }
            }
            Driven::Ended(_) | Driven::Broken(_) => {
                tree.kill();
                drop(stdin);
                tree.reap(&mut child, None).await;
            }
        }
        let said = if matches!(driven, Driven::Broken(_)) {
            stderr.await.unwrap_or_default()
        } else {
            stderr.abort();
            String::new()
        };
        if let (Driven::NoRoute(_), Some(job_id)) = (&driven, &run.job_id) {
            // The scheduler makes this fire again by the fallback (5.3).
            self.note(RunNote::NoRoute {
                job_id: job_id.clone(),
                run_id: params.run_id.clone(),
            });
        }
        let end = match driven {
            Driven::Ran => {
                let mut done = RunDoneData::new(None, "", rlxd_proto::Usage::default());
                done.loud = run.loud;
                End::Done(done)
            }
            Driven::Denied => {
                let reason = match call.outcome() {
                    Some(Outcome::Denied { by, reason, .. }) => {
                        format!("the call was denied by {}: {reason}", by.as_str())
                    }
                    _ => "the call was denied".to_owned(),
                };
                End::Failed(RunFailedData::new(
                    FailReason::Denied,
                    reason,
                    String::new(),
                ))
            }
            Driven::NoRoute(message) => {
                call.set(Outcome::NoRoute(message.clone()));
                failed(message)
            }
            Driven::Connectors(connectors) => End::Failed(
                RunFailedData::new(
                    FailReason::Connectors,
                    super::connector_gate::failure_message(&connectors, 1),
                    String::new(),
                )
                .with_connectors(connectors),
            ),
            Driven::Ended(reason) => ended_early(run, reason),
            Driven::Broken(mut message) => {
                if !said.trim().is_empty() {
                    message.push_str("; Claude Code said: ");
                    message.push_str(said.trim());
                }
                failed(message)
            }
        };
        let ending = run.inner().ending;
        match ending {
            // rlxd ended the run as the call came out: rlxd's reason stands.
            Some(reason) if !matches!(&end, End::Failed(f) if f.reason == reason) => {
                ended_early(run, reason)
            }
            _ => end,
        }
    }

    /// The control channel, from `initialize` to the call's result.
    async fn drive<R: AsyncBufRead + Unpin>(
        self: &Arc<Self>,
        run: &Arc<Run>,
        call: &CallRun,
        channel: &mut Channel<'_, R>,
        space: &std::path::Path,
        times: Times,
    ) -> Driven {
        let Some(pinned) = run.params.pinned_call.clone() else {
            return Driven::Broken("the call's run has no pinned call".to_owned());
        };
        let Mount::Connectors { connectors } = &run.params.mount else {
            return Driven::Broken("the call's run mounts no connector".to_owned());
        };
        // `initialize`, and its capabilities.
        let until = Instant::now() + times.init;
        let init = json!({"type": "control_request", "request_id": INIT_ID,
            "request": {"subtype": "initialize"}});
        if let Err(driven) = channel.write(&init, until, "initialize").await {
            return driven;
        }
        let reply = match channel.reply_to(INIT_ID, until, "answer initialize").await {
            Ok(reply) => reply,
            Err(driven) => return driven,
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                return Driven::Broken(format!("Claude Code refused initialize: {error}"));
            }
        };
        if !lists(&reply["capabilities"], CHECK_PERMISSIONS) {
            return Driven::NoRoute(format!(
                "this Claude Code's initialize does not list {CHECK_PERMISSIONS}, so it would \
                 make the call with no permission check: rlxd makes no call through it"
            ));
        }
        // The connector, connected: what the gate's rules decide by.
        let named = Gate {
            named: connectors.clone(),
            route_1: true,
            wait: times.connect,
            settle: Duration::ZERO,
        };
        let until = Instant::now() + times.connect;
        let mut asked = 0_u32;
        let mut last = named.none_seen();
        let (listed, servers) = loop {
            asked += 1;
            let id = format!("rlxd-mcp-status-{asked}");
            let status = json!({"type": "control_request", "request_id": id,
                "request": {"subtype": "mcp_status"}});
            if let Err(driven) = channel.write(&status, until, "take mcp_status").await {
                return match driven {
                    Driven::Broken(_) if Instant::now() >= until => Driven::Connectors(last),
                    driven => driven,
                };
            }
            let reply = match channel.reply_to(&id, until, "answer mcp_status").await {
                Ok(Ok(reply)) => reply,
                Ok(Err(error)) => {
                    return Driven::Broken(format!("Claude Code refused mcp_status: {error}"));
                }
                Err(Driven::Broken(_)) if Instant::now() >= until => {
                    return Driven::Connectors(last);
                }
                Err(driven) => return driven,
            };
            let rows = reply["mcpServers"].as_array().cloned().unwrap_or_default();
            let servers: Vec<McpServer> = rows
                .iter()
                .filter_map(|row| {
                    Some(McpServer::new(
                        row["name"].as_str()?,
                        row["status"].as_str()?,
                    ))
                })
                .collect();
            match named.verdict(&servers) {
                Verdict::Connected => break (listed_of(&rows), servers),
                Verdict::Stopped(connectors) => return Driven::Connectors(connectors),
                Verdict::Missed(connectors) => last = connectors,
            }
            let next = Instant::now() + STATUS_EVERY;
            if next >= until {
                return Driven::Connectors(last);
            }
            tokio::time::sleep_until(next).await;
            let ending = run.inner().ending;
            if let Some(reason) = ending {
                return Driven::Ended(reason);
            }
        };
        {
            let mut inner = run.inner();
            let mut tools: Vec<String> = listed.tools.iter().cloned().collect();
            tools.sort();
            inner.listed = listed;
            let data =
                RunStartedData::new(space.display().to_string(), "", "").with_tools(tools, servers);
            self.emit(run, &mut inner, run_types::STARTED, &data);
        }
        // The one call, always with check_permissions.
        let input = Value::Object(call.input.clone());
        let mcp_call = json!({"type": "control_request", "request_id": CALL_ID,
            "request": {"subtype": "mcp_call", "tool": pinned.tool, "arguments": input,
                "check_permissions": true, "result_not_for_model": true,
                "soft_error_result": true}});
        let mut until = Instant::now() + times.ask;
        if let Err(driven) = channel.write(&mcp_call, until, "take the call").await {
            return driven;
        }
        // The call allowed, by its call id; or the gate's deny.
        let mut allowed: Option<String> = None;
        let mut denied = false;
        loop {
            let what = if allowed.is_some() || denied {
                "give the call's result"
            } else {
                "ask can_use_tool"
            };
            let message = match channel.next(until, what).await {
                Ok(message) => message,
                Err(driven) => return driven,
            };
            match Message::of(&message) {
                Message::Asked {
                    request_id,
                    request,
                } if request["subtype"] == "can_use_tool" => {
                    let taken = match self.ask_gate(run, channel, &request).await {
                        Ok(taken) => taken,
                        Err(driven) => return driven,
                    };
                    let answer = match &taken {
                        Some((call_id, taken)) if taken.decision == Decision::Allow => {
                            allowed = Some(call_id.clone());
                            // The pinned input, always: never what the request carried.
                            json!({"behavior": "allow", "updatedInput": input})
                        }
                        Some((call_id, taken)) => {
                            if allowed.is_none() && !denied {
                                denied = true;
                                call.set(Outcome::Denied {
                                    call_id: call_id.clone(),
                                    by: taken.by,
                                    reason: taken.reason.clone(),
                                });
                            }
                            json!({"behavior": "deny", "message": taken.reason})
                        }
                        None => json!({"behavior": "deny",
                            "message": "rlxd could not read the request"}),
                    };
                    let response = json!({"type": "control_response", "response": {
                        "subtype": "success", "request_id": request_id, "response": answer}});
                    if let Err(driven) = channel.write(&response, until, "take the answer").await {
                        return driven;
                    }
                    if allowed.is_some() {
                        until = Instant::now() + times.result;
                    }
                }
                Message::Asked {
                    request_id,
                    request,
                } => {
                    // Nothing else is rlxd's to answer on a call's channel.
                    let subtype = request["subtype"].as_str().unwrap_or("?");
                    let response = json!({"type": "control_response", "response": {
                        "subtype": "error", "request_id": request_id,
                        "error": format!("rlxd answers no {subtype} on a connectors.call")}});
                    if let Err(driven) = channel.write(&response, until, "take an answer").await {
                        return driven;
                    }
                }
                Message::Replied { request_id, result } if request_id == CALL_ID => {
                    return self.came_out(run, call, allowed, denied, result);
                }
                Message::Replied { .. } | Message::Other => {}
                Message::Turn(kind) => {
                    return Driven::Broken(format!(
                        "Claude Code wrote {kind:?}, a model turn's, which a connectors.call \
                         never makes"
                    ));
                }
            }
        }
    }

    /// Hands a `can_use_tool` request to the gate: the call's id and the decision taken,
    /// or `None` for a request that names no tool or no call id (denied unread).
    async fn ask_gate<R: AsyncBufRead + Unpin>(
        self: &Arc<Self>,
        run: &Arc<Run>,
        channel: &Channel<'_, R>,
        request: &Value,
    ) -> Result<Option<(String, gate::Taken)>, Driven> {
        let (Some(tool), Some(id)) = (
            request["tool_name"].as_str().filter(|s| !s.is_empty()),
            request["tool_use_id"].as_str().filter(|s| !s.is_empty()),
        ) else {
            return Ok(None);
        };
        // The input as Claude Code asks it: the gate checks it against the pin. What
        // runs is the pinned input all the same.
        let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
        let call = Call::new(id.to_owned(), tool.to_owned(), input);
        let deciding = self.decide_call(run, call, Via::Control);
        tokio::select! {
            taken = deciding => Ok(Some((id.to_owned(), taken.unwrap_or_else(|| gate::Taken {
                decision: Decision::Deny,
                by: DecidedBy::Rlxd,
                reason: "the call was not decided".to_owned(),
            })))),
            reason = ending(run) => Err(Driven::Ended(reason)),
            () = tokio::time::sleep_until(channel.deadline) => {
                Err(Driven::Ended(timed_out(run)))
            }
        }
    }

    /// The call's result, `result`: into the ledger, the stream and the scheduler.
    fn came_out(
        &self,
        run: &Run,
        call: &CallRun,
        allowed: Option<String>,
        denied: bool,
        result: Result<Value, String>,
    ) -> Driven {
        let Some(call_id) = allowed else {
            return match result {
                Ok(_) if denied => {
                    warning!(
                        "run {}: Claude Code made the call the gate denied: its content is \
                         dropped",
                        run.params.run_id
                    );
                    Driven::Broken(
                        "Claude Code made the call rlxd's gate denied: its content is dropped"
                            .to_owned(),
                    )
                }
                Err(_) if denied => Driven::Denied,
                Ok(_) => {
                    warning!(
                        "run {}: Claude Code made the call with no can_use_tool: its content \
                         is dropped",
                        run.params.run_id
                    );
                    Driven::Broken(
                        "Claude Code made the call without asking rlxd's gate (it did not \
                         honour check_permissions): its content is dropped"
                            .to_owned(),
                    )
                }
                Err(error) => Driven::Broken(format!("Claude Code did not make the call: {error}")),
            };
        };
        let tool = run
            .params
            .pinned_call
            .as_ref()
            .map(|pinned| pinned.tool.clone())
            .unwrap_or_default();
        let mut inner = run.inner();
        let (content, is_error, failure) = match result {
            Ok(result) => {
                let is_error = result["isError"] == true;
                (
                    result.get("content").cloned().unwrap_or(Value::Null),
                    is_error,
                    None,
                )
            }
            Err(error) => (Value::String(error.clone()), true, Some(error)),
        };
        let ran = self.tool_ran(run, &mut inner, &call_id, is_error);
        let (content, truncated) = stream::bounded(content);
        if let (Some(ran), Some(job_id)) = (ran, &run.job_id)
            && run.watch_job
            && failure.is_none()
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
        let data =
            RunToolResultData::new(call_id, tool, is_error, content.clone()).truncated(truncated);
        self.emit(run, &mut inner, run_types::TOOL_RESULT, &data);
        drop(inner);
        if let Some(error) = failure {
            return Driven::Broken(format!("the call failed: {error}"));
        }
        call.set(Outcome::Ran { content, is_error });
        info!(
            "run {}: the call ran, with no model turn",
            run.params.run_id
        );
        Driven::Ran
    }
}

/// `connectors.call`'s reply, from how its run ended.
fn reply(
    run: &Run,
    run_id: &RunId,
    state: RunState,
    outcome: Option<Outcome>,
) -> Result<ConnectorsCallData, ErrorBody> {
    match (state, outcome) {
        (RunState::Done, Some(Outcome::Ran { content, is_error })) => Ok(ConnectorsCallData::new(
            run_id.clone(),
            Decision::Allow,
            Some(content),
            Some(is_error),
        )),
        (
            _,
            Some(Outcome::Denied {
                call_id,
                by,
                reason,
            }),
        ) => Err(with_detail(
            refusal(
                codes::DENIED,
                format!("the call was denied by {}: {reason}", by.as_str()),
            ),
            json!({"run_id": run_id, "call_id": call_id, "by": by.as_str(), "reason": reason}),
        )),
        (_, Some(Outcome::NoRoute(message))) => Err(with_detail(
            refusal(codes::NO_ROUTE, message),
            json!({"run_id": run_id}),
        )),
        _ => {
            let last = run.inner().last_state_event.clone().unwrap_or_default();
            let reason = last["data"]["reason"]
                .as_str()
                .unwrap_or("error")
                .to_owned();
            let message = last["data"]["message"]
                .as_str()
                .unwrap_or("the call's run did not end done")
                .to_owned();
            Err(with_detail(
                refusal(codes::CALL_FAILED, message),
                json!({"run_id": run_id, "reason": reason}),
            ))
        }
    }
}

/// The gate's listing of `mcp_status`'s `rows`: each tool as Claude Code names it
/// (`mcp__<server>__<tool>`), each server with its source, and each tool's read-only hint
/// where it is `readOnly: true` (`mcp_status` gives no other value: absent is a write).
fn listed_of(rows: &[Value]) -> gate::Listed {
    let mut listed = gate::Listed::default();
    for row in rows {
        let Some(name) = row["name"].as_str() else {
            continue;
        };
        listed.servers.push(gate::Server {
            name: name.to_owned(),
            source: row["source"].as_str().map(str::to_owned),
        });
        if row["status"] != "connected" {
            continue;
        }
        let server = gate::claude_server_name(name);
        for tool in row["tools"].as_array().into_iter().flatten() {
            let Some(tool_name) = tool["name"].as_str().filter(|n| !n.is_empty()) else {
                continue;
            };
            let full = format!("mcp__{server}__{tool_name}");
            if tool["annotations"]["readOnly"] == true {
                listed.hints.insert(full.clone(), true);
            }
            listed.tools.insert(full);
        }
    }
    listed
}

/// Whether `capabilities` (a list of names, or an object keyed by them) names `name`.
fn lists(capabilities: &Value, name: &str) -> bool {
    match capabilities {
        Value::Array(names) => names.iter().any(|each| each == name),
        Value::Object(names) => names.get(name).is_some_and(|v| v != &Value::Bool(false)),
        _ => false,
    }
}

/// One message of the control channel, as the driver reads it.
enum Message {
    /// A `control_request` of Claude Code's: its id and its `request`.
    Asked { request_id: Value, request: Value },
    /// A `control_response` to one of rlxd's: its id, and its `response` or its error.
    Replied {
        request_id: String,
        result: Result<Value, String>,
    },
    /// A line of a model turn (`assistant`, `user`, `result`, `stream_event`).
    Turn(String),
    /// Anything else, which is ignored.
    Other,
}

impl Message {
    fn of(message: &Value) -> Self {
        match message["type"].as_str() {
            Some("control_request") => match message.get("request_id") {
                Some(id) if !id.is_null() => Self::Asked {
                    request_id: id.clone(),
                    request: message["request"].clone(),
                },
                _ => Self::Other,
            },
            Some("control_response") => {
                let response = &message["response"];
                let Some(id) = response["request_id"].as_str() else {
                    return Self::Other;
                };
                let result = if response["subtype"] == "success" {
                    Ok(response.get("response").cloned().unwrap_or_default())
                } else {
                    Err(response["error"]
                        .as_str()
                        .unwrap_or("an error, unsaid")
                        .to_owned())
                };
                Self::Replied {
                    request_id: id.to_owned(),
                    result,
                }
            }
            Some(kind @ ("assistant" | "user" | "result" | "stream_event")) => {
                Self::Turn(kind.to_owned())
            }
            _ => Self::Other,
        }
    }
}

/// A call's control channel: Claude Code's stdin and stdout, every read and write bounded.
struct Channel<'a, R> {
    run: &'a Run,
    stdin: &'a mut ChildStdin,
    stdout: R,
    line: Vec<u8>,
    /// The run's own deadline: its `timeout_s`.
    deadline: Instant,
}

impl<R: AsyncBufRead + Unpin> Channel<'_, R> {
    /// Writes `message` as one line, taken by `until` (and the run's deadline).
    async fn write(&mut self, message: &Value, until: Instant, what: &str) -> Result<(), Driven> {
        let mut line = message.to_string();
        line.push('\n');
        let at = until.min(self.deadline);
        let written = tokio::time::timeout_at(at, async {
            self.stdin.write_all(line.as_bytes()).await?;
            self.stdin.flush().await
        })
        .await;
        match written {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(Driven::Broken(format!(
                "Claude Code did not {what}: its stdin could not be written: {e}"
            ))),
            Err(_) => Err(self.ran_out(until, what)),
        }
    }

    /// The next message, by `until` (and the run's deadline): a line that is not JSON is
    /// skipped.
    async fn next(&mut self, until: Instant, what: &str) -> Result<Value, Driven> {
        loop {
            let at = until.min(self.deadline);
            let read = tokio::select! {
                read = stream::next_line(&mut self.stdout, &mut self.line, MAX_LINE) => read,
                () = tokio::time::sleep_until(at) => return Err(self.ran_out(until, what)),
                reason = ending(self.run) => return Err(Driven::Ended(reason)),
            };
            match read {
                Ok(Some(())) => {}
                Ok(None) => {
                    return Err(Driven::Broken(format!(
                        "Claude Code's output ended before it would {what}"
                    )));
                }
                Err(stream::ReadError::TooLong) => {
                    return Err(Driven::Broken(format!(
                        "line_too_long: Claude Code wrote a control line longer than {MAX_LINE} \
                         bytes, which rlxd does not read"
                    )));
                }
                Err(stream::ReadError::Io(e)) => {
                    return Err(Driven::Broken(format!(
                        "Claude Code's output could not be read: {e}"
                    )));
                }
            }
            if let Ok(message) = serde_json::from_slice::<Value>(&self.line) {
                return Ok(message);
            }
        }
    }

    /// The reply to rlxd's request `id`, by `until`: Claude Code's requests meanwhile are
    /// refused, and a model turn's line ends it.
    async fn reply_to(
        &mut self,
        id: &str,
        until: Instant,
        what: &str,
    ) -> Result<Result<Value, String>, Driven> {
        loop {
            let message = self.next(until, what).await?;
            match Message::of(&message) {
                Message::Replied { request_id, result } if request_id == id => return Ok(result),
                Message::Asked {
                    request_id,
                    request,
                } => {
                    let subtype = request["subtype"].as_str().unwrap_or("?");
                    let response = json!({"type": "control_response", "response": {
                        "subtype": "error", "request_id": request_id,
                        "error": format!("rlxd answers no {subtype} here")}});
                    self.write(&response, until, what).await?;
                }
                Message::Turn(kind) => {
                    return Err(Driven::Broken(format!(
                        "Claude Code wrote {kind:?}, a model turn's, which a connectors.call \
                         never makes"
                    )));
                }
                Message::Replied { .. } | Message::Other => {}
            }
        }
    }

    /// What a wait that ran out by `until` is: the run's timeout when its deadline came
    /// first, else the step's own failure.
    fn ran_out(&self, until: Instant, what: &str) -> Driven {
        if self.deadline <= until && Instant::now() >= self.deadline {
            return Driven::Ended(timed_out(self.run));
        }
        Driven::Broken(format!("Claude Code did not {what} in time"))
    }
}

/// The run's timeout, noted as why rlxd ends it (a cancel noted first stands).
fn timed_out(run: &Run) -> FailReason {
    *run.inner().ending.get_or_insert(FailReason::Timeout)
}

/// Resolves once rlxd is ending `run` (a cancel, rlxd stopping): why.
async fn ending(run: &Run) -> FailReason {
    loop {
        let ending = run.inner().ending;
        if let Some(reason) = ending {
            return reason;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Test builds only: `RLXD_TEST_CALL_WAIT_MS` is every step's wait of a call.
#[cfg(feature = "debug-methods")]
fn test_wait() -> Option<Duration> {
    std::env::var("RLXD_TEST_CALL_WAIT_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map(Duration::from_millis)
}

#[cfg(not(feature = "debug-methods"))]
fn test_wait() -> Option<Duration> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_are_named_in_a_list_or_an_object() {
        assert!(lists(&json!(["a", CHECK_PERMISSIONS]), CHECK_PERMISSIONS));
        assert!(lists(&json!({CHECK_PERMISSIONS: true}), CHECK_PERMISSIONS));
        assert!(!lists(
            &json!({CHECK_PERMISSIONS: false}),
            CHECK_PERMISSIONS
        ));
        assert!(!lists(
            &json!(["mcp_call_result_not_for_model_v1"]),
            CHECK_PERMISSIONS
        ));
        assert!(!lists(&json!(CHECK_PERMISSIONS), CHECK_PERMISSIONS));
        assert!(!lists(&Value::Null, CHECK_PERMISSIONS));
    }

    #[test]
    fn mcp_status_rows_list_tools_as_claude_code_names_them() {
        let rows = json!([
            {"name": "claude.ai GitHub", "status": "connected", "source": "claudeai",
             "tools": [
                {"name": "get_check_runs", "annotations": {"readOnly": true}},
                {"name": "create_issue", "annotations": {"openWorld": true}},
                {"name": "delete_repo", "annotations": {"readOnly": false, "destructive": true}},
                {"name": "", "annotations": {"readOnly": true}},
             ]},
            {"name": "claude.ai Slack", "status": "pending", "source": "claudeai",
             "tools": [{"name": "search", "annotations": {"readOnly": true}}]},
            {"status": "connected"},
        ]);
        let listed = listed_of(rows.as_array().unwrap());
        let read = "mcp__claude_ai_GitHub__get_check_runs";
        let mut tools: Vec<&str> = listed.tools.iter().map(String::as_str).collect();
        tools.sort_unstable();
        assert_eq!(
            tools,
            [
                "mcp__claude_ai_GitHub__create_issue",
                "mcp__claude_ai_GitHub__delete_repo",
                read
            ]
        );
        assert_eq!(listed.hints.len(), 1);
        assert_eq!(gate::read_only(&listed.hints, read), Some(true));
        assert_eq!(
            gate::read_only(&listed.hints, "mcp__claude_ai_GitHub__create_issue"),
            None
        );
        assert_eq!(listed.servers.len(), 2);
        assert_eq!(listed.servers[0].source.as_deref(), Some("claudeai"));
    }

    #[test]
    fn a_runs_timeout_covers_every_step_and_the_apps_deadline() {
        let times = Times {
            init: Duration::from_secs(15),
            connect: Duration::from_secs(30),
            ask: Duration::from_secs(30),
            result: Duration::from_secs(60),
        };
        assert_eq!(times.timeout_s(120_000), 135 + 120 + 5);
        assert_eq!(times.timeout_s(1), 135 + 1 + 5);
    }

    #[test]
    fn messages_are_read_by_their_type() {
        let asked = json!({"type": "control_request", "request_id": "r1",
            "request": {"subtype": "can_use_tool"}});
        assert!(matches!(Message::of(&asked), Message::Asked { .. }));
        let replied = json!({"type": "control_response", "response": {"subtype": "error",
            "request_id": CALL_ID, "error": "The tool did not run: no"}});
        match Message::of(&replied) {
            Message::Replied { request_id, result } => {
                assert_eq!(request_id, CALL_ID);
                assert_eq!(result, Err("The tool did not run: no".to_owned()));
            }
            _ => panic!("a reply"),
        }
        assert!(matches!(
            Message::of(&json!({"type": "assistant"})),
            Message::Turn(_)
        ));
        assert!(matches!(
            Message::of(&json!({"type": "keep_alive"})),
            Message::Other
        ));
        // A request with no id can't be answered: it is not one.
        assert!(matches!(
            Message::of(&json!({"type": "control_request", "request": {}})),
            Message::Other
        ));
    }
}
