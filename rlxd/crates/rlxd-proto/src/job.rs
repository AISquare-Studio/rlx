//! The scheduler's wire types (handoff 0.10.3, 2.3 and 7.6): `job.put`, `job.list`,
//! `job.delete`, `job.pause`, `job.resume` and `job.run_now`, and the `job` topic's events.
//!
//! rlxapp owns its jobs' definitions and pushes them to rlxd by `job_id`; rlxd keeps a
//! definition only to fire it, and records what each fire did.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::check::{self, drop_null};
use crate::{Data, Invalid, RunId, RunStartParams, RunState};

/// The `job` topic's event types (7.6).
pub mod job_types {
    /// `job.put {job_id}`: a job was created or replaced.
    pub const PUT: &str = "job.put";
    /// `job.deleted {job_id}`.
    pub const DELETED: &str = "job.deleted";
    /// `job.paused {job_id}`.
    pub const PAUSED: &str = "job.paused";
    /// `job.resumed {job_id}`.
    pub const RESUMED: &str = "job.resumed";
    /// `job.fired {job_id, run_id, scheduled_for, fired_at, cause, loud}`.
    pub const FIRED: &str = "job.fired";
    /// `job.skipped {job_id, scheduled_for, reason}`.
    pub const SKIPPED: &str = "job.skipped";
    /// `job.watch_result {job_id, run_id, tool, input_sha256, content, is_error,
    /// truncated}`: a watch job's call's raw result.
    pub const WATCH_RESULT: &str = "job.watch_result";
}

/// A job's id, which the app picks: 1 to 128 ASCII letters, digits, `.`, `_`, `-` and
/// `:`, starting with a letter or a digit.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct JobId(
    #[schemars(
        regex(pattern = r"^[A-Za-z0-9][A-Za-z0-9._:-]*$"),
        length(min = 1, max = 128)
    )]
    String,
);

impl JobId {
    /// `id`, when it keeps the rule.
    pub fn new(id: impl Into<String>) -> Result<Self, Invalid> {
        let id = id.into();
        let first_ok = id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());
        let rest_ok = id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'));
        if id.is_empty() || id.len() > 128 || !first_ok || !rest_ok {
            return Err(Invalid::new(
                "job_id",
                "must be 1 to 128 ASCII letters, digits, '.', '_', '-' and ':', starting with \
                 a letter or a digit",
            ));
        }
        Ok(Self(id))
    }

    /// The id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for JobId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = String::deserialize(deserializer)?;
        Self::new(id).map_err(serde::de::Error::custom)
    }
}

/// A job's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobType {
    /// Each fire is a run of the job's `run` template.
    Run,
    /// Each fire is one pinned read call, with no prompt.
    Watch,
}

impl JobType {
    /// Its name on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Watch => "watch",
        }
    }
}

/// When a job fires: every so many minutes, measured from `job.put` (never drifting with
/// how late a fire ran), or at each match of a five-field cron expression in an IANA time
/// zone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum Schedule {
    /// `{"every_minutes": N}`.
    Every {
        /// Minutes between fires, 1 to 44,640 (31 days).
        #[schemars(range(min = 1, max = 44_640))]
        every_minutes: u32,
    },
    /// `{"cron": "*/15 * * * *", "tz": "Europe/London"}`.
    Cron {
        /// Minute, hour, day of month, month and day of week: `*`, numbers, lists,
        /// ranges, steps, and month and day names.
        #[schemars(length(min = 1, max = 256))]
        cron: String,
        /// An IANA time zone, such as `UTC` or `Europe/London`.
        #[schemars(length(min = 1, max = 64))]
        tz: String,
    },
}

/// What a job does about fires it couldn't make (no app connected, rlxd down): `once`
/// fires once when it can, `skip` waits for the next scheduled time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CatchUp {
    /// Fire once.
    Once,
    /// Wait for the next scheduled time.
    Skip,
}

impl CatchUp {
    /// Its name on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Skip => "skip",
        }
    }
}

/// A `run` job's template: `run.start`'s params (run-start.json) without `run_id`, which
/// rlxd picks for each fire. `job.put` checks it as `run.start` checks its params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunTemplate(pub Data);

