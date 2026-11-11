use std::str::FromStr;

use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use serde::{Deserialize, Serialize};

use crate::errors::{CoreError, Result};

/// Largest interval we accept (≈10 years); keeps all arithmetic far from overflow.
const MAX_INTERVAL_SECS: u64 = 10 * 365 * 24 * 3600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Schedule {
    /// Fires at `anchor + k * every`. Because the slots hang off a fixed
    /// anchor, an interval is measured between scheduled times — not from the
    /// end of the previous run — and survives restarts without drifting.
    Interval {
        every_secs: u64,
        anchor: DateTime<Utc>,
    },
    Daily {
        time: NaiveTime,
    },
    Weekly {
        days: Vec<Weekday>,
        time: NaiveTime,
    },
    /// Standard 5-field Unix cron (`min hour day-of-month month day-of-week`),
    /// with day-of-week `0`/`7` = Sunday and `1` = Monday.
    Cron {
        expr: String,
    },
}

impl Schedule {
    pub fn every_minutes(minutes: u64, anchor: DateTime<Utc>) -> Self {
        Self::Interval {
            every_secs: minutes.saturating_mul(60),
            anchor,
        }
    }

    pub fn every_hours(hours: u64, anchor: DateTime<Utc>) -> Self {
        Self::Interval {
            every_secs: hours.saturating_mul(3600),
            anchor,
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Interval { every_secs, .. } => {
                if *every_secs == 0 || *every_secs > MAX_INTERVAL_SECS {
                    return Err(CoreError::InvalidSchedule(format!(
                        "interval must be between 1 second and {MAX_INTERVAL_SECS} seconds"
                    )));
                }
            }
            Self::Daily { .. } => {}
            Self::Weekly { days, .. } => {
                if days.is_empty() {
                    return Err(CoreError::InvalidSchedule(
                        "weekly schedule needs at least one day".into(),
                    ));
                }
            }
            Self::Cron { expr } => {
                parse_cron(expr)?;
            }
        }
        Ok(())
    }

    /// First scheduled instant strictly after `after`, evaluating wall-clock
    /// rules (daily/weekly/cron) in `tz`. `None` if the schedule has no future
    /// occurrence or is invalid.
    pub fn next_after<Tz: TimeZone>(&self, after: DateTime<Utc>, tz: &Tz) -> Option<DateTime<Utc>> {
        match self {
            Self::Interval { every_secs, anchor } => next_interval(*every_secs, *anchor, after),
            Self::Daily { time } => next_wall_clock(after, tz, *time, |_| true),
            Self::Weekly { days, time } => next_wall_clock(after, tz, *time, |weekday| days.contains(&weekday)),
            Self::Cron { expr } => {
                let schedule = parse_cron(expr).ok()?;
                let next = schedule.after(&after.with_timezone(tz)).next()?;
                Some(next.with_timezone(&Utc))
            }
        }
    }
}

fn next_interval(every_secs: u64, anchor: DateTime<Utc>, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if every_secs == 0 || every_secs > MAX_INTERVAL_SECS {
        return None;
    }
    // The database keeps slots with millisecond precision, truncating downwards.
    // An anchor with sub-millisecond digits would yield slots that are stored
    // slightly *earlier* than they really are, so a wake-up inside that gap sees
    // the slot as still due and fires it again. Aligning the anchor to the
    // millisecond makes every slot exactly representable.
    let anchor = DateTime::from_timestamp_millis(anchor.timestamp_millis())?;
    let every = every_secs as i64;
    let elapsed = (after - anchor).num_seconds();
    let k = if elapsed < 0 { 1 } else { elapsed / every + 1 };
    anchor.checked_add_signed(Duration::seconds(k.checked_mul(every)?))
}

