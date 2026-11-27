//! When a job is due (handoff 0.10.3, 7.6): `every_minutes` on a fixed grid, or a
//! five-field cron expression walked in civil minutes in an IANA time zone.
//!
//! - **every_minutes** is a grid from its anchor (the `job.put` time): the fires are at
//!   anchor + k·N minutes, whenever each one actually ran, so latency never drifts it.
//! - **cron** matches civil minutes in its zone, each civil minute once:
//!   - a match in a DST gap (a local time that doesn't happen) fires once, at the gap's
//!     end, the next instant that is valid; several matches in one gap are that one fire;
//!   - a match in a DST fold (a local time that happens twice) fires once, at the first
//!     occurrence, and the repeated hour fires nothing again.
//!
//! The zones come from jiff's bundled tz database, the same on every OS.

use jiff::civil::DateTime;
use jiff::tz::{AmbiguousOffset, TimeZone};
use jiff::{SignedDuration, Timestamp};
use rlxd_proto::Schedule;

/// How far ahead a cron expression is searched for its next match: past this, it never
/// matches (`30 2 31 2 *`), and `job.put` refuses it.
const SEARCH_YEARS: i16 = 5;

/// A job's schedule, read and checked.
#[derive(Clone, Debug)]
pub enum Plan {
    /// Every `minutes`, on the grid of `anchor`.
    Every { minutes: u32 },
    /// A cron expression in a time zone.
    Cron { cron: Cron, tz: TimeZone },
}

impl Plan {
    /// `schedule`, read: an error says what is wrong with it, for `invalid_params`.
    pub fn of(schedule: &Schedule) -> Result<Self, String> {
        match schedule {
            Schedule::Every { every_minutes } if *every_minutes >= 1 => Ok(Self::Every {
                minutes: *every_minutes,
            }),
            Schedule::Every { .. } => Err("schedule.every_minutes must be at least 1".to_owned()),
            Schedule::Cron { cron, tz } => {
                let zone = TimeZone::get(tz)
                    .map_err(|_| format!("schedule.tz {tz:?} is not a time zone rlxd knows"))?;
                let cron = Cron::parse(cron).map_err(|why| format!("schedule.cron: {why}"))?;
                let plan = Self::Cron { cron, tz: zone };
                // One that never matches is refused now, not found out at each tick.
                if plan.next_after(Timestamp::now(), None).is_none() {
                    return Err(format!(
                        "schedule.cron {cron:?} matches no time in the next {SEARCH_YEARS} years",
                        cron = schedule_text(schedule)
                    ));
                }
                Ok(plan)
            }
        }
    }

    /// The first fire strictly after `after`. `anchor` is an `every_minutes` grid's origin
    /// (the put time); `None` for cron.
    pub fn next_after(&self, after: Timestamp, anchor: Option<Timestamp>) -> Option<Timestamp> {
        match self {
            Self::Every { minutes } => {
                let anchor = anchor.unwrap_or(after);
                let step = i64::from(*minutes) * 60;
                let since = after.as_second() - anchor.as_second();
                // The grid point after `after`: anchor + k·step, k ≥ 1.
                let k = if since < 0 { 1 } else { since / step + 1 };
                Timestamp::from_second(anchor.as_second() + k * step).ok()
            }
            Self::Cron { cron, tz } => cron.next_after(after, tz),
        }
    }
}

fn schedule_text(schedule: &Schedule) -> String {
    match schedule {
        Schedule::Cron { cron, .. } => cron.clone(),
        Schedule::Every { every_minutes } => format!("every {every_minutes} minutes"),
    }
}

/// A five-field cron expression: minute, hour, day of month, month, day of week.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    /// Whether the day of month and day of week fields were both restricted (neither
    /// starts with `*`): then a day matches either, as cron has it; else both.
    either_day: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

