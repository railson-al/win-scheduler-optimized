//! Turning the daemon's data into text for the screens: durations, local dates, byte counts,
//! schedule summaries, status labels. Pure functions, no window needed (D19).
//!
//! The daemon speaks UTC; dates are converted to a time zone only here, at display time (D18). The
//! zone is a parameter so the tests do not depend on the machine they run on; the `*_local` helpers
//! pass the machine's own.

use std::fmt::Display;

use chrono::{DateTime, Duration, Local, NaiveTime, TimeZone, Timelike, Utc};
use scriptrunner_core::{ExecutionStatus, Schedule, Trigger};

use crate::backend::{ConnectionState, Reason};
use crate::i18n::{self, Lang, Text, t};

/// How a status should look, without naming a colour: the views map a tone to their theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Active,
    Good,
    Bad,
    Warn,
}

pub fn status_label(status: ExecutionStatus) -> &'static str {
    t(match status {
        ExecutionStatus::Queued => Text::StatusQueued,
        ExecutionStatus::Running => Text::StatusRunning,
        ExecutionStatus::Success => Text::StatusSuccess,
        ExecutionStatus::Failed => Text::StatusFailed,
        ExecutionStatus::Interrupted => Text::StatusInterrupted,
        ExecutionStatus::Skipped => Text::StatusSkipped,
    })
}

pub fn status_tone(status: ExecutionStatus) -> Tone {
    match status {
        ExecutionStatus::Queued | ExecutionStatus::Skipped => Tone::Neutral,
        ExecutionStatus::Running => Tone::Active,
        ExecutionStatus::Success => Tone::Good,
        ExecutionStatus::Failed => Tone::Bad,
        ExecutionStatus::Interrupted => Tone::Warn,
    }
}

pub fn trigger_label(trigger: Trigger) -> &'static str {
    t(match trigger {
        Trigger::Manual => Text::TriggerManual,
        Trigger::Scheduled => Text::TriggerScheduled,
    })
}

/// A span of whole seconds as the two largest non-zero units: `45 s`, `3 min 05 s`, `1 h 02 min`,
/// `2 d 3 h`. Zero is `0 s`.
pub fn seconds(total: u64) -> String {
    let (days, rest) = (total / 86_400, total % 86_400);
    let (hours, rest) = (rest / 3_600, rest % 3_600);
    let (minutes, secs) = (rest / 60, rest % 60);
    if days > 0 {
        if hours > 0 {
            format!("{days} d {hours} h")
        } else {
            format!("{days} d")
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!("{hours} h {minutes:02} min")
        } else {
            format!("{hours} h")
        }
    } else if minutes > 0 {
        if secs > 0 {
            format!("{minutes} min {secs:02} s")
        } else {
            format!("{minutes} min")
        }
    } else {
        format!("{secs} s")
    }
}

/// How long an execution took: milliseconds under a second, one decimal under ten seconds
/// (`4,2 s`, with the decimal comma), then [`seconds`]. Negative spans (a clock that moved) read as zero.
pub fn duration(span: Duration) -> String {
    let millis = span.num_milliseconds().max(0);
    if millis < 1_000 {
        format!("{millis} ms")
    } else if millis < 10_000 {
        format!("{},{} s", millis / 1_000, (millis % 1_000) / 100)
    } else {
        seconds(millis as u64 / 1_000)
    }
}

/// `dd/mm/aaaa hh:mm:ss` in `zone`.
pub fn date_time<Tz>(at: DateTime<Utc>, zone: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    at.with_timezone(zone).format("%d/%m/%Y %H:%M:%S").to_string()
}

/// [`date_time`] in the machine's own zone.
pub fn date_time_local(at: DateTime<Utc>) -> String {
    date_time(at, &Local)
}

/// `hh:mm:ss.mmm` in `zone`, for a line of a log.
pub fn log_time<Tz>(at: DateTime<Utc>, zone: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    at.with_timezone(zone).format("%H:%M:%S%.3f").to_string()
}

