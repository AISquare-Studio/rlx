//! The time, as RFC 3339 in UTC: `2026-11-25T12:00:00.000Z`, the form the wire's `time`
//! fields and the daemon registry's records take.

use std::time::{SystemTime, UNIX_EPOCH};

/// Now, as `2026-11-25T12:00:00.000Z`.
pub fn now() -> String {
    format(SystemTime::now())
}

/// `time` as `2026-11-25T12:00:00.000Z`, to the millisecond. A time before 1970 is
/// written as 1970's first instant.
pub fn format(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since.as_secs();
    let (year, month, day) = civil_from_days(seconds / 86_400);
    let of_day = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60,
        since.subsec_millis()
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01, by Howard Hinnant's
/// `civil_from_days`.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(seconds: u64, millis: u64) -> String {
        format(UNIX_EPOCH + Duration::from_secs(seconds) + Duration::from_millis(millis))
    }

    #[test]
    fn known_instants_print_as_rfc_3339_in_utc() {
        assert_eq!(at(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(at(951_782_400, 5), "2000-02-29T00:00:00.005Z");
        assert_eq!(at(1_795_608_000, 0), "2026-11-25T12:00:00.000Z");
        assert_eq!(at(4_107_542_399, 999), "2100-02-28T23:59:59.999Z");
        assert_eq!(at(4_107_542_400, 0), "2100-03-01T00:00:00.000Z");
    }

    #[test]
    fn every_day_of_four_centuries_follows_the_one_before() {
        let (mut year, mut month, mut day) = civil_from_days(0);
        for days in 1..146_097 {
            let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
            let length = match month {
                2 if leap => 29,
                2 => 28,
                4 | 6 | 9 | 11 => 30,
                _ => 31,
            };
            (year, month, day) = match (day == length, month == 12) {
                (false, _) => (year, month, day + 1),
                (true, false) => (year, month + 1, 1),
                (true, true) => (year + 1, 1, 1),
            };
            assert_eq!(civil_from_days(days), (year, month, day), "day {days}");
        }
    }
}
