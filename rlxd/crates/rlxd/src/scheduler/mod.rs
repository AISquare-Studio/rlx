//! The scheduler (handoff 0.10.3, 2.3 and 7.6): rlxapp's jobs, which rlxd stores and
//! fires, keeping the record of each fire.
//!
//! - **Definitions are the app's.** `job.put` creates or replaces a job by `job_id`;
//!   rlxd keeps it in `<home>/rlxd/rlxd.db` ([`db`]) only to fire it. A run job's
//!   template is checked as `run.start` checks its params, at `job.put`.
//! - **When a job is due** comes from its schedule ([`schedule`]): `every_minutes` on a
//!   grid from `job.put`, or cron in an IANA time zone, with DST's gaps and folds firing
//!   once.
//! - **Whether it fires** is [`decide`]'s, the one place each rule is: paused, an app gate
//!   client connected (2.3), never overlapping itself, a run's rate limit, and `catch_up`.
//! - **A fire is a run** started through the runner, with the job's id and `loud`, and a
//!   fresh `run_id`; `job.fired` names it. A skip is `job.skipped {reason}`.
//! - **Fires it couldn't make** are decided once, however many: rlxd down, the clock
//!   jumping, or no app. `catch_up: once` makes one fire when it can; `skip` waits.
//! - **The clock** is the system's, or in test builds a manual one ([`clock`]).

mod clock;
mod db;
mod decide;
mod schedule;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use rlxd_client::Home;
use rlxd_proto::{
    Data, ErrorBody, FireCause, JobFiredData, JobId, JobListData, JobPutParams, JobRef,
    JobRunNowData, JobSkippedData, JobSummary, JobType, Jobs, RunId, RunState, SkipReason, codes,
    job_types, topics,
};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{Notify, mpsc, watch};

use self::clock::Clock;
use self::decide::{Cause, Decision, Moment, Presence, RunNowRefused};
use self::schedule::Plan;
use crate::bus::Bus;
use crate::log::{info, warning};
use crate::runner::{JobTag, RunNote, Runner};

/// How late a due fire may be decided and still count as on time: later, it was missed
/// (rlxd was down, or the clock jumped), and `catch_up` decides it.
const LATE: SignedDuration = SignedDuration::from_secs(60);

/// How long, after rlxd starts, a fire missed while it was down waits for an app gate
/// client to connect before it is recorded `app_absent`.
const STARTUP_WAIT: SignedDuration = SignedDuration::from_secs(10);

/// How long a run's rate limit holds a job's fires when it gave no reset time.
const RATE_LIMIT_HOLD: SignedDuration = SignedDuration::from_mins(5);

/// The longest the loop sleeps on the system's clock before it looks again.
const LONGEST_SLEEP: Duration = Duration::from_secs(30);

/// The client name a job's runs carry, in `run.list`.
const CLIENT: &str = "scheduler";

/// The scheduler.
pub struct Scheduler {
    bus: Arc<Bus>,
    runner: Arc<Runner>,
    db: db::Db,
    /// Where a database it could not read was moved, and why.
    set_aside: Option<String>,
    clock: Clock,
    jobs: Mutex<BTreeMap<JobId, Job>>,
    /// App gate clients connected, as the gate counts them.
    apps: watch::Receiver<u32>,
    /// Wakes the loop: a job changed.
    wake: Notify,
    stopping: AtomicBool,
}

/// A job as the scheduler holds it.
#[derive(Clone, Debug)]
struct Job {
    def: JobPutParams,
    plan: Plan,
    /// An `every_minutes` grid's origin: the put time.
    anchor: Timestamp,
    /// When it is due next; `None` when it isn't enabled.
    next: Option<Timestamp>,
    last_scheduled_for: Option<Timestamp>,
    paused: bool,
    /// The scheduled time of a fire skipped `app_absent`, owed when an app connects.
    owed: Option<Timestamp>,
    /// Until when a run's rate limit holds its fires.
    held_until: Option<Timestamp>,
    last_fire_at: Option<Timestamp>,
    last_run_id: Option<RunId>,
    last_state: Option<RunState>,
    skipped_count: u64,
    put_at: Timestamp,
    /// A fire missed while rlxd was down waits until then for an app (not stored).
    startup_wait: Option<Timestamp>,
}