/// A byte count in binary units with a decimal comma: `812 B`, `3,0 MB`.
pub fn bytes(count: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if count < 1024 {
        return format!("{count} B");
    }
    let mut value = count as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit]).replace('.', ",")
}

/// One line describing a schedule: `A cada 15 min`, `Todo dia às 09:30`, `seg, qua, sex às 09:30`,
/// `Cron: 0 9 * * 1-5`.
pub fn schedule_summary(lang: Lang, schedule: &Schedule) -> String {
    let word = |key| i18n::text(lang, key);
    match schedule {
        Schedule::Interval { every_secs, .. } => format!("{} {}", word(Text::ScheduleEvery), seconds(*every_secs)),
        Schedule::Daily { time } => format!(
            "{} {} {}",
            word(Text::ScheduleDaily),
            word(Text::ScheduleAt),
            clock(*time)
        ),
        Schedule::Weekly { days, time } => {
            let mut days = days.clone();
            days.sort_by_key(|day| day.num_days_from_monday());
            days.dedup();
            let names: Vec<_> = days.into_iter().map(|day| i18n::weekday_short(lang, day)).collect();
            format!("{} {} {}", names.join(", "), word(Text::ScheduleAt), clock(*time))
        }
        Schedule::Cron { expr } => format!("{}: {expr}", word(Text::ScheduleCron)),
    }
}

/// The status bar's account of the connection: `Conectado`, or why there is none and when it will try again.
pub fn connection_summary(state: &ConnectionState) -> String {
    match state {
        ConnectionState::Connecting => t(Text::Connecting).to_owned(),
        ConnectionState::Connected { .. } => t(Text::Connected).to_owned(),
        ConnectionState::Down { reason, retry_in } => {
            let (headline, detail) = match reason {
                Reason::NotRunning => (t(Text::ReasonNotRunning), None),
                Reason::AccessDenied => (t(Text::ReasonAccessDenied), None),
                Reason::Protocol(detail) => (t(Text::ReasonProtocol), Some(detail)),
                Reason::Lost => (t(Text::ReasonLost), None),
                Reason::Other(detail) => (t(Text::ReasonOther), Some(detail)),
            };
            let mut line = format!("{}: {headline}", t(Text::DaemonDown));
            if let Some(detail) = detail {
                line.push_str(&format!(" ({detail})"));
            }
            line.push_str(&format!(
                " {} {}.",
                t(Text::RetryingIn),
                seconds(retry_in.as_secs().max(1))
            ));
            line
        }
    }
}

