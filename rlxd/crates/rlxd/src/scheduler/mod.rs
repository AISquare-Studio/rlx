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
//!   fresh `run_id`; `job.fired` names it. A skip is `job.skipped {reason}`. A run job's
//!   run is its template; a watch job's is its one pinned call, by its route
//!   ([`watch_job`]), whose raw result is `job.watch_result`.
//! - **Fires it couldn't make** are decided once, however many: rlxd down, the clock
//!   jumping, or no app. `catch_up: once` makes one fire when it can; `skip` waits.
//! - **The clock** is the system's, or in test builds a manual one ([`clock`]).

mod clock;
mod db;
mod decide;
mod schedule;
mod watch_job;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use rlxd_client::Home;
use rlxd_proto::{
    Data, ErrorBody, FireCause, JobFiredData, JobId, JobListData, JobPutParams, JobRef,
    JobRunNowData, JobSkippedData, JobSummary, JobType, JobWatchResultData, Jobs, RunId, RunState,
    SkipReason, codes, job_types, topics,
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

/// `job.skipped start_failed`'s code for a fire whose run rlxd stopped in the middle of
/// starting.
const ENGINE_RESTARTED: &str = "engine_restarted";

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
    /// How many jobs it holds, for `status`, which takes no lock.
    count: std::sync::atomic::AtomicU64,
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
    /// The last fire, when it was a `connectors.call` run: its run, when it was
    /// scheduled for and why, to make again by the fallback if the run finds no route
    /// (not stored).
    call_fire: Option<(RunId, Timestamp, FireCause)>,
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
            count: std::sync::atomic::AtomicU64::new(0),
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
        // Fires whose runs were being started when rlxd stopped: fired if the run was
        // made (the runner has its record), else they didn't start. Never fired again.
        for starting in self.db.starting()? {
            let job = JobId::new(starting.job_id.clone())
                .ok()
                .and_then(|id| jobs.get_mut(&id));
            let run = RunId::new(starting.run_id.clone())
                .ok()
                .and_then(|id| self.runner.state(&id).map(|state| (id, state)));
            let scheduled: Timestamp = starting.scheduled_for.parse().unwrap_or(now);
            let at: Timestamp = starting.at.parse().unwrap_or(now);
            // A row left unsettled behind a later fire (a settle that failed) is the past:
            // settled quietly, the job's last fire left as it is.
            let stale = |job: &Job| job.last_fire_at.is_some_and(|last| last >= at);
            match (job, run) {
                (Some(job), Some(_)) if stale(job) => self.settle(starting.id, "fired", None, None),
                (Some(job), None) if stale(job) => self.settle(
                    starting.id,
                    "skipped",
                    Some(SkipReason::StartFailed.as_str()),
                    Some(ENGINE_RESTARTED),
                ),
                (Some(job), Some((run_id, state))) => {
                    self.settle(starting.id, "fired", None, None);
                    job.last_fire_at = Some(at);
                    job.last_run_id = Some(run_id.clone());
                    job.last_state = Some(state);
                    job.owed = None;
                    let cause = match starting.cause.as_deref() {
                        Some("catch_up") => FireCause::CatchUp,
                        Some("run_now") => FireCause::RunNow,
                        _ => FireCause::Due,
                    };
                    self.publish(
                        job_types::FIRED,
                        &JobFiredData::new(
                            job.def.job_id.clone(),
                            run_id,
                            scheduled.to_string(),
                            starting.at.clone(),
                            cause,
                            job.def.loud,
                        ),
                    );
                    self.store(job, now);
                }
                (Some(job), None) => {
                    self.settle(
                        starting.id,
                        "skipped",
                        Some(SkipReason::StartFailed.as_str()),
                        Some(ENGINE_RESTARTED),
                    );
                    job.skipped_count += 1;
                    self.publish(
                        job_types::SKIPPED,
                        &JobSkippedData::new(
                            job.def.job_id.clone(),
                            scheduled.to_string(),
                            SkipReason::StartFailed,
                        )
                        .with_code(ENGINE_RESTARTED),
                    );
                    self.store(job, now);
                }
                (None, _) => self.settle(starting.id, "skipped", None, None),
            }
        }
        // Each job's last run as it ended, the runs a previous rlxd left included.
        for job in jobs.values_mut() {
            if let Some(state) = job
                .last_run_id
                .as_ref()
                .and_then(|id| self.runner.state(id))
                && job.last_state != Some(state)
            {
                job.last_state = Some(state);
                self.store(job, now);
            }
        }
        self.count_of(&jobs);
        info!("scheduler: {} jobs", jobs.len());
        Ok(())
    }

    /// Notes how many jobs there are, for `status`.
    fn count_of(&self, jobs: &BTreeMap<JobId, Job>) {
        self.count.store(
            u64::try_from(jobs.len()).unwrap_or(u64::MAX),
            Ordering::SeqCst,
        );
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
        jobs.count = self.count.load(Ordering::SeqCst);
        if let Some(aside) = &self.set_aside {
            jobs.db = Some("set_aside".to_owned());
            jobs.set_aside = Some(aside.clone());
        }
        jobs
    }

    /// `job.put`: creates or replaces the job, idempotently: the app pushes its jobs again
    /// on each connect. The same definition again changes nothing. A replace starts the
    /// schedule again from now only when its `schedule` changes or it is enabled again;
    /// else it keeps where its schedule is, a fire it owes, and a missed fire waiting for
    /// an app. A replace keeps what the job has done (its last fire, its skips) and whether
    /// it is paused.
    pub fn put(&self, params: JobPutParams) -> Result<JobRef, ErrorBody> {
        params
            .validate()
            .map_err(|invalid| refusal(codes::INVALID_PARAMS, invalid.to_string()))?;
        if params.r#type == JobType::Watch {
            watch_job::check(&params).map_err(|why| refusal(codes::INVALID_PARAMS, why))?;
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
        let job = match jobs.get(&id) {
            // The same definition again: nothing changes.
            Some(kept) if kept.def == params => None,
            Some(kept) => {
                // The schedule as it fires, not as it is written: a tz alias or a cron
                // respelled is no change.
                let again = !kept.plan.same_as(&plan, now) || (!kept.def.enabled && params.enabled);
                let mut job = kept.clone();
                if again {
                    job.anchor = now;
                    job.next = params
                        .enabled
                        .then(|| plan.next_after(now, Some(now)))
                        .flatten();
                    job.owed = None;
                    job.startup_wait = None;
                    job.last_scheduled_for = None;
                } else if !params.enabled {
                    // Disabled: nothing due, owed or waiting.
                    job.next = None;
                    job.owed = None;
                    job.startup_wait = None;
                }
                job.plan = plan;
                job.def = params;
                job.put_at = now;
                Some(job)
            }
            None => Some(Job {
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
                call_fire: None,
            }),
        };
        if let Some(job) = job {
            self.db.save(&job.row(now)).map_err(|e| {
                refusal(codes::INTERNAL, format!("the job could not be stored: {e}"))
            })?;
            jobs.insert(id.clone(), job);
            self.count_of(&jobs);
        }
        drop(jobs);
        self.publish(job_types::PUT, &JobRef::new(id.clone()));
        self.wake.notify_one();
        Ok(JobRef::new(id))
    }

    /// `job.list`.
    pub fn list(&self) -> JobListData {
        let jobs = self
            .jobs()
            .values()
            .map(|job| {
                let mut summary = job.summary();
                // The last run's state as the runner has it now: running, or how it ended.
                if let Some(state) = job
                    .last_run_id
                    .as_ref()
                    .and_then(|id| self.runner.state(id))
                {
                    summary.last_state = Some(state);
                }
                summary
            })
            .collect();
        JobListData { jobs }
    }

    /// `job.delete`.
    pub fn delete(&self, job: &JobRef) -> Result<JobRef, ErrorBody> {
        let mut jobs = self.jobs();
        if jobs.remove(&job.job_id).is_none() {
            return Err(unknown_job(&job.job_id));
        }
        self.count_of(&jobs);
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
            Err(RunNowRefused::Disabled) => {
                return Err(refusal(
                    codes::JOB_DISABLED,
                    format!("job {} is disabled (enabled: false)", job.job_id),
                ));
            }
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
            // The clock went back (NTP, or set by hand): counted from now, the job is due
            // sooner than its next, so its next is counted from now again, and it doesn't
            // go quiet for as long as the clock moved.
            if job.def.enabled {
                let fresh = job.plan.next_after(now, Some(job.anchor));
                if let (Some(fresh), Some(next)) = (fresh, job.next)
                    && fresh < next
                    && next > now
                {
                    job.next = Some(fresh);
                    job.last_scheduled_for = None;
                    self.store(job, now);
                }
            }
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
            // On time, the next is the next after it; missed, the next after now, so
            // however many were missed, they were one decision. Moved before the fire, and
            // stored with it before its run starts ([`db::Db::begin_fire`]).
            let from = if cause == Cause::Due { scheduled } else { now };
            job.next = job.plan.next_after(from, Some(job.anchor));
            job.last_scheduled_for = Some(scheduled);
            let moment = self.moment(job, now, cause);
            let decision = decide::decide(&job.view(), &moment);
            self.apply(job, now, scheduled, decision);
            self.store(job, now);
        }
    }

    /// An app gate client connected: each job that owes a fire it skipped `app_absent`
    /// makes it now, once, when its `catch_up` is `once`.
    fn app_connected(&self) {
        let now = self.clock.now();
        let mut jobs = self.jobs();
        for job in jobs.values_mut() {
            let Some(owed) = job.owed else {
                continue;
            };
            let moment = self.moment(job, now, Cause::Owed);
            let decision = decide::decide(&job.view(), &moment);
            if decision == Decision::Nothing && moment.presence == Presence::Absent {
                // The app left again before this: still owed, for when it is back.
                continue;
            }
            job.owed = None;
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
                job_id,
                resets_at,
                status,
                ..
            } => {
                // Only a limit that is hit holds the job's fires: a warning (Claude Code's
                // `allowed_warning`) lets runs go on (manager #2687).
                if !limit_hit(status.as_deref()) {
                    return;
                }
                let Some(job) = JobId::new(job_id).ok().and_then(|id| jobs.get_mut(&id)) else {
                    return;
                };
                let until = resets_at
                    .and_then(|at| at.parse::<Timestamp>().ok())
                    .unwrap_or_else(|| now.saturating_add(RATE_LIMIT_HOLD).unwrap_or(now));
                job.held_until = Some(until);
                self.store(job, now);
            }
            RunNote::WatchResult {
                job_id,
                run_id,
                tool,
                input_sha256,
                content,
                is_error,
                truncated,
            } => {
                let Ok(job_id) = JobId::new(job_id) else {
                    return;
                };
                let mut data = JobWatchResultData::new(job_id, run_id, tool, input_sha256, content);
                data.is_error = is_error;
                data.truncated = truncated;
                self.publish(job_types::WATCH_RESULT, &data);
            }
            RunNote::NoRoute { job_id, run_id } => {
                let Some(job) = JobId::new(job_id).ok().and_then(|id| jobs.get_mut(&id)) else {
                    return;
                };
                let Some((scheduled, cause)) = job
                    .call_fire
                    .take()
                    .filter(|(fired, ..)| *fired == run_id)
                    .map(|(_, scheduled, cause)| (scheduled, cause))
                else {
                    return;
                };
                // The same fire, again, by the fallback (5.3): the run that found no
                // route has made no call.
                info!(
                    "scheduler: job {}: run {run_id} found no route to connectors.call; its \
                     fire goes by the fallback",
                    job.def.job_id
                );
                let _ = self.fire_by(job, now, scheduled, cause, watch_job::Route::Fallback);
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
                // A fire that doesn't start is job.skipped start_failed, from fire().
                let _ = self.fire(job, now, scheduled, cause);
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
    /// job's id and `loud`. The job (its schedule already moved past this fire) and a
    /// `starting` record of the fire are stored together first, so a crash while the run
    /// starts can't fire the same time again ([`Scheduler::load`] settles it). `job.fired`
    /// names the run; a run that can't start is `job.skipped {reason: start_failed, code}`.
    fn fire(
        &self,
        job: &mut Job,
        now: Timestamp,
        scheduled: Timestamp,
        cause: FireCause,
    ) -> Result<RunId, ErrorBody> {
        let route = watch_job::route(self.runner.calls_connectors());
        self.fire_by(job, now, scheduled, cause, route)
    }

    /// [`Scheduler::fire`], a watch job's call made by `route`.
    fn fire_by(
        &self,
        job: &mut Job,
        now: Timestamp,
        scheduled: Timestamp,
        cause: FireCause,
        route: watch_job::Route,
    ) -> Result<RunId, ErrorBody> {
        let cause_name = match cause {
            FireCause::Due => "due",
            FireCause::CatchUp => "catch_up",
            FireCause::RunNow => "run_now",
        };
        let run_id = fresh_run_id();
        let begun = start_params(&job.def, &run_id, route).and_then(|params| {
            let fire = db::Fire {
                job_id: job.def.job_id.as_str(),
                scheduled_for: &scheduled.to_string(),
                at: &now.to_string(),
                outcome: "starting",
                reason: None,
                code: None,
                run_id: Some(run_id.as_str()),
                cause: Some(cause_name),
            };
            let id = self.db.begin_fire(&job.row(now), &fire).map_err(|e| {
                refusal(
                    codes::INTERNAL,
                    format!("the fire could not be stored: {e}"),
                )
            })?;
            Ok((id, params))
        });
        job.call_fire = None;
        let (id, started) = match begun {
            Ok((id, params)) => {
                let tag = JobTag {
                    job_id: job.def.job_id.to_string(),
                    loud: job.def.loud,
                    watch: job.def.r#type == JobType::Watch,
                };
                let started = match (test_refusal(), params) {
                    (Some(code), _) => Err(refusal(
                        &code,
                        "refused by RLXD_TEST_REFUSE_FIRES".to_owned(),
                    )),
                    (None, watch_job::Start::Run(params)) => {
                        self.runner.start(*params, CLIENT, Some(tag))
                    }
                    (None, watch_job::Start::Call(call)) => {
                        let started = self.runner.start_call(call, CLIENT, Some(tag));
                        if started.is_ok() {
                            job.call_fire = Some((run_id.clone(), scheduled, cause));
                        }
                        started
                    }
                };
                test_crash_after_start(started.is_ok());
                (Some(id), started)
            }
            Err(error) => (None, Err(error)),
        };
        match started {
            Ok(_) => {
                if let Some(id) = id {
                    self.settle(id, "fired", None, None);
                }
                job.last_fire_at = Some(now);
                job.last_run_id = Some(run_id.clone());
                job.last_state = Some(RunState::Queued);
                job.owed = None;
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
            Err(error) => {
                warning!(
                    "scheduler: job {}: its fire for {scheduled} did not start ({}): {}",
                    job.def.job_id,
                    error.code,
                    error.message
                );
                let reason = SkipReason::StartFailed.as_str();
                match id {
                    Some(id) => self.settle(id, "skipped", Some(reason), Some(&error.code)),
                    None => {
                        let fire = db::Fire {
                            job_id: job.def.job_id.as_str(),
                            scheduled_for: &scheduled.to_string(),
                            at: &now.to_string(),
                            outcome: "skipped",
                            reason: Some(reason),
                            code: Some(&error.code),
                            run_id: None,
                            cause: Some(cause_name),
                        };
                        if let Err(e) = self.db.record(&fire) {
                            warning!(
                                "scheduler: job {}: its fire was not recorded: {e}",
                                job.def.job_id
                            );
                        }
                    }
                }
                job.skipped_count += 1;
                self.publish(
                    job_types::SKIPPED,
                    &JobSkippedData::new(
                        job.def.job_id.clone(),
                        scheduled.to_string(),
                        SkipReason::StartFailed,
                    )
                    .with_code(error.code.clone()),
                );
                Err(error)
            }
        }
    }

    /// Settles fire record `id`, logging a failure.
    fn settle(&self, id: i64, outcome: &str, reason: Option<&str>, code: Option<&str>) {
        if let Err(e) = self.db.settle(id, outcome, reason, code) {
            warning!("scheduler: fire record {id} was not settled: {e}");
        }
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
            code: None,
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
            enabled: self.def.enabled,
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
            call_fire: None,
            def,
        })
    }
}

