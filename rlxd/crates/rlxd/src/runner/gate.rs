//! The tool-call gate (handoff 0.10.3, 5.1): it decides every tool call of every run, and
//! fails closed.
//!
//! - **A hook request is bound to its run.** It must name the run's live attempt, carry
//!   that attempt's key (`RLXD_HOOK_KEY`, 256 random bits made at each attempt's start, in
//!   Claude Code's environment and so its hooks'), and come from a process inside the run
//!   ([`process::holds`]): on macOS and Linux a descendant of the run's guard by its parent
//!   chain, each step checked against a reused pid (Claude Code starts hooks detached, in a
//!   session of their own), on Windows a process in the run's Job Object, through the
//!   handle pinned when it connected. Any other request is denied at once, with no event
//!   and no ledger entry, so a guessed run id from outside the run reaches nothing. The
//!   key is inherited by every process Claude Code starts, not only its hooks, so a
//!   process inside the run can ask about a call of its own making: it gets the app's
//!   answer to that call only, never an allow for one of Claude Code's (each call is its
//!   own `call_id`, decided once), and it can't reach another run. The key is never
//!   logged, published or written to disk.
//! - **rlxd's own rules come first,** and deny `by: "rlxd"`: the mount, the kind and the
//!   pinned call ([`rlxd_rule`]). A tool's server is the one MCP server of the attempt's
//!   `system/init` whose name, as Claude Code writes it, the tool's name starts with
//!   ([`claude_server_name`]); none, or more than one, is a deny. A tool's read-only hint
//!   is [`read_only`]'s alone.
//! - **Every other call goes to the app** as `run.tool_call`, and only `run.decide` from an
//!   app-role connection answers it; the first answer wins. With no app connected it is
//!   denied `by: "unreachable"` at once. Pending when the last app leaves, it is held
//!   until its deadline, then denied `by: "timeout"`. Nothing a run writes (its text, a
//!   tool's result, a prompt) can decide a call: a reason is data only.
//! - **The ledger** in `run.json` holds every call: `decided`, and `executing` for an
//!   allow, written and fsynced before the hook gets its answer, and an allow whose entry
//!   can't be written is a deny `by: "rlxd"`; `ok` or `error` from the call's
//!   `tool_result`; `unknown` when the run ends with it still executing.
//!
//! A call reaches the gate one of two ways ([`Via`]): a hook request, bound as above, or
//! Claude Code's own `can_use_tool` on the control channel of a `connectors.call` run
//! ([`super::call`]), which is rlxd's own child's stdio and needs no binding. Both take
//! the same path from there ([`Runner::decide_call`]): rlxd's rules, the app, the ledger.
//!
//! Until rlxd-hook's own pre-tool-use is wired to [`Runner::pre_tool_use`] (with #617, at
//! rc/0.10.3's merge), only test builds reach the hook side, through `debug.pre_tool_use`:
//! the items marked so are unused in a release build until then.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rlxd_proto::{
    CallState, DecidedBy, Decision, ErrorBody, Mount, RunCall, RunDecideData, RunDecideParams,
    RunKind, RunStartParams, RunState, RunToolCallData, RunToolDecidedData, codes, run_types,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::{Run, RunInner, Runner, blocking, jcs, process, refusal};
use crate::log::{info, warning};

/// The reason of a pinned run's every call after its pinned call was allowed once.
pub const PINNED_CALL_USED: &str = "pinned_call_used";

/// The reason of a call to a tool the attempt's `system/init` didn't list.
pub const TOOL_NOT_IN_INIT: &str = "tool_not_in_init";

/// What an attempt's `system/init` listed (a `connectors.call` run's: its `mcp_status`),
/// which rlxd's rules decide by.
#[derive(Clone, Debug, Default)]
pub struct Listed {
    /// Its tools, by their exact names.
    pub tools: std::collections::HashSet<String>,
    /// Its MCP servers.
    pub servers: Vec<Server>,
    /// Each tool's `readOnlyHint`, where its entry carried one.
    pub hints: HashMap<String, bool>,
}

/// An MCP server an attempt's `system/init` listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    /// Its name, such as `claude.ai Slack`.
    pub name: String,
    /// Where it comes from, where init said: `claudeai` for a claude.ai connector.
    pub source: Option<String>,
}

/// A claude.ai connector's name begins so, as Claude Code names them.
const CLAUDE_AI: &str = "claude.ai ";

/// A claude.ai connector's `source` in `system/init`.
const CLAUDE_AI_SOURCE: &str = "claudeai";

/// Whether the mount's connector `connector` is `server`: the same name, and for a
/// claude.ai connector, a server whose source is claude.ai. A local, user, project or
/// plugin server named exactly like a claude.ai connector is never it.
fn is_mounted(connector: &str, server: &Server) -> bool {
    connector == server.name
        && (!connector.starts_with(CLAUDE_AI) || server.source.as_deref() == Some(CLAUDE_AI_SOURCE))
}

/// The name of the variable that holds an attempt's hook key.
pub const HOOK_KEY_ENV: &str = "RLXD_HOOK_KEY";

/// The reason of a call still open as its run ends.
const RUN_ENDED: &str = "the run ended before the call was decided";