impl Scheduler {
    /// The scheduler of `home`, its jobs read back from `rlxd.db`, following `apps`.
    pub fn new(
        home: &Home,
        bus: Arc<Bus>,
        runner: Arc<Runner>,
        apps: watch::Receiver<u32>,
    ) -> Result<Arc<Self>, String> {
        let opened = db::open(&home.root().join("rlxd"))?;
        let clock = Clock::from_env();
        if clock.is_manual() {
            info!("scheduler: on a test clock, at {}", clock.now());
        }
        let scheduler = Arc::new(Self {
            bus,
            runner,
            db: opened.db,
            set_aside: opened.set_aside,
            clock,
            jobs: Mutex::new(BTreeMap::new()),
            apps,
            wake: Notify::new(),
            stopping: AtomicBool::new(false),
        });
        scheduler.load()?;
        Ok(scheduler)
    }

    /// Reads the jobs back. A fire one missed while rlxd was down waits a little for an
    /// app ([`STARTUP_WAIT`]).
    fn load(&self) -> Result<(), String> {
        let now = self.clock.now();
        let mut jobs = self.jobs();
        for row in self.db.jobs()? {
            match Job::from_row(&row) {
                Ok(mut job) => {
                    if job.next.is_some_and(|next| next <= now) {
                        job.startup_wait = Some(now.saturating_add(STARTUP_WAIT).unwrap_or(now));
                    }
                    jobs.insert(job.def.job_id.clone(), job);
                }
                Err(why) => warning!("scheduler: job {}: left out: {why}", row.job_id),
            }
        }
        info!("scheduler: {} jobs", jobs.len());
        Ok(())
    }

    fn jobs(&self) -> MutexGuard<'_, BTreeMap<JobId, Job>> {
        self.jobs.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The loop: decides what is due, then sleeps until the next job is, a job changes,
    /// an app connects or leaves, or a job's run says something. It ends once rlxd stops.
    pub async fn run(self: Arc<Self>, mut notes: mpsc::UnboundedReceiver<RunNote>) {
        let mut apps = self.apps.clone();
        let mut present = *apps.borrow() > 0;
        loop {
            if self.stopping.load(Ordering::SeqCst) {
                return;
            }
            let this = Arc::clone(&self);
            blocking(move || this.tick());
            let sleep = self.sleep();
            tokio::select! {
                () = tokio::time::sleep(sleep) => {}
                () = self.wake.notified() => {}
                changed = apps.changed() => {
                    if changed.is_err() {
                        // The gate is gone: rlxd stops.
                        return;
                    }
                    let now_present = *apps.borrow_and_update() > 0;
                    if now_present && !present {
                        let this = Arc::clone(&self);
                        blocking(move || this.app_connected());
                    }
                    present = now_present;
                }
                note = notes.recv() => {
                    let Some(note) = note else { return };
                    let this = Arc::clone(&self);
                    blocking(move || this.noted(note));
                }
            }
        }
    }

    /// How long the loop may sleep: until the next job is due, at most [`LONGEST_SLEEP`];
    /// on a test clock, until something wakes it.
    fn sleep(&self) -> Duration {
        if self.clock.is_manual() {
            return Duration::from_secs(3600);
        }
        let now = self.clock.now();
        let next = self
            .jobs()
            .values()
            .filter_map(|job| job.startup_wait.or(job.next))
            .min();
        match next {
            Some(next) if next > now => Duration::try_from(next.duration_since(now))
                .unwrap_or(LONGEST_SLEEP)
                .min(LONGEST_SLEEP),
            Some(_) => Duration::from_millis(50),
            None => LONGEST_SLEEP,
        }
    }