impl Cron {
    /// `text`, read: `*`, numbers, `a-b` ranges, `/n` steps, `,` lists, and month and day
    /// names (`jan`, `mon`); day of week 0 or 7 is Sunday.
    pub fn parse(text: &str) -> Result<Self, String> {
        let fields: Vec<&str> = text.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields[..] else {
            return Err(format!(
                "{text:?} has {} fields; it takes five: minute, hour, day of month, month, \
                 day of week",
                fields.len()
            ));
        };
        let weekdays = field(weekday, 0, 7, &WEEKDAYS, "day of week")?;
        // 7 is Sunday too.
        let weekdays = (weekdays | ((weekdays >> 7) & 1)) & 0x7f;
        Ok(Self {
            minutes: field(minute, 0, 59, &[], "minute")?,
            hours: u32::try_from(field(hour, 0, 23, &[], "hour")?).unwrap_or(0),
            days: u32::try_from(field(day, 1, 31, &[], "day of month")? >> 1).unwrap_or(0),
            months: u16::try_from(field(month, 1, 12, &MONTHS, "month")? >> 1).unwrap_or(0),
            weekdays: u8::try_from(weekdays).unwrap_or(0),
            either_day: !day.starts_with('*') && !weekday.starts_with('*'),
        })
    }

    fn day_matches(&self, at: DateTime) -> bool {
        let day = self.days & (1 << (at.day() - 1)) != 0;
        let weekday = self.weekdays & (1 << at.weekday().to_sunday_zero_offset()) != 0;
        if self.either_day {
            day || weekday
        } else {
            day && weekday
        }
    }

    /// The first civil minute strictly after `after` that matches, within
    /// [`SEARCH_YEARS`].
    fn next_civil(&self, after: DateTime) -> Option<DateTime> {
        let minute = |dt: DateTime| dt.with().second(0).subsec_nanosecond(0).build().ok();
        let mut at = minute(after)?
            .checked_add(SignedDuration::from_mins(1))
            .ok()?;
        let limit = after.year().saturating_add(SEARCH_YEARS);
        while at.year() <= limit {
            if self.months & (1 << (at.month() - 1)) == 0 {
                // The first of the next month.
                let first = at.date().first_of_month();
                at = first
                    .last_of_month()
                    .tomorrow()
                    .ok()?
                    .to_datetime(jiff::civil::Time::midnight());
                continue;
            }
            if !self.day_matches(at) {
                at = at
                    .date()
                    .tomorrow()
                    .ok()?
                    .to_datetime(jiff::civil::Time::midnight());
                continue;
            }
            if self.hours & (1 << at.hour()) == 0 {
                at = at
                    .with()
                    .minute(0)
                    .build()
                    .ok()?
                    .checked_add(SignedDuration::from_hours(1))
                    .ok()?;
                continue;
            }
            if self.minutes & (1 << at.minute()) == 0 {
                at = at.checked_add(SignedDuration::from_mins(1)).ok()?;
                continue;
            }
            return Some(at);
        }
        None
    }

    /// The first fire strictly after `after`, in `tz`: each civil minute that matches,
    /// once, a gap's at the gap's end and a fold's at its first occurrence.
    pub fn next_after(&self, after: Timestamp, tz: &TimeZone) -> Option<Timestamp> {
        let mut civil = tz.to_datetime(after);
        loop {
            let found = self.next_civil(civil)?;
            let at = resolve(tz, found)?;
            if at > after {
                return Some(at);
            }
            // The instant was had already: a gap's end, or a fold's first occurrence.
            civil = found;
        }
    }
}

/// The instant civil time `at` names in `tz`: itself when there is one; the gap's end when
/// `at` falls in one (it never happens, and the next valid instant stands in for it); the
/// first of the two when it falls in a fold.
pub fn resolve(tz: &TimeZone, at: DateTime) -> Option<Timestamp> {
    let ambiguous = tz.to_ambiguous_timestamp(at);
    match ambiguous.offset() {
        AmbiguousOffset::Unambiguous { .. } | AmbiguousOffset::Fold { .. } => {
            ambiguous.earlier().ok()
        }
        AmbiguousOffset::Gap { .. } => {
            // Read with the offset after the gap, it is an instant before the transition:
            // the transition that follows it is the gap's end.
            let before = ambiguous.earlier().ok()?;
            tz.following(before)
                .next()
                .map(|transition| transition.timestamp())
        }
    }
}