/// The reason of an allow whose ledger entry could not be written: it is a deny.
const LEDGER_UNWRITTEN: &str = "rlxd could not write the call to the run's ledger";

/// The reason of a call made after the app ended the run on a denial (`stop: true`).
const STOPPED: &str = "the app stopped the run";

/// The reason of a call the app didn't answer before its deadline.
const TIMED_OUT: &str = "no answer came from the app before the call's deadline";

/// A hook request: what `rlxd-hook pre-tool-use` sends for one call.
#[cfg_attr(not(feature = "debug-methods"), allow(dead_code))]
pub struct Ask {
    pub run_id: String,
    pub attempt: u64,
    /// The attempt's key, as the hook got it in its environment.
    pub key: String,
    /// The connecting process, pinned when its connection was accepted.
    pub peer: Arc<crate::peer::Peer>,
    /// Claude Code's PreToolUse input: `tool_name`, `tool_input`, `tool_use_id`, ….
    pub stdin: Value,
}

/// Where a call the gate is deciding stands, by `call_id`.
pub(super) enum Status {
    /// Not decided yet: waiting for the app, whose answer goes to `answer`, or, `answer`
    /// taken, the app's answer on its way to the call.
    Open {
        tool: String,
        sha256: String,
        answer: Option<oneshot::Sender<AppAnswer>>,
    },
    /// Decided, by whom.
    Decided(DecidedBy),
}

/// The app's `run.decide`, handed to the call waiting for it.
pub(super) struct AppAnswer {
    decision: Decision,
    reason: String,
    draft_id: Option<String>,
    stop: bool,
    /// Told the decision taken once it is written: the app's, or rlxd's deny (a pinned
    /// call another call used meanwhile, a ledger that could not be written).
    written: oneshot::Sender<Decision>,
}

/// One call, as the gate reads it.
pub(super) struct Call {
    pub(super) id: String,
    pub(super) tool: String,
    pub(super) input: Value,
    pub(super) sha256: String,
}

impl Call {
    /// The call `id` of `tool` with `input`, as Claude Code asked it.
    pub(super) fn new(id: String, tool: String, input: Value) -> Self {
        let sha256 = jcs::sha256(&input);
        Self {
            id,
            tool,
            input,
            sha256,
        }
    }
}

/// Where a call came to the gate from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Via {
    /// A hook request, bound to the run's live attempt.
    Hook,
    /// Claude Code's `can_use_tool` on a `connectors.call` run's control channel.
    Control,
}

/// A decision as taken: who took it, and its reason.
pub(super) struct Taken {
    pub(super) decision: Decision,
    pub(super) by: DecidedBy,
    pub(super) reason: String,
}

impl Taken {
    /// A deny by `by`, for `reason`.
    fn deny(by: DecidedBy, reason: &str) -> Self {
        Self {
            decision: Decision::Deny,
            by,
            reason: reason.to_owned(),
        }
    }

    #[cfg_attr(not(feature = "debug-methods"), allow(dead_code))]
    fn output(&self) -> String {
        output(self.decision, &self.reason)
    }
}

/// What a waiting call got.
enum Came {
    Answer(AppAnswer),
    Deadline,
    /// Its answer's sender went without an answer: the run's end settled it.
    Gone,
}

impl Runner {
    /// Follows the app gate's connections: `status`'s `clients.app`, as it changes.
    pub fn follow_apps(&self, apps: tokio::sync::watch::Receiver<u32>) {
        if self.apps.set(apps).is_err() {
            warning!("runner: the app gate's connections are followed once");
        }
    }

    fn app_present(&self) -> bool {
        self.apps.get().is_some_and(|apps| *apps.borrow() > 0)
    }

    /// Ends every hook connection waiting on the gate, as a dropped connection would:
    /// each one's call is denied, and its hook gets no answer (test builds' `debug.
    /// drop_hook_connections`). How many there were.
    #[cfg(feature = "debug-methods")]
    pub fn drop_hooks(&self) -> u64 {
        let waiting = *self.hooks_waiting.borrow();
        self.hooks_dropped.send_modify(|n| *n += 1);
        waiting
    }

    /// Test builds only: resolves once `debug.drop_hook_connections` ends the hook
    /// connections waiting now. A release build's never does.
    #[cfg(feature = "debug-methods")]
    fn hooks_dropped(&self) -> impl std::future::Future<Output = ()> + use<> {
        let mut dropped = self.hooks_dropped.subscribe();
        async move {
            let _ = dropped.changed().await;
        }
    }

    #[cfg(not(feature = "debug-methods"))]
    fn hooks_dropped(&self) -> impl std::future::Future<Output = ()> + use<> {
        std::future::pending()
    }

    /// Test builds only: counts a hook connection as it starts waiting on the gate (`up`)
    /// or stops.
    #[cfg(feature = "debug-methods")]
    fn count_waiting(&self, up: bool) {
        self.hooks_waiting.send_modify(|n| {
            *n = if up { *n + 1 } else { n.saturating_sub(1) };
        });
    }

    #[cfg(not(feature = "debug-methods"))]
    fn count_waiting(&self, _up: bool) {}

