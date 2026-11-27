//! The tool-call gate (handoff 0.10.3, 5.1): it decides every tool call of every run, and
//! fails closed.
//!
//! - **A hook request is bound to its run.** It must name the run's live attempt, carry
//!   that attempt's key (`RLXD_HOOK_KEY`, random at each attempt's start, in Claude Code's
//!   environment and so its hooks'), and come from a process inside the run: on macOS and
//!   Linux one in the run's process group (the guard leads a session of its own, so no
//!   process outside the run can join it), on Windows one in the run's Job Object. Any
//!   other request is denied at once, with no event and no ledger entry, so a guessed
//!   run id from outside the run reaches nothing. The key is inherited by every process
//!   Claude Code starts, not only its hooks, so a process inside the run can ask about a
//!   call of its own making: it gets the app's answer to that call only, never an allow
//!   for one of Claude Code's (each call is its own `call_id`, decided once), and it can't
//!   reach another run. The key is never logged, published or written to disk.
//! - **rlxd's own rules come first,** and deny `by: "rlxd"`: the mount, the kind and the
//!   pinned call ([`rlxd_rule`]). A tool's read-only hint is [`read_only`]'s alone.
//! - **Every other call goes to the app** as `run.tool_call`, and only `run.decide` from an
//!   app-role connection answers it; the first answer wins. With no app connected it is
//!   denied `by: "unreachable"` at once. Pending when the last app leaves, it is held
//!   until its deadline, then denied `by: "timeout"`. Nothing a run writes (its text, a
//!   tool's result, a prompt) can decide a call: a reason is data only.
//! - **The ledger** in `run.json` holds every call: `decided`, and `executing` for an
//!   allow, written and fsynced before the hook gets its answer; `ok` or `error` from the
//!   call's `tool_result`; `unknown` when the run ends with it still executing.
//!
//! Until rlxd-hook's own pre-tool-use is wired to [`Runner::pre_tool_use`] (with #617, at
//! rc/0.10.3's merge), only test builds reach the hook side, through `debug.pre_tool_use`.

// Until then, a release build makes no hook request: its hook side is unused there.
#![cfg_attr(not(feature = "debug-methods"), allow(dead_code))]

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

/// The name of the variable that holds an attempt's hook key.
pub const HOOK_KEY_ENV: &str = "RLXD_HOOK_KEY";

/// How long `run.decide` waits for its decision to be written before it answers anyway.
const DECIDE_WAIT: Duration = Duration::from_secs(30);

/// A hook request: what `rlxd-hook pre-tool-use` sends for one call.
pub struct Ask {
    pub run_id: String,
    pub attempt: u64,
    /// The attempt's key, as the hook got it in its environment.
    pub key: String,
    /// The connecting process, as the socket or pipe pins it.
    pub peer: Option<u32>,
    /// Claude Code's PreToolUse input: `tool_name`, `tool_input`, `tool_use_id`, ….
    pub stdin: Value,
}

/// Where a call the gate is deciding stands, by `call_id`.
pub(super) enum Status {
    /// Not decided yet: waiting for the app, whose answer goes to `answer`, or, `answer`
    /// taken, the app's answer being written.
    Open {
        tool: String,
        sha256: String,
        answer: Option<oneshot::Sender<AppAnswer>>,
    },
    /// Decided, by whom.
    Decided(DecidedBy),
}

/// The reason of a call still open as its run ends.
const RUN_ENDED: &str = "the run ended before the call was decided";

/// The app's `run.decide`, handed to the call waiting for it.
pub(super) struct AppAnswer {
    decision: Decision,
    reason: String,
    draft_id: Option<String>,
    stop: bool,
    /// Told the decision taken once it is written: the app's, or rlxd's deny when the
    /// app allowed a pinned call that another call used meanwhile.
    written: oneshot::Sender<Decision>,
}

/// One call, as the gate reads it.
struct Call {
    id: String,
    tool: String,
    input: Value,
    sha256: String,
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
    pub fn drop_hooks(&self) -> u64 {
        let waiting = *self.hooks_waiting.borrow();
        self.hooks_dropped.send_modify(|n| *n += 1);
        waiting
    }

