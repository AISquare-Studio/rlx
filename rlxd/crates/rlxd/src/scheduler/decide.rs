//! Whether a job fires: the one place every fire is decided (handoff 0.10.3, 2.3 and 7.6),
//! a pure function of the job and the moment, so each rule is a unit test.
//!
//! The app-absent rule is [`app_policy`] alone. Today a job with no app gate client
//! connected is skipped `app_absent` (2.3). The app-absent policy (#26) adds a job's
//! `run_when: always` and `allowed_tools` there, which fire such a job under a static
//! policy instead: an addition to [`app_policy`] and the [`Job`] it reads, not a rewrite.

use jiff::Timestamp;
use rlxd_proto::{CatchUp, FireCause, SkipReason};

/// What [`decide`] reads of a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Job {
    pub paused: bool,
    pub catch_up: CatchUp,
}

/// Whether an app gate client is connected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    App,
    Absent,
}

/// Why a fire is being decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// It came due, on time.
    Due,
    /// It came due while rlxd was down or the clock jumped past it: one or more scheduled
    /// times, decided once.
    Missed,
    /// An app gate client connected, and the job owes a fire it skipped `app_absent`.
    Owed,
}

/// The moment a fire is decided at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Moment {
    pub now: Timestamp,
    pub cause: Cause,
    pub presence: Presence,
    /// Whether the job's last run is still queued or running.
    pub last_run_live: bool,
    /// Until when a run of the job was rate-limited.
    pub held_until: Option<Timestamp>,
}

/// What to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Fire, for this cause.
    Fire(FireCause),
    /// Record `job.skipped` for this reason.
    Skip(SkipReason),
    /// Nothing: an owed fire whose `catch_up` is `skip`, or that can't be made now, waits
    /// for the next scheduled time.
    Nothing,
}

/// What the app's presence allows: the app-absent rule, in one place (#26 adds to it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppPolicy {
    /// Fire as usual: the app is there to decide the run's gated calls.
    Fire,
    /// Don't fire: `app_absent`.
    Skip,
}

/// Whether `job` may fire with the app's `presence` (2.3: only while an app gate client is
/// connected).
pub fn app_policy(_job: &Job, presence: Presence) -> AppPolicy {
    match presence {
        Presence::App => AppPolicy::Fire,
        Presence::Absent => AppPolicy::Skip,
    }
}

/// Whether `job` fires at `moment`. In order: paused, the app's presence (so a missed fire
/// with no app is `app_absent`, as 7.6 has it), overlap, the rate limit, then `catch_up`
/// for a fire it couldn't make on time.
pub fn decide(job: &Job, moment: &Moment) -> Decision {
    if moment.cause == Cause::Owed {
        // An owed fire is made once, when it can be, and only for `once`; else it is let
        // go, and the next scheduled time comes as usual.
        if job.catch_up == CatchUp::Skip
            || job.paused
            || app_policy(job, moment.presence) == AppPolicy::Skip
        {
            return Decision::Nothing;
        }
    }
    if job.paused {
        return Decision::Skip(SkipReason::Paused);
    }
    if app_policy(job, moment.presence) == AppPolicy::Skip {
        return Decision::Skip(SkipReason::AppAbsent);
    }
    if moment.last_run_live {
        return Decision::Skip(SkipReason::Overlap);
    }
    if moment.held_until.is_some_and(|until| until > moment.now) {
        return Decision::Skip(SkipReason::RateLimited);
    }
    match (moment.cause, job.catch_up) {
        (Cause::Due, _) => Decision::Fire(FireCause::Due),
        (Cause::Missed | Cause::Owed, CatchUp::Once) => Decision::Fire(FireCause::CatchUp),
        (Cause::Missed, CatchUp::Skip) => Decision::Skip(SkipReason::Missed),
        (Cause::Owed, CatchUp::Skip) => Decision::Nothing,
    }
}

/// Why `job.run_now` is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunNowRefused {
    /// No app gate client: `app_absent`.
    AppAbsent,
    /// The job's last run is live: `job_running`.
    Running,
    /// A run of the job was rate-limited until then: `rate_limited`.
    RateLimited(Timestamp),
}