    /// The gate's answer to one hook request: Claude Code's hook JSON, allow or deny.
    /// `None` when its connection was dropped (`debug.drop_hook_connections`): the hook
    /// then gets no answer, and denies.
    #[cfg_attr(not(feature = "debug-methods"), allow(dead_code))]
    pub async fn pre_tool_use(self: &Arc<Self>, ask: Ask) -> Option<String> {
        let (run, call) = match self.bind(&ask) {
            Ok(bound) => bound,
            Err(why) => {
                // As the caller sent it: quoted, so it can't forge a line of the log.
                warning!(
                    "gate: a hook request for run {:?} was refused: {why}",
                    ask.run_id
                );
                let reason = format!("rlxd refused the hook request: {why}");
                return Some(output(Decision::Deny, &reason));
            }
        };
        let taken = self.decide_call(&run, call, Via::Hook).await?;
        Some(taken.output())
    }

    /// The gate's decision on `call` of `run`, which came `via` a hook or the control
    /// channel: rlxd's rules at once, else the app's answer before the run's
    /// `deadline_ms`, written to the ledger before it is returned. `None` only for a hook
    /// whose connection was dropped (`debug.drop_hook_connections`): the call is denied
    /// all the same, and its hook gets no answer.
    pub(super) async fn decide_call(
        self: &Arc<Self>,
        run: &Arc<Run>,
        call: Call,
        via: Via,
    ) -> Option<Taken> {
        let deadline_ms = run.params.deadline_ms;
        // Decided at once (rlxd's rules, no app), or registered to wait for the app.
        let mut waiting = {
            let mut inner = run.inner();
            // Each call is decided once: a second request for it (a forged one, say) is
            // denied, and changes nothing of the first.
            if inner.gate.contains_key(&call.id) || inner.calls.iter().any(|c| c.call_id == call.id)
            {
                let reason = "rlxd already decided a call of this id";
                return Some(Taken::deny(DecidedBy::Rlxd, reason));
            }
            let at_once = if inner.stopped.is_some() {
                Some((DecidedBy::Rlxd, STOPPED.to_owned()))
            } else if let Some(reason) = rlxd_rule(
                &run.params,
                inner.pinned_call_used,
                &inner.listed,
                &call.tool,
                &call.sha256,
            ) {
                Some((DecidedBy::Rlxd, reason))
            } else if !self.app_present() {
                let reason = "no app is connected to decide the call".to_owned();
                Some((DecidedBy::Unreachable, reason))
            } else {
                None
            };
            if let Some((by, reason)) = at_once {
                let taken =
                    self.decide_now(run, &mut inner, &call, Decision::Deny, by, &reason, None);
                return Some(taken);
            }
            let (answer, waiting) = oneshot::channel();
            let open = Status::Open {
                tool: call.tool.clone(),
                sha256: call.sha256.clone(),
                answer: Some(answer),
            };
            inner.gate.insert(call.id.clone(), open);
            let data = RunToolCallData::new(
                call.id.clone(),
                call.tool.clone(),
                call.input.clone(),
                call.sha256.clone(),
                deadline_ms,
            );
            self.emit(run, &mut inner, run_types::TOOL_CALL, &data);
            waiting
        };
        let mut guard = Unanswered::new(self, run, call, via);
        // Only a hook's connection is dropped so; the control channel's call is ended by
        // its run's end, which its driver watches.
        let dropped = async {
            match via {
                Via::Hook => self.hooks_dropped().await,
                Via::Control => std::future::pending().await,
            }
        };
        let deadline = tokio::time::Instant::now() + Duration::from_millis(deadline_ms);
        let mut came = tokio::select! {
            answer = &mut waiting => answer.map_or(Came::Gone, Came::Answer),
            () = tokio::time::sleep_until(deadline) => Came::Deadline,
            () = dropped => {
                // Denied by its end: the guard writes it.
                drop(guard);
                return None;
            }
        };
        if matches!(came, Came::Deadline) {
            // Still waiting: a timeout, decided under this one lock, so neither an answer
            // nor the run's end can come between.
            let timed_out = {
                let mut inner = run.inner();
                let waiting = matches!(
                    inner.gate.get(guard.call_id()),
                    Some(Status::Open {
                        answer: Some(_),
                        ..
                    })
                );
                waiting.then(|| {
                    let call = guard.take();
                    self.decide_now(
                        run,
                        &mut inner,
                        &call,
                        Decision::Deny,
                        DecidedBy::Timeout,
                        TIMED_OUT,
                        None,
                    )
                })
            };
            if let Some(taken) = timed_out {
                return Some(taken);
            }
            // The app's answer was taken as the deadline came: it is on its way.
            came = waiting.await.map_or(Came::Gone, Came::Answer);
        }
        let call = guard.take();
        let mut inner = run.inner();
        if inner.calls.iter().any(|c| c.call_id == call.id) {
            // The run ended first, or the app stopped it, and rlxd denied it.
            let reason = if inner.stopped.is_some() {
                STOPPED
            } else {
                RUN_ENDED
            };
            return Some(Taken::deny(DecidedBy::Rlxd, reason));
        }
        let Came::Answer(answer) = came else {
            // No answer, yet not settled: denied all the same.
            let taken = self.decide_now(
                run,
                &mut inner,
                &call,
                Decision::Deny,
                DecidedBy::Rlxd,
                RUN_ENDED,
                None,
            );
            return Some(taken);
        };
        // A pinned call is allowed once: two calls waiting together (Claude Code's
        // parallel calls) can't both be.
        let taken = if answer.decision == Decision::Allow
            && run.params.pinned_call.is_some()
            && inner.pinned_call_used
        {
            self.decide_now(
                run,
                &mut inner,
                &call,
                Decision::Deny,
                DecidedBy::Rlxd,
                PINNED_CALL_USED,
                None,
            )
        } else {
            self.decide_now(
                run,
                &mut inner,
                &call,
                answer.decision,
                DecidedBy::App,
                &answer.reason,
                answer.draft_id,
            )
        };
        let _ = answer.written.send(taken.decision);
        // A connectors.call run makes one call, and a deny ends it `failed {reason:
        // "denied"}` whatever `stop` says: there is nothing more of it to stop.
        if answer.stop && taken.decision != Decision::Allow && via == Via::Hook {
            // The run ends needing the user, once the hook has its deny; rlxd denies its
            // other calls itself, those waiting and those to come.
            inner.stopped = Some(call.id.clone());
            self.settle(run, &mut inner, STOPPED);
            if let Some(tree) = inner.tree.clone() {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    tree.kill();
                });
            }
        }
        info!(
            "gate: run {} call {:?}: {}",
            run.params.run_id,
            call.id,
            taken.decision.as_str()
        );
        Some(taken)
    }

    /// The run and the call a hook request is about, when it is bound to the run's live
    /// attempt: its key, and a process inside the run (the module's doc).
    #[cfg_attr(not(feature = "debug-methods"), allow(dead_code))]
    fn bind(&self, ask: &Ask) -> Result<(Arc<Run>, Call), String> {
        let id =
            rlxd_proto::RunId::new(ask.run_id.clone()).map_err(|_| "no such run".to_owned())?;
        let run = self.find(&id).map_err(|_| "no such run".to_owned())?;
        let inner = run.inner();
        if inner.state != RunState::Running || inner.attempt != ask.attempt {
            return Err(format!("attempt {} of the run is not running", ask.attempt));
        }
        let key_matches = inner
            .hook_key
            .as_deref()
            .is_some_and(|key| !key.is_empty() && equal(key.as_bytes(), ask.key.as_bytes()));
        if !key_matches {
            return Err("its key is not the attempt's".to_owned());
        }
        let inside = inner
            .tree
            .as_ref()
            .is_some_and(|tree| process::holds(tree, &ask.peer));
        if !inside {
            return Err("the process that asked is not inside the run".to_owned());
        }
        drop(inner);
        let tool = ask.stdin["tool_name"]
            .as_str()
            .filter(|tool| !tool.is_empty())
            .ok_or_else(|| "no tool_name".to_owned())?
            .to_owned();
        let input = ask
            .stdin
            .get("tool_input")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let id = ask.stdin["tool_use_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "no tool_use_id".to_owned())?
            .to_owned();
        Ok((run, Call::new(id, tool, input)))
    }

    /// Writes `call`'s decision to the ledger, durably (and `executing` for an allow, and
    /// the pinned call used), then publishes `run.tool_decided`: all before the hook gets
    /// its answer. An allow whose entry can't be written is a deny `by: "rlxd"` instead
    /// (the pinned call stays used): never an allow without its entry on disk.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn decide_now(
        &self,
        run: &Run,
        inner: &mut RunInner,
        call: &Call,
        decision: Decision,
        by: DecidedBy,
        reason: &str,
        draft_id: Option<String>,
    ) -> Taken {
        let entry = |decision, by, state| {
            RunCall::new(
                call.id.clone(),
                call.tool.clone(),
                call.sha256.clone(),
                decision,
                by,
                state,
            )
        };
        inner.gate.insert(call.id.clone(), Status::Decided(by));
        let state = if decision == Decision::Allow {
            CallState::Executing
        } else {
            CallState::Decided
        };
        inner.calls.push(entry(decision, by, state));
        if decision == Decision::Allow && run.params.pinned_call.is_some() {
            inner.pinned_call_used = true;
        }
        let (decision, by, reason, draft_id) = match self.write_ledger(run, inner) {
            Ok(()) => (decision, by, reason, draft_id),
            Err(e) => {
                warning!(
                    "gate: run {}: the ledger was not written: {e}",
                    run.params.run_id
                );
                if decision == Decision::Allow {
                    let denied = entry(Decision::Deny, DecidedBy::Rlxd, CallState::Decided);
                    if let Some(last) = inner.calls.last_mut() {
                        *last = denied;
                    }
                    inner
                        .gate
                        .insert(call.id.clone(), Status::Decided(DecidedBy::Rlxd));
                    let _ = self.write_ledger(run, inner);
                    (Decision::Deny, DecidedBy::Rlxd, LEDGER_UNWRITTEN, None)
                } else {
                    (decision, by, reason, draft_id)
                }
            }
        };
        let data =
            RunToolDecidedData::new(call.id.clone(), decision, by, reason).with_draft(draft_id);
        self.emit(run, inner, run_types::TOOL_DECIDED, &data);
        Taken {
            decision,
            by,
            reason: reason.to_owned(),
        }
    }

    /// Writes `run`'s record, and so its ledger, durably. Test builds fail it for the
    /// space `RLXD_TEST_FAIL_LEDGER` names.
    fn write_ledger(&self, run: &Run, inner: &RunInner) -> std::io::Result<()> {
        if self.fail_ledger.as_deref() == Some(run.params.space.as_str()) {
            return Err(std::io::Error::other("refused, as the test asked"));
        }
        let record = run.record(inner);
        blocking(|| run.dir.write_record(&record))
    }

    /// Denies, by rlxd, every call of `run` still open as it ends: into the ledger and the
    /// stream before the run's end, which writes the ledger. Each call's hook gets a deny.
    pub(super) fn settle_open(&self, run: &Run, inner: &mut RunInner) {
        self.settle(run, inner, RUN_ENDED);
    }

    /// Denies, by rlxd for `reason`, every call of `run` still open.
    fn settle(&self, run: &Run, inner: &mut RunInner, reason: &str) {
        let open: Vec<(String, String, String)> = inner
            .gate
            .iter()
            .filter_map(|(id, status)| match status {
                Status::Open { tool, sha256, .. } => {
                    Some((id.clone(), tool.clone(), sha256.clone()))
                }
                Status::Decided(_) => None,
            })
            .collect();
        for (id, tool, sha256) in open {
            inner
                .gate
                .insert(id.clone(), Status::Decided(DecidedBy::Rlxd));
            inner.calls.push(RunCall::new(
                id.clone(),
                tool,
                sha256,
                Decision::Deny,
                DecidedBy::Rlxd,
                CallState::Decided,
            ));
            let data = RunToolDecidedData::new(id, Decision::Deny, DecidedBy::Rlxd, reason);
            self.emit(run, inner, run_types::TOOL_DECIDED, &data);
        }
    }

    /// `run.decide`: the app's answer to a pending call. The first answer wins. Its reply
    /// says the decision taken once it is written: the app's, or rlxd's deny.
    pub async fn decide(&self, params: RunDecideParams) -> Result<RunDecideData, ErrorBody> {
        let run = self.find(&params.run_id)?;
        let written = {
            let mut inner = run.inner();
            let status = inner.gate.get(&params.call_id).map(|status| match status {
                Status::Open {
                    answer: Some(_), ..
                } => None,
                Status::Open { answer: None, .. } => Some(DecidedBy::App),
                Status::Decided(by) => Some(*by),
            });
            let known = inner
                .calls
                .iter()
                .find(|call| call.call_id == params.call_id)
                .and_then(|call| call.by.clone());
            match (status, known) {
                (Some(None), _) => {
                    let answer = match inner.gate.get_mut(&params.call_id) {
                        Some(Status::Open { answer, .. }) => answer.take(),
                        _ => None,
                    };
                    let Some(answer) = answer else {
                        unreachable!("the call was open under this lock");
                    };
                    let (written, wait) = oneshot::channel();
                    let sent = answer.send(AppAnswer {
                        decision: params.decision,
                        reason: params.reason.clone(),
                        draft_id: params.draft_id.clone(),
                        stop: params.stop,
                        written,
                    });
                    if sent.is_err() {
                        return Err(already(&params.call_id));
                    }
                    wait
                }
                (Some(Some(DecidedBy::Timeout)), _) => return Err(too_late(&params.call_id)),
                (None, Some(by)) if by == DecidedBy::Timeout.as_str() => {
                    return Err(too_late(&params.call_id));
                }
                (Some(Some(_)), _) | (None, Some(_)) => return Err(already(&params.call_id)),
                (None, None) => {
                    return Err(refusal(
                        codes::UNKNOWN_CALL,
                        format!("run {} has no call {}", params.run_id, params.call_id),
                    ));
                }
            }
        };
        // The waiting call takes it at once and writes it; it drops the answer unused only
        // when its hook's connection or the run ended first.
        let taken = written.await.map_err(|_| already(&params.call_id))?;
        Ok(RunDecideData::new(params.run_id, params.call_id, taken))
    }

    /// The call `call_id`'s `tool_result`: `ok`, or `error` when `is_error`, for a call the
    /// ledger has executing. Written durably. The call's entry, when it was one the gate
    /// allowed.
    pub(super) fn tool_ran(
        &self,
        run: &Run,
        inner: &mut RunInner,
        call_id: &str,
        is_error: bool,
    ) -> Option<RunCall> {
        let call = inner
            .calls
            .iter_mut()
            .find(|call| call.call_id == call_id && call.state == CallState::Executing)?;
        call.state = if is_error {
            CallState::Error
        } else {
            CallState::Ok
        };
        let ran = call.clone();
        if let Err(e) = self.write_ledger(run, inner) {
            warning!(
                "gate: run {}: the ledger was not written: {e}",
                run.params.run_id
            );
        }
        Some(ran)
    }
}