impl RunTemplate {
    /// The template as `run.start`'s params for run `run_id`: the template's own checks
    /// are `run.start`'s (an invalid template is refused at `job.put`, not at a fire).
    pub fn start(&self, run_id: &RunId) -> Result<RunStartParams, Invalid> {
        if self.0.contains_key("run_id") {
            return Err(Invalid::new(
                "run.run_id",
                "is rlxd's to pick, for each fire: leave it out of the template",
            ));
        }
        let mut params = self.0.clone();
        params.insert("run_id".to_owned(), Value::String(run_id.to_string()));
        serde_json::from_value::<RunStartParams>(Value::Object(params))
            .map_err(|e| Invalid::new("run", e.to_string()))
    }
}

impl JsonSchema for RunTemplate {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("RunTemplate")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        // run-start.json's own schema, without run_id.
        let mut schema = RunStartParams::json_schema(generator);
        if let Some(object) = schema.as_object_mut() {
            if let Some(Value::Object(properties)) = object.get_mut("properties") {
                properties.remove("run_id");
            }
            if let Some(Value::Array(required)) = object.get_mut("required") {
                required.retain(|field| field != "run_id");
            }
            object.insert(
                "description".to_owned(),
                "A run job's template: run.start's params (run-start.json) without run_id, \
                 which rlxd picks for each fire."
                    .into(),
            );
            object.insert("title".to_owned(), "RunTemplate".into());
        }
        schema
    }
}

/// `job.put`'s params (7.6). It creates or replaces the job by `job_id`, idempotently,
/// and returns `{job_id}` (`job-ref.json`).
///
/// ```json
/// {"job_id": "slack-digest", "type": "run", "schedule": {"every_minutes": 15},
///  "run": {"kind": "widget_refresh", "prompt": "…", "mount": {"kind": "no_tools"},
///          "space": "default"},
///  "catch_up": "once", "enabled": true, "loud": false}
/// {"job_id": "ci-green", "type": "watch", "schedule": {"every_minutes": 5},
///  "connector": "claude.ai GitHub", "tool": "mcp__claude_ai_GitHub__get_check_runs",
///  "input": {"ref": "main"}, "catch_up": "skip", "enabled": true}
/// ```
///
/// rlxd refuses, by error code: `kind_mount_mismatch` and `pinned_call_required` for a
/// template `run.start` would refuse; `invalid_params` for anything else, an unknown
/// field, a cron expression it can't read, a time zone it doesn't know, or a watch job's
/// tool that isn't its connector's included.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "JobPutParams")]
#[non_exhaustive]
pub struct JobPutParams {
    /// The job's id.
    pub job_id: JobId,
    /// `run` or `watch`.
    #[serde(rename = "type")]
    pub r#type: JobType,
    /// When it fires.
    pub schedule: Schedule,
    /// A `run` job's template. Required for `run`, refused for `watch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub run: Option<RunTemplate>,
    /// A `watch` job's connector, such as `claude.ai GitHub`. Required for `watch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, length(min = 1))]
    pub connector: Option<String>,
    /// A `watch` job's tool, one of its connector's, such as
    /// `mcp__claude_ai_GitHub__get_check_runs`. Required for `watch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, length(min = 1))]
    pub tool: Option<String>,
    /// A `watch` job's tool input, a JSON object: the call is pinned to its RFC 8785
    /// SHA-256. Required for `watch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub input: Option<Map<String, Value>>,
    /// What it does about fires it couldn't make.
    pub catch_up: CatchUp,
    /// Whether it fires at all. Absent: `true`.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Opaque to rlxd: stored, and echoed on `job.fired` and on the fired run's
    /// `run.done` and `run.needs_you`. Absent: `false`.
    #[serde(default)]
    pub loud: bool,
}

fn yes() -> bool {
    true
}

impl JobPutParams {
    /// A `run` job `job_id` of `template`, on `schedule`, enabled and quiet.
    pub fn run(job_id: JobId, schedule: Schedule, template: Data, catch_up: CatchUp) -> Self {
        Self {
            job_id,
            r#type: JobType::Run,
            schedule,
            run: Some(RunTemplate(template)),
            connector: None,
            tool: None,
            input: None,
            catch_up,
            enabled: true,
            loud: false,
        }
    }

    /// A `watch` job `job_id`: one pinned call of `connector`'s `tool` with `input` per
    /// tick, on `schedule`, enabled and quiet.
    pub fn watch(
        job_id: JobId,
        schedule: Schedule,
        connector: impl Into<String>,
        tool: impl Into<String>,
        input: Map<String, Value>,
        catch_up: CatchUp,
    ) -> Self {
        Self {
            job_id,
            r#type: JobType::Watch,
            schedule,
            run: None,
            connector: Some(connector.into()),
            tool: Some(tool.into()),
            input: Some(input),
            catch_up,
            enabled: true,
            loud: false,
        }
    }