/// `hh:mm`, with seconds only when there are some.
fn clock(time: NaiveTime) -> String {
    if time.second() == 0 {
        time.format("%H:%M").to_string()
    } else {
        time.format("%H:%M:%S").to_string()
    }
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, Weekday};

    use super::*;

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    #[test]
    fn seconds_use_the_two_largest_units() {
        assert_eq!(seconds(0), "0 s");
        assert_eq!(seconds(45), "45 s");
        assert_eq!(seconds(60), "1 min");
        assert_eq!(seconds(185), "3 min 05 s");
        assert_eq!(seconds(3_600), "1 h");
        assert_eq!(seconds(3_720), "1 h 02 min");
        assert_eq!(seconds(86_400), "1 d");
        assert_eq!(seconds(97_200), "1 d 3 h");
    }

    #[test]
    fn durations_get_finer_for_short_runs() {
        assert_eq!(duration(Duration::milliseconds(320)), "320 ms");
        assert_eq!(duration(Duration::milliseconds(4_250)), "4,2 s");
        assert_eq!(duration(Duration::milliseconds(12_900)), "12 s");
        assert_eq!(duration(Duration::seconds(3_725)), "1 h 02 min");
        assert_eq!(duration(Duration::seconds(-5)), "0 ms");
    }

    #[test]
    fn dates_are_shown_in_the_requested_zone() {
        let at = utc("2026-11-11T20:50:03Z");
        let brasilia = FixedOffset::west_opt(3 * 3600).unwrap();
        assert_eq!(date_time(at, &brasilia), "11/11/2026 17:50:03");
        assert_eq!(date_time(at, &Utc), "11/11/2026 20:50:03");
        assert_eq!(log_time(utc("2026-11-11T20:50:03.042Z"), &Utc), "20:50:03.042");
    }

    #[test]
    fn a_date_crossing_midnight_changes_day_in_the_zone() {
        let at = utc("2026-11-12T01:30:00Z");
        let brasilia = FixedOffset::west_opt(3 * 3600).unwrap();
        assert_eq!(date_time(at, &brasilia), "11/11/2026 22:30:00");
    }

    #[test]
    fn byte_counts_use_binary_units_and_a_decimal_comma() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(812), "812 B");
        assert_eq!(bytes(1024), "1,0 KB");
        assert_eq!(bytes(3 * 1024 * 1024), "3,0 MB");
        assert_eq!(bytes(5 * 1024 * 1024 * 1024 * 1024), "5,0 TB");
        // Larger than the largest unit stays in it.
        assert_eq!(bytes(2048 * 1024 * 1024 * 1024 * 1024), "2048,0 TB");
    }

    #[test]
    fn schedules_read_as_a_sentence() {
        let anchor = utc("2026-01-01T00:00:00Z");
        let at = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
        assert_eq!(
            schedule_summary(Lang::PtBr, &Schedule::every_minutes(15, anchor)),
            "A cada 15 min"
        );
        assert_eq!(
            schedule_summary(Lang::PtBr, &Schedule::every_hours(2, anchor)),
            "A cada 2 h"
        );
        assert_eq!(
            schedule_summary(Lang::PtBr, &Schedule::Daily { time: at(9, 30) }),
            "Todo dia às 09:30"
        );
        assert_eq!(
            schedule_summary(
                Lang::PtBr,
                &Schedule::Cron {
                    expr: "0 9 * * 1-5".into()
                }
            ),
            "Cron: 0 9 * * 1-5"
        );
    }

    #[test]
    fn weekly_days_are_sorted_and_deduplicated() {
        let schedule = Schedule::Weekly {
            days: vec![Weekday::Fri, Weekday::Mon, Weekday::Fri, Weekday::Wed],
            time: NaiveTime::from_hms_opt(8, 5, 0).unwrap(),
        };
        assert_eq!(schedule_summary(Lang::PtBr, &schedule), "seg, qua, sex às 08:05");
    }

    #[test]
    fn the_connection_summary_says_why_and_when() {
        let down = |reason, secs| ConnectionState::Down {
            reason,
            retry_in: std::time::Duration::from_secs(secs),
        };
        assert_eq!(
            connection_summary(&ConnectionState::Connected {
                version: "0.1.0".into(),
                protocol: 2
            }),
            "Conectado"
        );
        assert_eq!(
            connection_summary(&down(Reason::NotRunning, 4)),
            "Serviço parado: O serviço ScriptRunner não está em execução. nova tentativa em 4 s."
        );
        assert!(connection_summary(&down(Reason::Protocol("speaks 9".into()), 1)).contains("(speaks 9)"));
        // Never "0 s": a sub-second wait reads as one second.
        assert!(connection_summary(&down(Reason::Lost, 0)).ends_with("1 s."));
    }

    #[test]
    fn every_status_has_a_label_and_a_tone() {
        use ExecutionStatus::*;
        for status in [Queued, Running, Success, Failed, Interrupted, Skipped] {
            assert!(!status_label(status).is_empty());
        }
        assert_eq!(status_tone(Success), Tone::Good);
        assert_eq!(status_tone(Failed), Tone::Bad);
        assert_eq!(status_tone(Running), Tone::Active);
        assert_eq!(status_tone(Interrupted), Tone::Warn);
    }
}