/// A call waiting for the app whose asker went before it was decided (a hook's
/// connection that ended, a control channel's run that is ending): denied by rlxd, so the
/// ledger and the stream say so.
struct Unanswered {
    runner: Arc<Runner>,
    run: Arc<Run>,
    call: Option<Call>,
    via: Via,
}

impl Unanswered {
    /// Holds `call` of `run`, counted among the hook connections waiting on the gate when
    /// it came `via` a hook.
    fn new(runner: &Arc<Runner>, run: &Arc<Run>, call: Call, via: Via) -> Self {
        if via == Via::Hook {
            runner.count_waiting(true);
        }
        Self {
            runner: Arc::clone(runner),
            run: Arc::clone(run),
            call: Some(call),
            via,
        }
    }

    fn call_id(&self) -> &str {
        self.call.as_ref().map_or("", |call| call.id.as_str())
    }

    /// The call, taken back to be decided by its caller.
    fn take(&mut self) -> Call {
        self.call
            .take()
            .expect("the call is the guard's until it is taken")
    }
}

impl Drop for Unanswered {
    fn drop(&mut self) {
        if self.via == Via::Hook {
            self.runner.count_waiting(false);
        }
        let Some(call) = self.call.take() else {
            return;
        };
        let mut inner = self.run.inner();
        if matches!(inner.gate.get(&call.id), Some(Status::Decided(_))) {
            return;
        }
        let reason = match self.via {
            Via::Hook => "the hook's connection ended before the call was decided",
            Via::Control => "the run ended before the call was decided",
        };
        self.runner.decide_now(
            &self.run,
            &mut inner,
            &call,
            Decision::Deny,
            DecidedBy::Rlxd,
            reason,
            None,
        );
    }
}