    /// Stops firing: rlxd stops.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    fn presence(&self) -> Presence {
        if *self.apps.borrow() > 0 {
            Presence::App
        } else {
            Presence::Absent
        }
    }

    /// `status`'s `jobs`.
    pub fn status(&self) -> Jobs {
        let mut jobs = Jobs::default();
        jobs.count = u64::try_from(self.jobs().len()).unwrap_or(u64::MAX);
        if let Some(aside) = &self.set_aside {
            jobs.db = Some("set_aside".to_owned());
            jobs.set_aside = Some(aside.clone());
        }
        jobs
    }

    /// `job.put`: creates or replaces the job. A replace starts its schedule again from
    /// now, and keeps what it has done (its last fire, its skips) and whether it is paused.
    pub fn put(&self, params: JobPutParams) -> Result<JobRef, ErrorBody> {
        params
            .validate()
            .map_err(|invalid| refusal(codes::INVALID_PARAMS, invalid.to_string()))?;
        if params.r#type == JobType::Watch {
            return Err(refusal(
                codes::NOT_IMPLEMENTED,
                "watch jobs are not served yet: they come with the tool gate's pinned calls \
                 (handoff 0.10.3, 5.1). Put run jobs for now."
                    .to_owned(),
            ));
        }
        let plan = Plan::of(&params.schedule).map_err(|why| refusal(codes::INVALID_PARAMS, why))?;
        if let Some(template) = &params.run {
            // run.start's own checks, now rather than at each fire.
            let start = template
                .start(&fresh_run_id())
                .map_err(|invalid| refusal(codes::INVALID_PARAMS, invalid.to_string()))?;
            crate::runner::check_start(&start)?;
        }
        let now = self.clock.now();
        let id = params.job_id.clone();
        let mut jobs = self.jobs();
        let kept = jobs.get(&id).cloned();
        let mut job = Job {
            next: params
                .enabled
                .then(|| plan.next_after(now, Some(now)))
                .flatten(),
            def: params,
            plan,
            anchor: now,
            last_scheduled_for: None,
            paused: false,
            owed: None,
            held_until: None,
            last_fire_at: None,
            last_run_id: None,
            last_state: None,
            skipped_count: 0,
            put_at: now,
            startup_wait: None,
        };
        if let Some(kept) = kept {
            job.paused = kept.paused;
            job.held_until = kept.held_until;
            job.last_scheduled_for = kept.last_scheduled_for;
            job.last_fire_at = kept.last_fire_at;
            job.last_run_id = kept.last_run_id;
            job.last_state = kept.last_state;
            job.skipped_count = kept.skipped_count;
        }
        self.db
            .save(&job.row(now))
            .map_err(|e| refusal(codes::INTERNAL, format!("the job could not be stored: {e}")))?;
        jobs.insert(id.clone(), job);
        drop(jobs);
        self.publish(job_types::PUT, &JobRef::new(id.clone()));
        self.wake.notify_one();
        Ok(JobRef::new(id))
    }

    /// `job.list`.
    pub fn list(&self) -> JobListData {
        let jobs = self.jobs().values().map(Job::summary).collect();
        JobListData { jobs }
    }

    /// `job.delete`.
    pub fn delete(&self, job: &JobRef) -> Result<JobRef, ErrorBody> {
        let mut jobs = self.jobs();
        if jobs.remove(&job.job_id).is_none() {
            return Err(unknown_job(&job.job_id));
        }
        if let Err(e) = self.db.delete(job.job_id.as_str()) {
            warning!(
                "scheduler: job {}: its row was not deleted: {e}",
                job.job_id
            );
        }
        drop(jobs);
        self.publish(job_types::DELETED, job);
        Ok(job.clone())
    }

    /// `job.pause` and `job.resume`.
    pub fn pause(&self, job: &JobRef, paused: bool) -> Result<JobRef, ErrorBody> {
        let now = self.clock.now();
        let mut jobs = self.jobs();
        let held = jobs
            .get_mut(&job.job_id)
            .ok_or_else(|| unknown_job(&job.job_id))?;
        held.paused = paused;
        self.store(held, now);
        drop(jobs);
        let r#type = if paused {
            job_types::PAUSED
        } else {
            job_types::RESUMED
        };
        self.publish(r#type, job);
        self.wake.notify_one();
        Ok(job.clone())
    }

    /// `job.run_now`: fires the job now, which needs an app gate client; `paused` doesn't
    /// stop it, a live run of it or its rate limit does.
    pub fn run_now(&self, job: &JobRef) -> Result<JobRunNowData, ErrorBody> {
        let now = self.clock.now();
        let mut jobs = self.jobs();
        let held = jobs
            .get_mut(&job.job_id)
            .ok_or_else(|| unknown_job(&job.job_id))?;
        let moment = self.moment(held, now, Cause::Due);
        match decide::run_now(&held.view(), &moment) {
            Ok(()) => {}
            Err(RunNowRefused::AppAbsent) => {
                return Err(refusal(
                    codes::APP_ABSENT,
                    "no app gate client is connected: jobs fire only while the app is \
                     there to decide their runs' calls (2.3)"
                        .to_owned(),
                ));
            }
            Err(RunNowRefused::Running) => {
                return Err(refusal(
                    codes::JOB_RUNNING,
                    format!(
                        "job {}'s last run is still queued or running: a job never \
                         overlaps itself",
                        job.job_id
                    ),
                ));
            }
            Err(RunNowRefused::RateLimited(until)) => {
                let mut detail = Data::new();
                detail.insert("resets_at".to_owned(), Value::String(until.to_string()));
                let error = refusal(
                    codes::RATE_LIMITED,
                    format!("job {}'s runs are rate-limited until {until}", job.job_id),
                );
                return Err(error.clone().with_detail(detail).unwrap_or(error));
            }
        }
        let run_id = self.fire(held, now, now, FireCause::RunNow)?;
        Ok(JobRunNowData {
            job_id: job.job_id.clone(),
            run_id,
        })
    }

    /// Moves a test clock to `at` and decides everything due by then before it answers.
    #[cfg(feature = "debug-methods")]
    pub fn clock_to(&self, at: Timestamp) -> Result<Timestamp, ErrorBody> {
        let now = self
            .clock
            .set(at)
            .map_err(|why| refusal(codes::INVALID_PARAMS, why))?;
        self.tick();
        Ok(now)
    }

    /// The test clock's time, or the system's.
    #[cfg(feature = "debug-methods")]
    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Decides every job that is due.
    fn tick(&self) {
        if self.stopping.load(Ordering::SeqCst) {
            return;
        }
        let now = self.clock.now();
        let presence = self.presence();
        let mut jobs = self.jobs();
        for job in jobs.values_mut() {
            let Some(scheduled) = job.next else {
                continue;
            };
            if scheduled > now {
                continue;
            }
            let cause = match job.startup_wait {
                // Missed while rlxd was down: decided once an app is here, or the wait ends.
                Some(until) if presence == Presence::Absent && now < until => continue,
                Some(_) => Cause::Missed,
                None => {
                    let following = job.plan.next_after(scheduled, Some(job.anchor));
                    let late = now.duration_since(scheduled) > LATE
                        || following.is_some_and(|following| following <= now);
                    if late { Cause::Missed } else { Cause::Due }
                }
            };
            job.startup_wait = None;
            let moment = self.moment(job, now, cause);
            let decision = decide::decide(&job.view(), &moment);
            self.apply(job, now, scheduled, decision);
            // On time, the next is the next after it; missed, the next after now, so
            // however many were missed, they were one decision.
            let from = if cause == Cause::Due { scheduled } else { now };
            job.next = job.plan.next_after(from, Some(job.anchor));
            job.last_scheduled_for = Some(scheduled);
            self.store(job, now);
        }
    }

    /// An app gate client connected: each job that owes a fire it skipped `app_absent`
    /// makes it now, once, when its `catch_up` is `once`.
    fn app_connected(&self) {
        let now = self.clock.now();
        let mut jobs = self.jobs();
        for job in jobs.values_mut() {
            let Some(owed) = job.owed.take() else {
                continue;
            };
            let moment = self.moment(job, now, Cause::Owed);
            let decision = decide::decide(&job.view(), &moment);
            self.apply(job, now, owed, decision);
            self.store(job, now);
        }
        drop(jobs);
        // Fires missed while rlxd was down, waiting for an app, are decided now too.
        self.tick();
    }

    /// What a job's run said.
    fn noted(&self, note: RunNote) {
        let now = self.clock.now();
        let mut jobs = self.jobs();
        match note {
            RunNote::RateLimited {
                job_id, resets_at, ..
            } => {
                let Some(job) = JobId::new(job_id).ok().and_then(|id| jobs.get_mut(&id)) else {
                    return;
                };
                let until = resets_at
                    .and_then(|at| at.parse::<Timestamp>().ok())
                    .unwrap_or_else(|| now.saturating_add(RATE_LIMIT_HOLD).unwrap_or(now));
                job.held_until = Some(until);
                self.store(job, now);
            }
            RunNote::Ended {
                job_id,
                run_id,
                state,
            } => {
                let Some(job) = JobId::new(job_id).ok().and_then(|id| jobs.get_mut(&id)) else {
                    return;
                };
                if job.last_run_id.as_ref() == Some(&run_id) {
                    job.last_state = Some(state);
                    self.store(job, now);
                }
            }
        }
    }

    /// The moment `job` is decided at.
    fn moment(&self, job: &Job, now: Timestamp, cause: Cause) -> Moment {
        Moment {
            now,
            cause,
            presence: self.presence(),
            last_run_live: job
                .last_run_id
                .as_ref()
                .is_some_and(|run| self.runner.live(run)),
            held_until: job.held_until,
        }
    }

    /// Carries out `decision` for `job`'s fire scheduled for `scheduled`.
    fn apply(&self, job: &mut Job, now: Timestamp, scheduled: Timestamp, decision: Decision) {
        match decision {
            Decision::Fire(cause) => {
                if let Err(error) = self.fire(job, now, scheduled, cause) {
                    warning!(
                        "scheduler: job {}: its fire for {scheduled} did not start: {}",
                        job.def.job_id,
                        error.message
                    );
                    self.record(
                        job,
                        scheduled,
                        now,
                        "error",
                        Some(error.code.as_str()),
                        None,
                        None,
                    );
                }
            }
            Decision::Skip(reason) => {
                if reason == SkipReason::AppAbsent {
                    job.owed = Some(scheduled);
                }
                job.skipped_count += 1;
                self.record(
                    job,
                    scheduled,
                    now,
                    "skipped",
                    Some(reason.as_str()),
                    None,
                    None,
                );
                self.publish(
                    job_types::SKIPPED,
                    &JobSkippedData::new(job.def.job_id.clone(), scheduled.to_string(), reason),
                );
            }
            Decision::Nothing => {}
        }
    }

    /// Fires `job` for `scheduled`: a run of its template, with a fresh `run_id`, the
    /// job's id and `loud`. `job.fired` names the run.
    fn fire(
        &self,
        job: &mut Job,
        now: Timestamp,
        scheduled: Timestamp,
        cause: FireCause,
    ) -> Result<RunId, ErrorBody> {
        let template =
            job.def.run.as_ref().ok_or_else(|| {
                refusal(codes::NOT_IMPLEMENTED, "only run jobs fire yet".to_owned())
            })?;
        let run_id = fresh_run_id();
        let params = template
            .start(&run_id)
            .map_err(|invalid| refusal(codes::INVALID_PARAMS, invalid.to_string()))?;
        let tag = JobTag {
            job_id: job.def.job_id.to_string(),
            loud: job.def.loud,
        };
        self.runner.start(params, CLIENT, Some(tag))?;
        job.last_fire_at = Some(now);
        job.last_run_id = Some(run_id.clone());
        job.last_state = Some(RunState::Queued);
        job.owed = None;
        let cause_name = match cause {
            FireCause::Due => "due",
            FireCause::CatchUp => "catch_up",
            FireCause::RunNow => "run_now",
        };
        self.record(
            job,
            scheduled,
            now,
            "fired",
            None,
            Some(&run_id),
            Some(cause_name),
        );
        self.publish(
            job_types::FIRED,
            &JobFiredData::new(
                job.def.job_id.clone(),
                run_id.clone(),
                scheduled.to_string(),
                now.to_string(),
                cause,
                job.def.loud,
            ),
        );
        info!(
            "scheduler: job {} fired run {run_id} ({cause_name})",
            job.def.job_id
        );
        Ok(run_id)
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        job: &Job,
        scheduled: Timestamp,
        now: Timestamp,
        outcome: &str,
        reason: Option<&str>,
        run_id: Option<&RunId>,
        cause: Option<&str>,
    ) {
        let fire = db::Fire {
            job_id: job.def.job_id.as_str(),
            scheduled_for: &scheduled.to_string(),
            at: &now.to_string(),
            outcome,
            reason,
            run_id: run_id.map(RunId::as_str),
            cause,
        };
        if let Err(e) = self.db.record(&fire) {
            warning!(
                "scheduler: job {}: its fire was not recorded: {e}",
                job.def.job_id
            );
        }
    }

    /// Writes `job` back to the store.
    fn store(&self, job: &Job, now: Timestamp) {
        if let Err(e) = self.db.save(&job.row(now)) {
            warning!("scheduler: job {}: not stored: {e}", job.def.job_id);
        }
    }

    fn publish(&self, r#type: &str, data: &impl Serialize) {
        match serde_json::to_value(data) {
            Ok(Value::Object(data)) => {
                if let Err(e) = self.bus.publish(topics::JOB, r#type, data) {
                    warning!("scheduler: {type} was not published: {e}");
                }
            }
            other => warning!("scheduler: {type} did not print as an object: {other:?}"),
        }
    }
}