/// One field's values as bits (bit `n` is value `n`), from `low` to `high`.
fn field(text: &str, low: u64, high: u64, names: &[&str], what: &str) -> Result<u64, String> {
    let value = |part: &str| -> Result<u64, String> {
        if let Some(at) = names
            .iter()
            .position(|name| part.eq_ignore_ascii_case(name))
        {
            // Months are named from 1, days of week from 0.
            return Ok(at as u64 + low.min(1) * u64::from(names.len() == 12));
        }
        part.parse::<u64>()
            .ok()
            .filter(|n| (low..=high).contains(n))
            .ok_or_else(|| format!("{what} {part:?} is not a number from {low} to {high}"))
    };
    let mut bits = 0_u64;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => {
                let step = step
                    .parse::<u64>()
                    .ok()
                    .filter(|step| *step >= 1)
                    .ok_or_else(|| format!("{what} step {step:?} is not a positive number"))?;
                (range, step)
            }
            None => (item, 1),
        };
        let (from, to) = if range == "*" {
            (low, high)
        } else if let Some((from, to)) = range.split_once('-') {
            (value(from)?, value(to)?)
        } else if item.contains('/') {
            // `a/n`: from a to the end, every n.
            (value(range)?, high)
        } else {
            let one = value(range)?;
            (one, one)
        };
        if from > to {
            return Err(format!("{what} range {range:?} runs backwards"));
        }
        // A step past the field's span takes its first value only: the same as the span.
        let step = step.min(high - low + 1);
        let mut n = from;
        while n <= to {
            bits |= 1 << n;
            match n.checked_add(step) {
                Some(next) => n = next,
                None => break,
            }
        }
    }
    if bits == 0 {
        return Err(format!("{what} {text:?} matches nothing"));
    }
    Ok(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn cron(text: &str, tz: &str) -> Plan {
        Plan::of(&Schedule::Cron {
            cron: text.to_owned(),
            tz: tz.to_owned(),
        })
        .unwrap()
    }

    /// The fires of `plan` after `from`, `n` of them.
    fn fires(plan: &Plan, from: &str, n: usize) -> Vec<String> {
        let mut after = at(from);
        let mut out = Vec::new();
        for _ in 0..n {
            after = plan.next_after(after, None).unwrap();
            out.push(after.to_string());
        }
        out
    }

    #[test]
    fn every_minutes_keeps_its_grid_whenever_it_is_asked() {
        let plan = Plan::Every { minutes: 15 };
        let anchor = Some(at("2027-01-01T00:00:00Z"));
        // The next point, asked on it, just after it, or late: always on the grid.
        for (after, next) in [
            ("2027-01-01T00:00:00Z", "2027-01-01T00:15:00Z"),
            ("2027-01-01T00:15:00Z", "2027-01-01T00:30:00Z"),
            ("2027-01-01T00:15:07Z", "2027-01-01T00:30:00Z"),
            ("2027-01-01T03:59:59Z", "2027-01-01T04:00:00Z"),
            ("2026-12-31T23:00:00Z", "2027-01-01T00:15:00Z"),
        ] {
            assert_eq!(
                plan.next_after(at(after), anchor),
                Some(at(next)),
                "{after}"
            );
        }
    }

    #[test]
    fn cron_reads_lists_ranges_steps_and_names() {
        let every = cron("* * * * *", "UTC");
        assert_eq!(
            fires(&every, "2027-01-01T00:00:30Z", 2),
            ["2027-01-01T00:01:00Z", "2027-01-01T00:02:00Z"]
        );
        let quarter = cron("*/15 9-10 * * mon-fri", "UTC");
        // 2027-01-01 is a Friday; then the weekend is passed over.
        assert_eq!(
            fires(&quarter, "2027-01-01T10:40:00Z", 2),
            ["2027-01-01T10:45:00Z", "2027-01-04T09:00:00Z"]
        );
        let named = cron("0 12 1 jan,jul *", "UTC");
        assert_eq!(
            fires(&named, "2027-01-02T00:00:00Z", 1),
            ["2027-07-01T12:00:00Z"]
        );
        // Day of month or day of week, when both are given: the 13th, or a Friday.
        let either = cron("0 0 13 * 5", "UTC");
        assert_eq!(
            fires(&either, "2027-01-02T00:00:00Z", 2),
            ["2027-01-08T00:00:00Z", "2027-01-13T00:00:00Z"]
        );
        // Sunday is 0 and 7.
        assert_eq!(
            fires(&cron("0 0 * * 7", "UTC"), "2027-01-01T00:00:00Z", 1),
            fires(&cron("0 0 * * 0", "UTC"), "2027-01-01T00:00:00Z", 1)
        );
        // A step of any size, never an overflow: past the span, the first value only.
        let huge = cron("1/18446744073709551615 * * * *", "UTC");
        assert_eq!(
            fires(&huge, "2027-01-01T00:00:30Z", 2),
            ["2027-01-01T00:01:00Z", "2027-01-01T01:01:00Z"]
        );
        for bad in [
            "* * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "5-1 * * * *",
            "*/0 * * * *",
            "* * * foo *",
            "0 0 31 2 *",
        ] {
            assert!(
                Plan::of(&Schedule::Cron {
                    cron: bad.to_owned(),
                    tz: "UTC".to_owned()
                })
                .is_err(),
                "{bad}"
            );
        }
        assert!(
            Plan::of(&Schedule::Cron {
                cron: "* * * * *".to_owned(),
                tz: "Mars/Olympus".to_owned()
            })
            .is_err()
        );
    }

    #[test]
    fn a_local_time_a_dst_gap_skips_fires_once_at_the_next_valid_instant() {
        // London, 2027-03-28: 01:00 GMT becomes 02:00 BST, so 01:00 to 01:59 never
        // happen locally. 01:30 fires once, at 02:00 BST (01:00 UTC), and the day after at
        // 01:30 BST.
        let half_past_one = cron("30 1 * * *", "Europe/London");
        assert_eq!(
            fires(&half_past_one, "2027-03-27T12:00:00Z", 2),
            ["2027-03-28T01:00:00Z", "2027-03-29T00:30:00Z"]
        );
        // Four matches in the gap are one fire, then 02:00 BST's own match is that same
        // instant, so it isn't a second one.
        let quarters = cron("*/15 1,2 * * *", "Europe/London");
        assert_eq!(
            fires(&quarters, "2027-03-28T00:50:00Z", 3),
            [
                "2027-03-28T01:00:00Z",
                "2027-03-28T01:15:00Z",
                "2027-03-28T01:30:00Z"
            ]
        );
    }

    #[test]
    fn a_local_time_a_dst_fold_repeats_fires_once() {
        // London, 2027-10-31: 02:00 BST becomes 01:00 GMT, so 01:00 to 01:59 happen twice.
        // 01:30 fires at the first (00:30 UTC), not again at 01:30 UTC.
        let half_past_one = cron("30 1 * * *", "Europe/London");
        assert_eq!(
            fires(&half_past_one, "2027-10-30T12:00:00Z", 2),
            ["2027-10-31T00:30:00Z", "2027-11-01T01:30:00Z"]
        );
        // Every 20 minutes: the repeated hour's minutes fire once each.
        let twenty = cron("*/20 * * * *", "Europe/London");
        assert_eq!(
            fires(&twenty, "2027-10-30T23:50:00Z", 7),
            [
                "2027-10-31T00:00:00Z",
                "2027-10-31T00:20:00Z",
                "2027-10-31T00:40:00Z",
                "2027-10-31T02:00:00Z",
                "2027-10-31T02:20:00Z",
                "2027-10-31T02:40:00Z",
                "2027-10-31T03:00:00Z"
            ]
        );
    }
}