/// rlxd's own rules (5.1), in order: the mount, the kind, the pinned call. `listed` is
/// what the attempt's `system/init` listed. The reason a call is denied, or `None` when
/// the app is to decide it.
///
/// Under a `connectors` mount, a call reaches the app only when its tool is one the init
/// listed, by its exact name, and is a tool of exactly one listed MCP server, one the
/// mount names. A connector still connecting when the init is written has no tools in it,
/// so its calls are denied: a run that names it in `needs_connectors` starts its first
/// turn only once it is connected (5.5).
pub fn rlxd_rule(
    params: &RunStartParams,
    pinned_call_used: bool,
    listed: &Listed,
    tool: &str,
    input_sha256: &str,
) -> Option<String> {
    match &params.mount {
        Mount::NoTools => {
            return Some("the run's mount is no_tools: it makes no tool calls".to_owned());
        }
        Mount::Connectors { connectors } => {
            if !listed.tools.contains(tool) {
                return Some(TOOL_NOT_IN_INIT.to_owned());
            }
            // Every listed server whose name, as Claude Code writes it, begins the tool's.
            let of: Vec<&Server> = listed
                .servers
                .iter()
                .filter(|server| {
                    tool.strip_prefix("mcp__")
                        .and_then(|rest| {
                            rest.strip_prefix(claude_server_name(&server.name).as_str())
                        })
                        .is_some_and(|rest| rest.starts_with("__"))
                })
                .collect();
            match of.as_slice() {
                [server]
                    if connectors
                        .iter()
                        .any(|connector| is_mounted(connector, server)) => {}
                [_, _, ..] => {
                    return Some(format!(
                        "{tool} could be a tool of more than one of the run's MCP servers"
                    ));
                }
                _ => {
                    return Some(format!(
                        "{tool} is not a tool of the connectors the run's mount names"
                    ));
                }
            }
        }
    }
    let kind = params.kind;
    if kind.needs_no_tools() {
        return Some(format!("a {kind} run makes no tool calls"));
    }
    if matches!(kind, RunKind::WidgetRefresh | RunKind::Watch)
        && read_only(&listed.hints, tool) != Some(true)
    {
        return Some(format!(
            "a {kind} run makes read calls only, and {tool} is not marked read-only"
        ));
    }
    if let Some(pinned) = &params.pinned_call {
        if pinned_call_used {
            return Some(PINNED_CALL_USED.to_owned());
        }
        if pinned.tool != tool || pinned.input_sha256 != input_sha256 {
            return Some("the call is not the run's pinned call".to_owned());
        }
    }
    None
}