impl Job {
    /// What [`decide`] reads of it.
    fn view(&self) -> decide::Job {
        decide::Job {
            paused: self.paused,
            catch_up: self.def.catch_up,
        }
    }

    fn summary(&self) -> JobSummary {
        let mut summary = JobSummary::new(
            self.def.job_id.clone(),
            self.def.r#type,
            self.def.schedule.clone(),
            self.def.catch_up,
        );
        summary.enabled = self.def.enabled;
        summary.paused = self.paused;
        summary.loud = self.def.loud;
        summary.next_fire_at = self.next.map(|at| at.to_string());
        summary.last_fire_at = self.last_fire_at.map(|at| at.to_string());
        summary.last_run_id.clone_from(&self.last_run_id);
        summary.last_state = self.last_state;
        summary.skipped_count = self.skipped_count;
        summary
    }

    fn row(&self, now: Timestamp) -> db::Row {
        let text = |at: Option<Timestamp>| at.map(|at| at.to_string());
        db::Row {
            job_id: self.def.job_id.to_string(),
            r#type: self.def.r#type.as_str().to_owned(),
            definition: serde_json::to_string(&self.def).unwrap_or_default(),
            anchor_at: self.anchor.to_string(),
            next_fire_at: text(self.next),
            last_scheduled_for: text(self.last_scheduled_for),
            paused: self.paused,
            owed: text(self.owed),
            held_until: text(self.held_until),
            last_fire_at: text(self.last_fire_at),
            last_run_id: self.last_run_id.as_ref().map(ToString::to_string),
            last_state: self.last_state.map(|state| state.as_str().to_owned()),
            skipped_count: self.skipped_count,
            put_at: self.put_at.to_string(),
            updated_at: now.to_string(),
        }
    }