/// Whether a `rate_limit_event`'s `status` says the limit is hit: anything but Claude
/// Code's `allowed` and `allowed_warning`, which let runs go on. No status says nothing.
fn limit_hit(status: Option<&str>) -> bool {
    status.is_some_and(|status| !status.starts_with("allowed"))
}

/// Test builds only: `RLXD_TEST_REFUSE_FIRES=<code>` refuses each fire's start with
/// `code`, as the runner would.
#[cfg(feature = "debug-methods")]
fn test_refusal() -> Option<String> {
    std::env::var("RLXD_TEST_REFUSE_FIRES")
        .ok()
        .filter(|code| !code.is_empty())
}

#[cfg(not(feature = "debug-methods"))]
fn test_refusal() -> Option<String> {
    None
}

/// Test builds only: with `RLXD_TEST_CRASH_AFTER_FIRE` set, rlxd exits at once after a
/// fire's run has started and before the scheduler has noted it: a crash in that window.
fn test_crash_after_start(started: bool) {
    #[cfg(feature = "debug-methods")]
    if started && std::env::var("RLXD_TEST_CRASH_AFTER_FIRE").is_ok_and(|crash| !crash.is_empty()) {
        crate::log::warning!("scheduler: RLXD_TEST_CRASH_AFTER_FIRE: exiting mid-fire");
        std::process::exit(70);
    }
    #[cfg(not(feature = "debug-methods"))]
    let _ = started;
}

/// What a fire of the job `def` starts as `run_id`: a run job's template, or a watch
/// job's call, by `route` ([`watch_job::start`]).
fn start_params(
    def: &JobPutParams,
    run_id: &RunId,
    route: watch_job::Route,
) -> Result<watch_job::Start, ErrorBody> {
    match def.r#type {
        JobType::Run => def
            .run
            .as_ref()
            .ok_or_else(|| refusal(codes::INVALID_PARAMS, "a run job needs its run".to_owned()))?
            .start(run_id)
            .map(|params| watch_job::Start::Run(Box::new(params)))
            .map_err(|invalid| refusal(codes::INVALID_PARAMS, invalid.to_string())),
        JobType::Watch => watch_job::start(def, run_id, route),
    }
}

/// A fresh run id: a random (version 4) UUID, lower case.
pub(crate) fn fresh_run_id() -> RunId {
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