/// Whether `tool` is read-only, as its MCP `readOnlyHint` says: from the run's own
/// `system/init` (tools listed as `{name, annotations: {readOnlyHint}}`). `None` when no
/// hint is known, which every rule counts as a write (fail closed). The one place the hint
/// comes from (hub question #2718).
pub fn read_only(hints: &HashMap<String, bool>, tool: &str) -> Option<bool> {
    hints.get(tool).copied()
}

/// An MCP server's name as Claude Code writes it in its tools' names
/// (`mcp__<server>__<tool>`), as Claude Code 2.1 makes it: each UTF-16 unit but an ASCII
/// letter, a digit, `_` or `-` as `_`; and for a claude.ai connector (a name that begins
/// `claude.ai `), runs of `_` as one, and none at either end. So `claude.ai Slack` is
/// `claude_ai_Slack`, and `claude.ai Slack (Beta)` is `claude_ai_Slack_Beta`.
pub fn claude_server_name(name: &str) -> String {
    let mut written = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            written.push(c);
        } else {
            written.extend(std::iter::repeat_n('_', c.len_utf16()));
        }
    }
    if !name.starts_with(CLAUDE_AI) {
        return written;
    }
    let mut collapsed = String::with_capacity(written.len());
    for c in written.chars() {
        if !(c == '_' && collapsed.ends_with('_')) {
            collapsed.push(c);
        }
    }
    collapsed.trim_matches('_').to_owned()
}