    /// The gate's answer to one hook request: Claude Code's hook JSON, allow or deny.
    /// `None` when its connection was dropped (`debug.drop_hook_connections`): the hook
    /// then gets no answer, and denies.
    pub async fn pre_tool_use(self: &Arc<Self>, ask: Ask) -> Option<String> {
        let (run, call) = match self.bind(&ask) {
            Ok(bound) => bound,
            Err(why) => {
                warning!(
                    "gate: a hook request for run {} was refused: {why}",
                    ask.run_id
                );
                let reason = format!("rlxd refused the hook request: {why}");
                return Some(output(Decision::Deny, &reason));
            }
        };
        let deadline_ms = run.params.deadline_ms;
        // Decided at once (rlxd's rules, no app), or registered to wait for the app.
        let mut waiting = {
            let mut inner = run.inner();
            // Each call is decided once: a second request for it (a forged one, say) is
            // denied, and changes nothing of the first.
            if inner.gate.contains_key(&call.id) || inner.calls.iter().any(|c| c.call_id == call.id)
            {
                let reason = "rlxd already decided a call of this id";
                return Some(output(Decision::Deny, reason));
            }
            if let Some(reason) = rlxd_rule(
                &run.params,
                inner.pinned_call_used,
                &inner.hints,
                &call.tool,
                &call.sha256,
            ) {
                self.decide_now(
                    &run,
                    &mut inner,
                    &call,
                    Decision::Deny,
                    DecidedBy::Rlxd,
                    &reason,
                    None,
                );
                return Some(output(Decision::Deny, &reason));
            }
            if !self.app_present() {
                let reason = "no app is connected to decide the call";
                self.decide_now(
                    &run,
                    &mut inner,
                    &call,
                    Decision::Deny,
                    DecidedBy::Unreachable,
                    reason,
                    None,
                );
                return Some(output(Decision::Deny, reason));
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
            self.emit(&run, &mut inner, run_types::TOOL_CALL, &data);
            waiting
        };
        let mut guard = Unanswered::new(self, &run, call);
        let mut dropped = self.hooks_dropped.subscribe();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(deadline_ms);
        let answered = tokio::select! {
            answer = &mut waiting => answer.ok(),
            () = tokio::time::sleep_until(deadline) => {
                // The deadline came: unless the app's answer is on its way, it is a timeout.
                let racing = {
                    let mut inner = run.inner();
                    let call_id = guard.call_id();
                    match inner.gate.get(call_id) {
                        Some(Status::Open { answer: Some(_), .. }) => {
                            let timeout = Status::Decided(DecidedBy::Timeout);
                            inner.gate.insert(call_id.to_owned(), timeout);
                            false
                        }
                        _ => true,
                    }
                };
                if racing { waiting.await.ok() } else { None }
            }
            _ = dropped.changed() => {
                // Denied by its end: the guard writes it.
                drop(guard);
                return None;
            }
        };
        let call = guard
            .call
            .take()
            .expect("the call is the guard's until here");
        let mut inner = run.inner();
        if inner.calls.iter().any(|c| c.call_id == call.id) {
            // The run ended first, and denied it.
            return Some(output(Decision::Deny, RUN_ENDED));
        }
        match answered {
            // A pinned call is allowed once: two calls waiting together (Claude Code's
            // parallel calls) can't both be.
            Some(answer)
                if answer.decision == Decision::Allow
                    && run.params.pinned_call.is_some()
                    && inner.pinned_call_used =>
            {
                self.decide_now(
                    &run,
                    &mut inner,
                    &call,
                    Decision::Deny,
                    DecidedBy::Rlxd,
                    PINNED_CALL_USED,
                    None,
                );
                let _ = answer.written.send(Decision::Deny);
                Some(output(Decision::Deny, PINNED_CALL_USED))
            }
            Some(answer) => {
                let decision = answer.decision;
                self.decide_now(
                    &run,
                    &mut inner,
                    &call,
                    decision,
                    DecidedBy::App,
                    &answer.reason,
                    answer.draft_id,
                );
                let _ = answer.written.send(decision);
                if answer.stop && decision != Decision::Allow {
                    // The run ends needing the user, once the hook has its deny.
                    inner.stopped = Some(call.id.clone());
                    if let Some(tree) = inner.tree.clone() {
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            tree.kill();
                        });
                    }
                }
                info!(
                    "gate: run {} call {}: {} by the app",
                    run.params.run_id,
                    call.id,
                    decision.as_str()
                );
                Some(output(decision, &answer.reason))
            }
            None => {
                let reason = "no answer came from the app before the call's deadline";
                self.decide_now(
                    &run,
                    &mut inner,
                    &call,
                    Decision::Deny,
                    DecidedBy::Timeout,
                    reason,
                    None,
                );
                Some(output(Decision::Deny, reason))
            }
        }
    }

    /// The run and the call a hook request is about, when it is bound to the run's live
    /// attempt (binding a and b in the module's doc, and c when the peer is known).
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
            .is_some_and(|key| equal(key.as_bytes(), ask.key.as_bytes()));
        if !key_matches {
            return Err("its key is not the attempt's".to_owned());
        }
        let Some(pid) = ask.peer else {
            return Err("the OS did not say which process asked".to_owned());
        };
        let inside = inner
            .tree
            .as_ref()
            .is_some_and(|tree| process::holds(tree, pid));
        if !inside {
            return Err(format!("process {pid} is not inside the run"));
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
        let sha256 = jcs::sha256(&input);
        Ok((
            run,
            Call {
                id,
                tool,
                input,
                sha256,
            },
        ))
    }

    /// Writes `call`'s decision to the ledger, durably (and `executing` for an allow, and
    /// the pinned call used), then publishes `run.tool_decided`: all before the hook gets
    /// its answer.
    #[allow(clippy::too_many_arguments)]
    fn decide_now(
        &self,
        run: &Run,
        inner: &mut RunInner,
        call: &Call,
        decision: Decision,
        by: DecidedBy,
        reason: &str,
        draft_id: Option<String>,
    ) {
        inner.gate.insert(call.id.clone(), Status::Decided(by));
        let state = if decision == Decision::Allow {
            CallState::Executing
        } else {
            CallState::Decided
        };
        inner.calls.push(RunCall::new(
            call.id.clone(),
            call.tool.clone(),
            call.sha256.clone(),
            decision,
            by,
            state,
        ));
        if decision == Decision::Allow && run.params.pinned_call.is_some() {
            inner.pinned_call_used = true;
        }
        let record = run.record(inner);
        if let Err(e) = blocking(|| run.dir.write_record(&record)) {
            warning!(
                "gate: run {}: the ledger was not written: {e}",
                run.params.run_id
            );
        }
        let data =
            RunToolDecidedData::new(call.id.clone(), decision, by, reason).with_draft(draft_id);
        self.emit(run, inner, run_types::TOOL_DECIDED, &data);
    }

    /// Denies, by rlxd, every call of `run` still open as it ends: into the ledger and the
    /// stream before the run's end, which writes the ledger. Each call's hook gets a deny.
    pub(super) fn settle_open(&self, run: &Run, inner: &mut RunInner) {
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
            let data = RunToolDecidedData::new(id, Decision::Deny, DecidedBy::Rlxd, RUN_ENDED);
            self.emit(run, inner, run_types::TOOL_DECIDED, &data);
        }
    }

    /// `run.decide`: the app's answer to a pending call. The first answer wins.
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
        let taken = match tokio::time::timeout(DECIDE_WAIT, written).await {
            Ok(Ok(taken)) => taken,
            // Its hook's connection ended, or the run did, before the answer was used.
            Ok(Err(_)) => return Err(already(&params.call_id)),
            // Not written in time: the answer is the call's, and is written when it can be.
            Err(_) => params.decision,
        };
        Ok(RunDecideData::new(params.run_id, params.call_id, taken))
    }

    /// The call `call_id`'s `tool_result`: `ok`, or `error` when `is_error`, for a call the
    /// ledger has executing. Written durably.
    pub(super) fn tool_ran(&self, run: &Run, inner: &mut RunInner, call_id: &str, is_error: bool) {
        let Some(call) = inner
            .calls
            .iter_mut()
            .find(|call| call.call_id == call_id && call.state == CallState::Executing)
        else {
            return;
        };
        call.state = if is_error {
            CallState::Error
        } else {
            CallState::Ok
        };
        let record = run.record(inner);
        if let Err(e) = blocking(|| run.dir.write_record(&record)) {
            warning!(
                "gate: run {}: the ledger was not written: {e}",
                run.params.run_id
            );
        }
    }
}