    /// Whether the params keep the rules the schema states: a `run` job has a `run`, and
    /// only a `watch` job names a connector, a tool or an input. (The template, the cron
    /// expression and the time zone are rlxd's to check.)
    pub fn validate(&self) -> Result<(), Invalid> {
        match self.r#type {
            JobType::Run => {
                if self.run.is_none() {
                    return Err(Invalid::new("run", "a run job needs its run template"));
                }
                for (field, present) in [
                    ("connector", self.connector.is_some()),
                    ("tool", self.tool.is_some()),
                    ("input", self.input.is_some()),
                ] {
                    if present {
                        return Err(Invalid::new(field, "is a watch job's, not a run job's"));
                    }
                }
            }
            JobType::Watch => {
                if self.run.is_some() {
                    return Err(Invalid::new("run", "is a run job's, not a watch job's"));
                }
                for (field, value) in [("connector", &self.connector), ("tool", &self.tool)] {
                    match value {
                        Some(value) => check::non_empty(field, value)?,
                        None => return Err(Invalid::new(field, "a watch job needs it")),
                    }
                }
                if self.input.is_none() {
                    return Err(Invalid::new("input", "a watch job needs it, an object"));
                }
            }
        }
        if let Schedule::Every { every_minutes } = self.schedule {
            check::at_least("schedule.every_minutes", u64::from(every_minutes), 1)?;
            check::at_most("schedule.every_minutes", u64::from(every_minutes), 44_640)?;
        }
        Ok(())
    }
}

/// `{job_id}`: `job.delete`'s, `job.pause`'s and `job.resume`'s params, `job.put`'s,
/// `job.delete`'s, `job.pause`'s and `job.resume`'s reply, and `job.run_now`'s params.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "JobRef")]
pub struct JobRef {
    /// The job's id.
    pub job_id: JobId,
}

impl JobRef {
    /// `{job_id}`.
    pub fn new(job_id: JobId) -> Self {
        Self { job_id }
    }
}

/// `job.list`'s params: none.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "JobListParams")]
pub struct JobListParams {}

/// One job, as `job.list` lists it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct JobSummary {
    /// The job's id.
    pub job_id: JobId,
    /// `run` or `watch`.
    #[serde(rename = "type")]
    pub r#type: JobType,
    /// When it fires.
    pub schedule: Schedule,
    /// What it does about fires it couldn't make.
    pub catch_up: CatchUp,
    /// Whether it fires at all.
    pub enabled: bool,
    /// Whether `job.pause` paused it.
    pub paused: bool,
    /// Opaque: as `job.put` gave it.
    pub loud: bool,
    /// When it is due next (RFC 3339, UTC); `null` when it isn't enabled.
    pub next_fire_at: Option<String>,
    /// When it last fired; `null` before its first fire.
    pub last_fire_at: Option<String>,
    /// Its last fire's run.
    pub last_run_id: Option<RunId>,
    /// Its last fire's run's state, as last known.
    pub last_state: Option<RunState>,
    /// How many of its due fires were skipped.
    pub skipped_count: u64,
}

impl JobSummary {
    /// Job `job_id` of `type`, on `schedule`, with nothing fired or skipped yet.
    pub fn new(job_id: JobId, r#type: JobType, schedule: Schedule, catch_up: CatchUp) -> Self {
        Self {
            job_id,
            r#type,
            schedule,
            catch_up,
            enabled: true,
            paused: false,
            loud: false,
            next_fire_at: None,
            last_fire_at: None,
            last_run_id: None,
            last_state: None,
            skipped_count: 0,
        }
    }
}

/// `job.list`'s reply.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "JobListData")]
pub struct JobListData {
    /// Every job rlxd holds, by `job_id`.
    pub jobs: Vec<JobSummary>,
}

/// `job.run_now`'s reply: the run it started.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "JobRunNowData")]
pub struct JobRunNowData {
    /// The job's id.
    pub job_id: JobId,
    /// The run it started.
    pub run_id: RunId,
}

/// Why a fire was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FireCause {
    /// It came due.
    Due,
    /// `catch_up: once`, for fires it couldn't make: no app was connected, or rlxd was
    /// down.
    CatchUp,
    /// `job.run_now`.
    RunNow,
}