fn next_wall_clock<Tz: TimeZone>(
    after: DateTime<Utc>,
    tz: &Tz,
    time: NaiveTime,
    day_matches: impl Fn(Weekday) -> bool,
) -> Option<DateTime<Utc>> {
    let first_day = after.with_timezone(tz).date_naive();
    // A weekly schedule is at most 7 days away; the extra day covers the case
    // where today's slot has already passed.
    (0..=8)
        .filter_map(|offset| first_day.checked_add_signed(Duration::days(offset)))
        .filter(|date| day_matches(date.weekday()))
        .filter_map(|date| resolve_local(tz, date, time))
        .find(|candidate| *candidate > after)
}

/// Maps a local date + time to an instant, coping with DST transitions:
/// * ambiguous (clocks go back): the earlier occurrence wins, so the slot
///   fires once;
/// * non-existent (clocks go forward): fires one hour later, right after the
///   gap, rather than being skipped for the day.
fn resolve_local<Tz: TimeZone>(tz: &Tz, date: NaiveDate, time: NaiveTime) -> Option<DateTime<Utc>> {
    let naive = date.and_time(time);
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(dt) => Some(dt.with_timezone(&Utc)),
        LocalResult::Ambiguous(earliest, _) => Some(earliest.with_timezone(&Utc)),
        LocalResult::None => tz
            .from_local_datetime(&(naive + Duration::hours(1)))
            .earliest()
            .map(|dt| dt.with_timezone(&Utc)),
    }
}

fn parse_cron(expr: &str) -> Result<cron::Schedule> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(CoreError::InvalidSchedule(format!(
            "cron expression needs 5 fields (min hour day month weekday), got {}",
            fields.len()
        )));
    }
    // The `cron` crate wants a leading seconds field and numbers weekdays
    // 1-7 starting on Sunday; translate from standard Unix cron.
    let normalized = format!(
        "0 {} {} {} {} {}",
        fields[0],
        fields[1],
        fields[2],
        fields[3],
        convert_weekday_field(fields[4])
    );
    cron::Schedule::from_str(&normalized)
        .map_err(|e| CoreError::InvalidSchedule(format!("invalid cron expression {expr:?}: {e}")))
}