/// Claude Code's PreToolUse hook output for `decision`, with `reason`.
#[cfg_attr(not(feature = "debug-methods"), allow(dead_code))]
pub fn output(decision: Decision, reason: &str) -> String {
    let permission = if decision == Decision::Allow {
        "allow"
    } else {
        "deny"
    };
    json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": permission,
        "permissionDecisionReason": reason,
    }})
    .to_string()
}

/// Whether `a` and `b` are equal, in time that doesn't depend on where they differ.
#[cfg_attr(not(feature = "debug-methods"), allow(dead_code))]
fn equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

fn already(call_id: &str) -> ErrorBody {
    refusal(
        codes::ALREADY_DECIDED,
        format!("call {call_id} was decided already: the first answer won"),
    )
}

fn too_late(call_id: &str) -> ErrorBody {
    refusal(
        codes::TOO_LATE,
        format!("call {call_id}'s deadline passed, and it was denied"),
    )
}

/// A new attempt's hook key: 256 random bits, in hex. `None` when the OS gives no
/// randomness: then the attempt doesn't start.
pub fn new_key() -> Option<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use rlxd_proto::{PinnedCall, RunId};

    use super::*;

    fn params(kind: RunKind, mount: Mount) -> RunStartParams {
        let mut params = RunStartParams::new(
            RunId::new("0f8e2d6c-3b1a-4c5d-9e7f-1a2b3c4d5e6f").unwrap(),
            kind,
            "x",
        );
        params.mount = mount;
        params
    }

    fn mount(connectors: &[&str]) -> Mount {
        Mount::Connectors {
            connectors: connectors.iter().map(|c| (*c).to_owned()).collect(),
        }
    }

    fn slack() -> Mount {
        mount(&["claude.ai Slack"])
    }

    const SEND: &str = "mcp__claude_ai_Slack__slack_send_message";
    const READ: &str = "mcp__claude_ai_Slack__slack_search_public";

    /// What an init listed: `tools`, `servers`, and `hints` for its tools.
    /// A claude.ai connector's names come from claude.ai, any other from a local config.
    fn listed(tools: &[&str], servers: &[&str], hints: &[(&str, bool)]) -> Listed {
        Listed {
            tools: tools.iter().map(|t| (*t).to_owned()).collect(),
            servers: servers.iter().map(|name| server(name)).collect(),
            hints: hints.iter().map(|(t, h)| ((*t).to_owned(), *h)).collect(),
        }
    }

    fn server(name: &str) -> Server {
        let source = if name.starts_with(CLAUDE_AI) {
            CLAUDE_AI_SOURCE
        } else {
            "local"
        };
        Server {
            name: name.to_owned(),
            source: Some(source.to_owned()),
        }
    }

    /// Slack's two tools, the send a write and the search a read.
    fn slack_listed() -> Listed {
        listed(
            &[SEND, READ, "Bash"],
            &["claude.ai Slack"],
            &[(READ, true), (SEND, false)],
        )
    }

    #[test]
    fn the_mount_comes_first() {
        let mut both = slack_listed();
        both.servers.push(server("claude.ai Gmail"));
        both.tools.insert("mcp__claude_ai_Gmail__send".to_owned());
        let no_tools = params(RunKind::Task, Mount::NoTools);
        assert!(rlxd_rule(&no_tools, false, &both, READ, "h").is_some());
        let task = params(RunKind::Task, slack());
        assert_eq!(rlxd_rule(&task, false, &both, SEND, "h"), None);
        for other in [
            "Bash",
            "mcp__claude_ai_Gmail__send",
            "mcp__claude_ai_Slack",
            "mcp__x",
        ] {
            assert!(
                rlxd_rule(&task, false, &both, other, "h").is_some(),
                "{other}"
            );
        }
        // No init seen yet: nothing is listed, so nothing passes the mount.
        assert_eq!(
            rlxd_rule(&task, false, &Listed::default(), SEND, "h").as_deref(),
            Some(TOOL_NOT_IN_INIT)
        );
    }

    #[test]
    fn a_tool_the_init_did_not_list_is_denied_though_its_name_reads_as_the_connectors() {
        let task = params(RunKind::Task, slack());
        let evil = "mcp__claude_ai_Slack__evil__delete_everything";
        // Only the connector is listed, and the tool isn't: denied.
        assert_eq!(
            rlxd_rule(&task, false, &slack_listed(), evil, "h").as_deref(),
            Some(TOOL_NOT_IN_INIT)
        );
        // A tool the init listed, of the one server whose name it reads as: the app's.
        let mut with = slack_listed();
        with.tools.insert(evil.to_owned());
        assert_eq!(rlxd_rule(&task, false, &with, evil, "h"), None);
    }

    #[test]
    fn a_server_named_as_a_claude_ai_connector_is_it_only_from_claude_ai() {
        let task = params(RunKind::Task, slack());
        let with = |source: Option<&str>| Listed {
            servers: vec![Server {
                name: "claude.ai Slack".to_owned(),
                source: source.map(str::to_owned),
            }],
            ..slack_listed()
        };
        assert_eq!(
            rlxd_rule(&task, false, &with(Some("claudeai")), SEND, "h"),
            None
        );
        for impostor in [
            Some("local"),
            Some("user"),
            Some("project"),
            Some("plugin"),
            None,
        ] {
            assert!(
                rlxd_rule(&task, false, &with(impostor), SEND, "h").is_some(),
                "{impostor:?}"
            );
        }
        // A mount naming a server that isn't a claude.ai connector matches it by name.
        let local = params(RunKind::Task, mount(&["mine"]));
        let tool = "mcp__mine__do";
        let seen = listed(&[tool], &["mine"], &[]);
        assert_eq!(rlxd_rule(&local, false, &seen, tool, "h"), None);
    }

    #[test]
    fn a_tool_is_its_one_servers_and_a_look_alike_server_is_denied() {
        let task = params(RunKind::Task, slack());
        let rule = |servers: &[&str], tool: &str| {
            rlxd_rule(
                &task,
                false,
                &listed(&[tool, SEND], servers, &[]),
                tool,
                "h",
            )
        };
        // A local server whose name holds `__`, or is the connector's as written: two
        // servers could own the tool, and the call is denied.
        let nested = ["claude.ai Slack", "claude_ai_Slack__x"];
        let reason = rule(&nested, "mcp__claude_ai_Slack__x__send").unwrap();
        assert!(reason.contains("more than one"), "{reason}");
        assert!(rule(&nested, SEND).is_none(), "only the connector's");
        let same = ["claude.ai Slack", "claude_ai_Slack"];
        assert!(
            rule(&same, "mcp__claude_ai_Slack__post")
                .unwrap()
                .contains("more than one")
        );
        // A local server alone, its name the connector's as written: not the mount's.
        assert!(rule(&["claude_ai_Slack"], "mcp__claude_ai_Slack__post").is_some());
        // A connector whose name collapses: its own tools pass.
        let beta = params(RunKind::Task, mount(&["claude.ai Slack (Beta)"]));
        let tool = "mcp__claude_ai_Slack_Beta__send";
        let seen = listed(&[tool], &["claude.ai Slack (Beta)"], &[]);
        assert_eq!(rlxd_rule(&beta, false, &seen, tool, "h"), None);
    }

    #[test]
    fn server_names_are_written_as_claude_code_writes_them() {
        for (name, written) in [
            ("claude.ai Slack", "claude_ai_Slack"),
            ("claude.ai Slack (Beta)", "claude_ai_Slack_Beta"),
            ("claude.ai  Google  Drive ", "claude_ai_Google_Drive"),
            ("claude_ai_Slack__x", "claude_ai_Slack__x"),
            ("my server (local)", "my_server__local_"),
            ("a-b_c.d", "a-b_c_d"),
            // One UTF-16 unit each, or two: an emoji is two.
            ("caf\u{e9}", "caf_"),
            ("x\u{1f600}y", "x__y"),
        ] {
            assert_eq!(claude_server_name(name), written, "{name}");
        }
    }

    #[test]
    fn a_kind_that_reads_only_needs_a_read_only_hint() {
        let seen = slack_listed();
        for kind in [RunKind::WidgetRefresh, RunKind::Watch] {
            let mut run = params(kind, slack());
            if kind == RunKind::Watch {
                run.pinned_call = Some(PinnedCall {
                    tool: READ.to_owned(),
                    input_sha256: "h".to_owned(),
                });
            }
            assert_eq!(rlxd_rule(&run, false, &seen, READ, "h"), None, "{kind}");
            assert!(rlxd_rule(&run, false, &seen, SEND, "h").is_some(), "{kind}");
            // No hint is a write.
            let mut unhinted = seen.clone();
            unhinted.hints.clear();
            assert!(
                rlxd_rule(&run, false, &unhinted, READ, "h").is_some(),
                "{kind}"
            );
        }
        for kind in [
            RunKind::Check,
            RunKind::ParseInstruction,
            RunKind::WidgetAuthor,
        ] {
            let run = params(kind, Mount::NoTools);
            assert!(rlxd_rule(&run, false, &seen, READ, "h").is_some(), "{kind}");
        }
    }

    #[test]
    fn a_pinned_call_is_made_once_and_only_as_pinned() {
        let mut send = params(RunKind::SendExecutor, slack());
        send.pinned_call = Some(PinnedCall {
            tool: SEND.to_owned(),
            input_sha256: "h1".to_owned(),
        });
        let seen = slack_listed();
        assert_eq!(rlxd_rule(&send, false, &seen, SEND, "h1"), None);
        assert!(
            rlxd_rule(&send, false, &seen, SEND, "h2").is_some(),
            "another input"
        );
        assert!(
            rlxd_rule(&send, false, &seen, READ, "h1").is_some(),
            "another tool"
        );
        assert_eq!(
            rlxd_rule(&send, true, &seen, SEND, "h1").as_deref(),
            Some(PINNED_CALL_USED)
        );
    }

    #[test]
    fn keys_compare_whole_and_are_new_each_time() {
        assert!(equal(b"abc", b"abc"));
        assert!(!equal(b"abc", b"abd"));
        assert!(!equal(b"abc", b"abcd"));
        let (a, b) = (new_key().unwrap(), new_key().unwrap());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }
}