/// `job.fired`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct JobFiredData {
    /// The job's id.
    pub job_id: JobId,
    /// The fire's run.
    pub run_id: RunId,
    /// The scheduled time it fired for (RFC 3339, UTC): for `run_now`, when it was asked.
    pub scheduled_for: String,
    /// When it fired.
    pub fired_at: String,
    /// Why.
    pub cause: FireCause,
    /// Opaque: as `job.put` gave it.
    pub loud: bool,
}

impl JobFiredData {
    /// Job `job_id`'s fire as run `run_id`.
    pub fn new(
        job_id: JobId,
        run_id: RunId,
        scheduled_for: String,
        fired_at: String,
        cause: FireCause,
        loud: bool,
    ) -> Self {
        Self {
            job_id,
            run_id,
            scheduled_for,
            fired_at,
            cause,
            loud,
        }
    }
}

/// Why a due fire was not made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The job's last run was still queued or running.
    Overlap,
    /// `job.pause` paused it.
    Paused,
    /// A run of the job was rate-limited, and the limit hasn't reset.
    RateLimited,
    /// No app gate client was connected.
    AppAbsent,
    /// It came due while rlxd was down (or the clock jumped past it), and its `catch_up`
    /// is `skip`.
    Missed,
    /// Its run could not be started: `code` says why (`run.start`'s refusal code, or
    /// `engine_restarted` for a start rlxd stopped in the middle of).
    StartFailed,
    /// Its template routes by `connectors`, and every account that has them all
    /// connected is usage-limited (#15): `connector`, `account` and `resets_at` say
    /// which, as `run.start`'s `connector_account_limited` does. No run is started on
    /// another account.
    ConnectorAccountLimited,
}

impl SkipReason {
    /// Its name on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Overlap => "overlap",
            Self::Paused => "paused",
            Self::RateLimited => "rate_limited",
            Self::AppAbsent => "app_absent",
            Self::Missed => "missed",
            Self::StartFailed => "start_failed",
            Self::ConnectorAccountLimited => "connector_account_limited",
        }
    }
}

/// `job.skipped`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct JobSkippedData {
    /// The job's id.
    pub job_id: JobId,
    /// The scheduled time it skipped (RFC 3339, UTC).
    pub scheduled_for: String,
    /// Why.
    pub reason: SkipReason,
    /// For `start_failed`: the error code the run's start was refused with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub code: Option<String>,
    /// For `connector_account_limited`: the connector its route was refused by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub connector: Option<String>,
    /// For `connector_account_limited`: the account that holds it, limited (the one whose
    /// limit resets soonest, when several do).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, range(min = 1))]
    pub account: Option<u32>,
    /// For `connector_account_limited`: when that account's limit resets, if rlx knows
    /// (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, extend("format" = "date-time"))]
    pub resets_at: Option<String>,
}

impl JobSkippedData {
    /// Job `job_id`'s fire for `scheduled_for`, skipped for `reason`.
    pub fn new(job_id: JobId, scheduled_for: String, reason: SkipReason) -> Self {
        Self {
            job_id,
            scheduled_for,
            reason,
            code: None,
            connector: None,
            account: None,
            resets_at: None,
        }
    }

    /// This skip, its start refused with `code`.
    #[must_use]
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    /// This skip, for `connector`, held by `account` alone while it is limited until
    /// `resets_at`.
    #[must_use]
    pub fn limited(
        mut self,
        connector: impl Into<String>,
        account: u32,
        resets_at: Option<String>,
    ) -> Self {
        self.connector = Some(connector.into());
        self.account = Some(account);
        self.resets_at = resets_at;
        self
    }
}

/// `job.watch_result`: a watch job's call's raw result, from the fire's run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct JobWatchResultData {
    /// The job's id.
    pub job_id: JobId,
    /// The fire's run, as `job.fired` named it.
    pub run_id: RunId,
    /// The tool called.
    pub tool: String,
    /// The SHA-256 of the call's input as RFC 8785 JSON: the job's pinned call.
    pub input_sha256: String,
    /// The call's result, exactly as Claude Code gave it, up to 1 MiB of JSON; past that,
    /// its first 1 MiB as a string (`truncated`).
    pub content: Value,
    /// Whether the call failed: then `content` is its error, not a result to match on.
    pub is_error: bool,
    /// Whether `content` was cut: then it is no result to match on either.
    pub truncated: bool,
}

impl JobWatchResultData {
    /// Job `job_id`'s call of `tool` (input `input_sha256`) in run `run_id`, which gave
    /// `content`.
    pub fn new(
        job_id: JobId,
        run_id: RunId,
        tool: impl Into<String>,
        input_sha256: impl Into<String>,
        content: Value,
    ) -> Self {
        Self {
            job_id,
            run_id,
            tool: tool.into(),
            input_sha256: input_sha256.into(),
            content,
            is_error: false,
            truncated: false,
        }
    }
}