/// Unix cron weekday numbers (0/7 = Sun, 1 = Mon … 6 = Sat) → the `cron`
/// crate's (1 = Sun, 2 = Mon … 7 = Sat). Names (`MON-FRI`) and `*` pass through.
fn convert_weekday_field(field: &str) -> String {
    let convert = |token: &str| match token.parse::<u32>() {
        Ok(n) => ((n % 7) + 1).to_string(),
        Err(_) => token.to_owned(),
    };
    field
        .split(',')
        .map(|part| {
            let (range, step) = match part.split_once('/') {
                Some((range, step)) => (range, Some(step)),
                None => (part, None),
            };
            let converted = match range.split_once('-') {
                // `5-7` (Fri–Sun) would become the descending `6-1`; split it.
                Some((from, "7")) if step.is_none() && matches!(from.parse::<u32>(), Ok(1..=6)) => {
                    let from: u32 = from.parse().unwrap_or(1);
                    format!("{}-7,1", from + 1)
                }
                Some((from, to)) => format!("{}-{}", convert(from), convert(to)),
                None => convert(range),
            };
            match step {
                Some(step) => format!("{converted}/{step}"),
                None => converted,
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use chrono::FixedOffset;
    use chrono_tz::America::New_York;

    use super::*;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn interval_is_measured_between_slots_not_from_now() {
        let anchor = utc(2025, 1, 1, 10, 0, 0);
        let s = Schedule::every_minutes(10, anchor);
        // Still inside the 10:00-10:10 slot: next is 10:10, even if "now" is 10:07.
        assert_eq!(
            s.next_after(utc(2025, 1, 1, 10, 7, 30), &Utc),
            Some(utc(2025, 1, 1, 10, 10, 0))
        );
        // Exactly on a slot: strictly after, so the next one.
        assert_eq!(
            s.next_after(utc(2025, 1, 1, 10, 10, 0), &Utc),
            Some(utc(2025, 1, 1, 10, 20, 0))
        );
        // Long after (e.g. service was down): jumps to the next future slot.
        assert_eq!(
            s.next_after(utc(2025, 1, 2, 3, 3, 3), &Utc),
            Some(utc(2025, 1, 2, 3, 10, 0))
        );
        // Before the anchor: first run is anchor + every.
        assert_eq!(
            s.next_after(utc(2024, 12, 31, 0, 0, 0), &Utc),
            Some(utc(2025, 1, 1, 10, 10, 0))
        );
    }

    #[test]
    fn interval_slots_are_aligned_to_the_millisecond() {
        // Regression: a sub-millisecond anchor produced slots the database stored
        // earlier than the real value, which made a due slot fire repeatedly.
        let anchor = utc(2025, 1, 1, 10, 0, 0) + Duration::nanoseconds(123_456_789);
        let schedule = Schedule::Interval { every_secs: 1, anchor };
        for offset_ns in [0, 1, 500_000, 999_999, 1_000_000, 123_000_000, 999_999_999] {
            let after = utc(2025, 1, 1, 10, 0, 7) + Duration::nanoseconds(offset_ns);
            let next = schedule.next_after(after, &Utc).unwrap();
            assert_eq!(
                next.timestamp_subsec_nanos() % 1_000_000,
                0,
                "slot {next} is not millisecond-aligned"
            );
            assert!(next > after, "slot {next} must lie after {after}");
            assert!(next - after <= Duration::seconds(1));
        }
    }

    #[test]
    fn invalid_intervals_are_rejected() {
        let anchor = utc(2025, 1, 1, 0, 0, 0);
        assert!(Schedule::Interval { every_secs: 0, anchor }.validate().is_err());
        assert!(
            Schedule::Interval {
                every_secs: u64::MAX,
                anchor
            }
            .validate()
            .is_err()
        );
        assert_eq!(
            Schedule::Interval { every_secs: 0, anchor }.next_after(anchor, &Utc),
            None
        );
    }

    #[test]
    fn daily_uses_the_local_timezone() {
        let brt = FixedOffset::west_opt(3 * 3600).unwrap();
        let s = Schedule::Daily { time: t(8, 0) };
        // 08:00 local = 11:00 UTC.
        assert_eq!(
            s.next_after(utc(2025, 6, 10, 9, 0, 0), &brt),
            Some(utc(2025, 6, 10, 11, 0, 0))
        );
        assert_eq!(
            s.next_after(utc(2025, 6, 10, 11, 0, 0), &brt),
            Some(utc(2025, 6, 11, 11, 0, 0))
        );
    }

    #[test]
    fn weekly_picks_the_next_selected_weekday() {
        // 2025-06-09 is a Monday.
        let s = Schedule::Weekly {
            days: vec![Weekday::Wed, Weekday::Fri],
            time: t(9, 30),
        };
        assert_eq!(
            s.next_after(utc(2025, 6, 9, 12, 0, 0), &Utc),
            Some(utc(2025, 6, 11, 9, 30, 0))
        );
        assert_eq!(
            s.next_after(utc(2025, 6, 11, 9, 30, 0), &Utc),
            Some(utc(2025, 6, 13, 9, 30, 0))
        );
        // After Friday's slot it wraps to next Wednesday.
        assert_eq!(
            s.next_after(utc(2025, 6, 13, 10, 0, 0), &Utc),
            Some(utc(2025, 6, 18, 9, 30, 0))
        );
        assert!(
            Schedule::Weekly {
                days: vec![],
                time: t(9, 0)
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn dst_gap_runs_right_after_the_gap() {
        // New York springs forward on 2025-03-09: 02:30 local does not exist.
        let s = Schedule::Daily { time: t(2, 30) };
        let next = s.next_after(utc(2025, 3, 8, 17, 0, 0), &New_York);
        assert_eq!(next, Some(utc(2025, 3, 9, 7, 30, 0))); // 03:30 EDT
    }

    #[test]
    fn dst_overlap_fires_once() {
        // New York falls back on 2025-11-02: 01:30 local happens twice.
        let s = Schedule::Daily { time: t(1, 30) };
        let first = s.next_after(utc(2025, 11, 1, 20, 0, 0), &New_York).unwrap();
        assert_eq!(first, utc(2025, 11, 2, 5, 30, 0)); // 01:30 EDT, the earlier one
        let second = s.next_after(first, &New_York).unwrap();
        assert_eq!(second, utc(2025, 11, 3, 6, 30, 0)); // next day, not the repeated 01:30 EST
    }

    #[test]
    fn cron_weekday_numbers_follow_unix_convention() {
        // 2025-06-09 is a Monday. "0 9 * * 1" = Mondays at 09:00 in Unix cron.
        let monday = Schedule::Cron {
            expr: "0 9 * * 1".into(),
        };
        assert_eq!(
            monday.next_after(utc(2025, 6, 8, 0, 0, 0), &Utc),
            Some(utc(2025, 6, 9, 9, 0, 0))
        );
        // 0 and 7 are both Sunday (2025-06-15).
        for dow in ["0", "7"] {
            let sunday = Schedule::Cron {
                expr: format!("0 9 * * {dow}"),
            };
            assert_eq!(
                sunday.next_after(utc(2025, 6, 9, 0, 0, 0), &Utc),
                Some(utc(2025, 6, 15, 9, 0, 0))
            );
        }
        // 1-5 = Mon-Fri: from Friday evening the next slot is Monday.
        let weekdays = Schedule::Cron {
            expr: "30 8 * * 1-5".into(),
        };
        assert_eq!(
            weekdays.next_after(utc(2025, 6, 13, 12, 0, 0), &Utc),
            Some(utc(2025, 6, 16, 8, 30, 0))
        );
        // 5-7 = Fri-Sun: from Friday 12:00 the next slot is Saturday.
        let weekend = Schedule::Cron {
            expr: "0 10 * * 5-7".into(),
        };
        assert_eq!(
            weekend.next_after(utc(2025, 6, 13, 12, 0, 0), &Utc),
            Some(utc(2025, 6, 14, 10, 0, 0))
        );
        assert_eq!(
            weekend.next_after(utc(2025, 6, 15, 12, 0, 0), &Utc),
            Some(utc(2025, 6, 20, 10, 0, 0))
        );
    }

    #[test]
    fn cron_steps_and_names_work() {
        let every_15 = Schedule::Cron {
            expr: "*/15 * * * *".into(),
        };
        assert_eq!(
            every_15.next_after(utc(2025, 6, 9, 10, 1, 0), &Utc),
            Some(utc(2025, 6, 9, 10, 15, 0))
        );
        let named = Schedule::Cron {
            expr: "0 9 * * MON".into(),
        };
        assert_eq!(
            named.next_after(utc(2025, 6, 8, 0, 0, 0), &Utc),
            Some(utc(2025, 6, 9, 9, 0, 0))
        );
    }

    #[test]
    fn bad_cron_is_rejected() {
        assert!(Schedule::Cron { expr: "* * * *".into() }.validate().is_err());
        assert!(
            Schedule::Cron {
                expr: "61 * * * *".into()
            }
            .validate()
            .is_err()
        );
        assert!(
            Schedule::Cron {
                expr: "0 0 0 9 * * *".into()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn schedule_survives_a_json_round_trip() {
        let schedules = [
            Schedule::every_hours(2, utc(2025, 1, 1, 0, 0, 0)),
            Schedule::Daily { time: t(7, 5) },
            Schedule::Weekly {
                days: vec![Weekday::Mon, Weekday::Sun],
                time: t(23, 59),
            },
            Schedule::Cron {
                expr: "0 9 * * 1-5".into(),
            },
        ];
        for s in schedules {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(serde_json::from_str::<Schedule>(&json).unwrap(), s);
        }
    }
}