/// A call waiting for the app whose hook connection ended before it was decided: denied
/// by rlxd, so the ledger and the stream say so.
struct Unanswered {
    runner: Arc<Runner>,
    run: Arc<Run>,
    call: Option<Call>,
}

impl Unanswered {
    /// Holds `call` of `run`, counted among the hook connections waiting on the gate.
    fn new(runner: &Arc<Runner>, run: &Arc<Run>, call: Call) -> Self {
        runner.hooks_waiting.send_modify(|n| *n += 1);
        Self {
            runner: Arc::clone(runner),
            run: Arc::clone(run),
            call: Some(call),
        }
    }

    fn call_id(&self) -> &str {
        self.call.as_ref().map_or("", |call| call.id.as_str())
    }
}

impl Drop for Unanswered {
    fn drop(&mut self) {
        self.runner
            .hooks_waiting
            .send_modify(|n| *n = n.saturating_sub(1));
        let Some(call) = self.call.take() else {
            return;
        };
        let mut inner = self.run.inner();
        if matches!(inner.gate.get(&call.id), Some(Status::Decided(_))) {
            return;
        }
        let reason = "the hook's connection ended before the call was decided";
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

/// rlxd's own rules (5.1), in order: the mount, the kind, the pinned call. The reason a
/// call is denied, or `None` when the app is to decide it.
pub fn rlxd_rule(
    params: &RunStartParams,
    pinned_call_used: bool,
    hints: &HashMap<String, bool>,
    tool: &str,
    input_sha256: &str,
) -> Option<String> {
    match &params.mount {
        Mount::NoTools => {
            return Some("the run's mount is no_tools: it makes no tool calls".to_owned());
        }
        Mount::Connectors { connectors } => {
            let server = tool
                .strip_prefix("mcp__")
                .and_then(|rest| rest.split_once("__"))
                .map(|(server, _)| server);
            let named = server.is_some_and(|server| {
                connectors
                    .iter()
                    .any(|connector| server_name(connector) == server)
            });
            if !named {
                return Some(format!(
                    "{tool} is not a tool of the connectors the run's mount names"
                ));
            }
        }
    }
    let kind = params.kind;
    if kind.needs_no_tools() {
        return Some(format!("a {kind} run makes no tool calls"));
    }
    if matches!(kind, RunKind::WidgetRefresh | RunKind::Watch)
        && read_only(hints, tool) != Some(true)
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

/// A connector's name as Claude Code writes it in its tools' names (`mcp__<server>__<tool>`):
/// each character but an ASCII letter, a digit, `_` or `-` as `_` (`claude.ai Slack` is
/// `claude_ai_Slack`).
fn server_name(connector: &str) -> String {
    connector
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Claude Code's PreToolUse hook output for `decision`, with `reason`.
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

/// A new attempt's hook key: 256 random bits, in hex.
pub fn new_key() -> String {
    let mut bytes = [0_u8; 32];
    if getrandom::fill(&mut bytes).is_err() {
        // No randomness: no key any hook could match.
        return String::new();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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

    fn slack() -> Mount {
        Mount::Connectors {
            connectors: vec!["claude.ai Slack".to_owned()],
        }
    }

    const SEND: &str = "mcp__claude_ai_Slack__slack_send_message";
    const READ: &str = "mcp__claude_ai_Slack__slack_search_public";

    #[test]
    fn the_mount_comes_first() {
        let none = HashMap::new();
        let no_tools = params(RunKind::Task, Mount::NoTools);
        assert!(rlxd_rule(&no_tools, false, &none, READ, "h").is_some());
        let task = params(RunKind::Task, slack());
        assert_eq!(rlxd_rule(&task, false, &none, SEND, "h"), None);
        for other in [
            "Bash",
            "mcp__claude_ai_Gmail__send",
            "mcp__claude_ai_Slack",
            "mcp__x",
        ] {
            assert!(
                rlxd_rule(&task, false, &none, other, "h").is_some(),
                "{other}"
            );
        }
    }

    #[test]
    fn a_kind_that_reads_only_needs_a_read_only_hint() {
        let hints = HashMap::from([(READ.to_owned(), true), (SEND.to_owned(), false)]);
        for kind in [RunKind::WidgetRefresh, RunKind::Watch] {
            let mut run = params(kind, slack());
            if kind == RunKind::Watch {
                run.pinned_call = Some(PinnedCall {
                    tool: READ.to_owned(),
                    input_sha256: "h".to_owned(),
                });
            }
            assert_eq!(rlxd_rule(&run, false, &hints, READ, "h"), None, "{kind}");
            assert!(
                rlxd_rule(&run, false, &hints, SEND, "h").is_some(),
                "{kind}"
            );
            // No hint is a write.
            assert!(
                rlxd_rule(&run, false, &HashMap::new(), READ, "h").is_some(),
                "{kind}"
            );
        }
        for kind in [
            RunKind::Check,
            RunKind::ParseInstruction,
            RunKind::WidgetAuthor,
        ] {
            let run = params(kind, Mount::NoTools);
            assert!(
                rlxd_rule(&run, false, &hints, READ, "h").is_some(),
                "{kind}"
            );
        }
    }

    #[test]
    fn a_pinned_call_is_made_once_and_only_as_pinned() {
        let mut send = params(RunKind::SendExecutor, slack());
        send.pinned_call = Some(PinnedCall {
            tool: SEND.to_owned(),
            input_sha256: "h1".to_owned(),
        });
        let none = HashMap::new();
        assert_eq!(rlxd_rule(&send, false, &none, SEND, "h1"), None);
        assert!(
            rlxd_rule(&send, false, &none, SEND, "h2").is_some(),
            "another input"
        );
        assert!(
            rlxd_rule(&send, false, &none, READ, "h1").is_some(),
            "another tool"
        );
        assert_eq!(
            rlxd_rule(&send, true, &none, SEND, "h1").as_deref(),
            Some(PINNED_CALL_USED)
        );
    }

    #[test]
    fn keys_compare_whole_and_are_new_each_time() {
        assert!(equal(b"abc", b"abc"));
        assert!(!equal(b"abc", b"abd"));
        assert!(!equal(b"abc", b"abcd"));
        let (a, b) = (new_key(), new_key());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert_eq!(server_name("claude.ai Slack"), "claude_ai_Slack");
    }
}