/// Whether `job.run_now` fires `job` now. It is an explicit act, so `paused` doesn't stop
/// it; the app's presence, overlap and the rate limit do.
pub fn run_now(job: &Job, moment: &Moment) -> Result<(), RunNowRefused> {
    if app_policy(job, moment.presence) == AppPolicy::Skip {
        return Err(RunNowRefused::AppAbsent);
    }
    if moment.last_run_live {
        return Err(RunNowRefused::Running);
    }
    match moment.held_until {
        Some(until) if until > moment.now => Err(RunNowRefused::RateLimited(until)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(paused: bool, catch_up: CatchUp) -> Job {
        Job { paused, catch_up }
    }

    fn moment(cause: Cause, presence: Presence) -> Moment {
        Moment {
            now: "2027-01-01T00:00:00Z".parse().unwrap(),
            cause,
            presence,
            last_run_live: false,
            held_until: None,
        }
    }

    #[test]
    fn a_due_fire_fires_with_the_app_and_is_skipped_without_it() {
        let quiet = job(false, CatchUp::Skip);
        assert_eq!(
            decide(&quiet, &moment(Cause::Due, Presence::App)),
            Decision::Fire(FireCause::Due)
        );
        assert_eq!(
            decide(&quiet, &moment(Cause::Due, Presence::Absent)),
            Decision::Skip(SkipReason::AppAbsent)
        );
    }

    #[test]
    fn the_rules_come_in_order_paused_app_overlap_rate_limit() {
        let mut all = moment(Cause::Due, Presence::Absent);
        all.last_run_live = true;
        all.held_until = Some("2027-01-01T00:05:00Z".parse().unwrap());
        let paused = job(true, CatchUp::Once);
        assert_eq!(decide(&paused, &all), Decision::Skip(SkipReason::Paused));
        let running = job(false, CatchUp::Once);
        assert_eq!(
            decide(&running, &all),
            Decision::Skip(SkipReason::AppAbsent)
        );
        all.presence = Presence::App;
        assert_eq!(decide(&running, &all), Decision::Skip(SkipReason::Overlap));
        all.last_run_live = false;
        assert_eq!(
            decide(&running, &all),
            Decision::Skip(SkipReason::RateLimited)
        );
        all.held_until = Some("2026-12-31T23:59:00Z".parse().unwrap());
        assert_eq!(
            decide(&running, &all),
            Decision::Fire(FireCause::Due),
            "reset"
        );
    }

    #[test]
    fn a_missed_fire_follows_catch_up_and_with_no_app_is_app_absent() {
        let missed = moment(Cause::Missed, Presence::App);
        assert_eq!(
            decide(&job(false, CatchUp::Once), &missed),
            Decision::Fire(FireCause::CatchUp)
        );
        assert_eq!(
            decide(&job(false, CatchUp::Skip), &missed),
            Decision::Skip(SkipReason::Missed)
        );
        let alone = moment(Cause::Missed, Presence::Absent);
        assert_eq!(
            decide(&job(false, CatchUp::Once), &alone),
            Decision::Skip(SkipReason::AppAbsent)
        );
    }

    #[test]
    fn an_owed_fire_is_made_once_for_once_and_let_go_for_skip() {
        let back = moment(Cause::Owed, Presence::App);
        assert_eq!(
            decide(&job(false, CatchUp::Once), &back),
            Decision::Fire(FireCause::CatchUp)
        );
        assert_eq!(decide(&job(false, CatchUp::Skip), &back), Decision::Nothing);
        assert_eq!(decide(&job(true, CatchUp::Once), &back), Decision::Nothing);
        let mut busy = back;
        busy.last_run_live = true;
        assert_eq!(
            decide(&job(false, CatchUp::Once), &busy),
            Decision::Skip(SkipReason::Overlap)
        );
    }

    #[test]
    fn run_now_ignores_paused_and_refuses_no_app_overlap_and_the_rate_limit() {
        let paused = job(true, CatchUp::Skip);
        let mut now = moment(Cause::Due, Presence::App);
        assert_eq!(run_now(&paused, &now), Ok(()));
        now.presence = Presence::Absent;
        assert_eq!(run_now(&paused, &now), Err(RunNowRefused::AppAbsent));
        now.presence = Presence::App;
        now.last_run_live = true;
        assert_eq!(run_now(&paused, &now), Err(RunNowRefused::Running));
        now.last_run_live = false;
        let until: Timestamp = "2027-01-01T00:05:00Z".parse().unwrap();
        now.held_until = Some(until);
        assert_eq!(
            run_now(&paused, &now),
            Err(RunNowRefused::RateLimited(until))
        );
    }
}