    fn from_row(row: &db::Row) -> Result<Self, String> {
        let def: JobPutParams =
            serde_json::from_str(&row.definition).map_err(|e| format!("its definition: {e}"))?;
        let plan = Plan::of(&def.schedule)?;
        let time = |text: &Option<String>| -> Result<Option<Timestamp>, String> {
            text.as_deref()
                .map(|text| text.parse::<Timestamp>().map_err(|e| e.to_string()))
                .transpose()
        };
        let anchor = row
            .anchor_at
            .parse()
            .map_err(|e: jiff::Error| e.to_string())?;
        let last_state = row
            .last_state
            .as_deref()
            .and_then(|state| serde_json::from_value(Value::String(state.to_owned())).ok());
        Ok(Self {
            next: time(&row.next_fire_at)?,
            plan,
            anchor,
            last_scheduled_for: time(&row.last_scheduled_for)?,
            paused: row.paused,
            owed: time(&row.owed)?,
            held_until: time(&row.held_until)?,
            last_fire_at: time(&row.last_fire_at)?,
            last_run_id: row
                .last_run_id
                .as_deref()
                .and_then(|id| RunId::new(id).ok()),
            last_state,
            skipped_count: row.skipped_count,
            put_at: row.put_at.parse().map_err(|e: jiff::Error| e.to_string())?,
            startup_wait: None,
            def,
        })
    }
}