/// One event of the `job` topic: an event (event.json) whose data, for each job event
/// type 7.6 describes, carries that type's fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobEvent(pub Value);

impl JsonSchema for JobEvent {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("JobEvent")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let reference = generator.subschema_for::<JobRef>();
        let typed = [
            (job_types::PUT, reference.clone()),
            (job_types::DELETED, reference.clone()),
            (job_types::PAUSED, reference.clone()),
            (job_types::RESUMED, reference),
            (job_types::FIRED, generator.subschema_for::<JobFiredData>()),
            (
                job_types::SKIPPED,
                generator.subschema_for::<JobSkippedData>(),
            ),
            (
                job_types::WATCH_RESULT,
                generator.subschema_for::<JobWatchResultData>(),
            ),
        ];
        let mut rules = vec![Value::from(generator.subschema_for::<crate::Event>())];
        rules.extend(typed.into_iter().map(|(r#type, data)| {
            json!({
                "if": {"properties": {"type": {"const": r#type}}, "required": ["type"]},
                "then": {"properties": {"data": data}},
            })
        }));
        json_schema!({
            "description": "One event of the job topic: an event (event.json) whose data, \
                            for each job event type 7.6 describes, carries that type's \
                            fields.",
            "allOf": rules,
        })
    }
}

/// `debug.clock_advance`'s params (test builds only): move the scheduler's test clock on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClockAdvanceParams {
    /// Seconds to move it on by, 1 to 31,622,400 (a leap year).
    #[schemars(range(min = 1, max = 31_622_400))]
    pub seconds: u64,
}

/// `debug.clock_set`'s params (test builds only): set the scheduler's test clock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClockSetParams {
    /// The instant (RFC 3339); it may not go back.
    pub at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_id_keeps_its_rule() {
        assert!(JobId::new("slack-digest").is_ok());
        assert!(JobId::new("a:b.c_d-1").is_ok());
        for bad in ["", "-x", "a b", "é", &"x".repeat(129)] {
            assert!(JobId::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_put_reads_both_schedules_and_refuses_unknown_fields() {
        let every: JobPutParams = serde_json::from_value(json!({
            "job_id": "t1", "type": "run", "schedule": {"every_minutes": 1},
            "run": {"kind": "task", "prompt": "hi", "mount": {"kind": "no_tools"}},
            "catch_up": "skip"}))
        .unwrap();
        assert!(every.enabled && !every.loud);
        assert_eq!(every.schedule, Schedule::Every { every_minutes: 1 });
        let cron: JobPutParams = serde_json::from_value(json!({
            "job_id": "t2", "type": "run", "schedule": {"cron": "* * * * *", "tz": "UTC"},
            "run": {"kind": "task", "mount": {"kind": "no_tools"}}, "catch_up": "once",
            "loud": true}))
        .unwrap();
        assert!(matches!(cron.schedule, Schedule::Cron { .. }));
        assert!(
            serde_json::from_value::<JobPutParams>(json!({
                "job_id": "t3", "type": "run", "schedule": {"every_minutes": 1},
                "run": {}, "catch_up": "skip", "run_when": "always"}))
            .is_err(),
            "an unknown field"
        );
        assert!(
            serde_json::from_value::<JobPutParams>(json!({
                "job_id": "t3", "type": "run", "schedule": {"every_minutes": 1, "tz": "UTC"},
                "run": {}, "catch_up": "skip"}))
            .is_err(),
            "a schedule of neither shape"
        );
    }

    #[test]
    fn a_template_is_run_starts_params_without_run_id() {
        let template = RunTemplate(
            json!({"kind": "task", "prompt": "hi", "mount": {"kind": "no_tools"}})
                .as_object()
                .unwrap()
                .clone(),
        );
        let id = RunId::new("0f8e2d6c-3b1a-4c5d-9e7f-1a2b3c4d5e6f").unwrap();
        assert_eq!(template.start(&id).unwrap().run_id, id);
        let mut with_id = template.clone();
        with_id.0.insert("run_id".to_owned(), json!(id.to_string()));
        assert_eq!(with_id.start(&id).unwrap_err().field(), "run.run_id");
        let mut bad_space = template.clone();
        bad_space.0.insert("space".to_owned(), json!("../x"));
        assert!(bad_space.start(&id).is_err());
    }
}