/// A fresh run id: a random (version 4) UUID, lower case.
fn fresh_run_id() -> RunId {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // No randomness: the time stands in, unique enough for one rlxd.
        let nanos = Timestamp::now().as_nanosecond().unsigned_abs();
        bytes.copy_from_slice(&nanos.to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let id = format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    );
    RunId::new(id).expect("a formatted UUID is a run id")
}

fn refusal(code: &str, message: String) -> ErrorBody {
    ErrorBody::new(code, message).expect("a scheduler refusal has a code and a message")
}

fn unknown_job(job_id: &JobId) -> ErrorBody {
    refusal(codes::UNKNOWN_JOB, format!("rlxd holds no job {job_id}"))
}

/// Runs `work`, which writes the store, as blocking work on rlxd's multi-thread runtime.
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
    use rlxd_proto::CatchUp;

    #[test]
    fn fresh_run_ids_are_version_4_uuids() {
        let a = fresh_run_id();
        let b = fresh_run_id();
        assert_ne!(a, b);
        assert_eq!(&a.as_str()[14..15], "4");
        assert!(matches!(&a.as_str()[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn catch_up_names_its_two_policies() {
        assert_eq!(CatchUp::Once.as_str(), "once");
        assert_eq!(CatchUp::Skip.as_str(), "skip");
    }
}
