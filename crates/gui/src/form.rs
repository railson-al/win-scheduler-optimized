//! The task editor's model: what the user types, how it becomes a [`Task`], and what is wrong with it.
//! Pure, no egui (D19).
//!
//! The form holds **text**, exactly as typed, so a half-written number or time never gets lost or
//! "corrected" under the user's fingers. [`TaskForm::to_task`] turns it into a real `Task` or says, field
//! by field, what is wrong. The rules are the daemon's own: the built task goes through
//! `Task::validate` / `Schedule::validate` from `core`, so the form cannot accept what the daemon refuses
//! (a cron expression, an interval out of range); the checks here only add what a form needs on top
//! (a required name, a number that is not a number, which field is at fault).

use std::path::PathBuf;
use std::str::FromStr;

use chrono::{DateTime, NaiveTime, TimeZone, Utc, Weekday};
use scriptrunner_core::{RunAs, Schedule, ScriptKind, Task};

/// Days in the order the editor lists them.
pub const WEEK: [Weekday; 7] = [
    Weekday::Mon,
    Weekday::Tue,
    Weekday::Wed,
    Weekday::Thu,
    Weekday::Fri,
    Weekday::Sat,
    Weekday::Sun,
];

/// Next runs shown under the schedule.
pub const PREVIEW_RUNS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeUnit {
    Seconds,
    Minutes,
    Hours,
}

impl TimeUnit {
    pub fn seconds(self) -> u64 {
        match self {
            Self::Seconds => 1,
            Self::Minutes => 60,
            Self::Hours => 3600,
        }
    }

    /// The largest unit that divides `total` evenly, with the amount in it, so a loaded value reads
    /// `2 h` and not `7200 s`, and converts back without loss.
    pub fn fit(total: u64) -> (u64, Self) {
        for unit in [Self::Hours, Self::Minutes] {
            if total != 0 && total.is_multiple_of(unit.seconds()) {
                return (total / unit.seconds(), unit);
            }
        }
        (total, Self::Seconds)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleKind {
    /// Manual only.
    None,
    Interval,
    Daily,
    Weekly,
    Cron,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleForm {
    pub kind: ScheduleKind,
    pub interval_amount: String,
    pub interval_unit: TimeUnit,
    /// `HH:MM` (or `HH:MM:SS`), for daily and weekly.
    pub time: String,
    /// Monday first, as [`WEEK`].
    pub days: [bool; 7],
    pub cron: String,
}

impl Default for ScheduleForm {
    fn default() -> Self {
        Self {
            kind: ScheduleKind::None,
            interval_amount: "15".into(),
            interval_unit: TimeUnit::Minutes,
            time: "09:00".into(),
            days: [true, true, true, true, true, false, false],
            cron: "0 9 * * 1-5".into(),
        }
    }
}

impl ScheduleForm {
    pub fn from_schedule(schedule: Option<&Schedule>) -> Self {
        let mut form = Self::default();
        match schedule {
            None => {}
            Some(Schedule::Interval { every_secs, .. }) => {
                let (amount, unit) = TimeUnit::fit(*every_secs);
                form.kind = ScheduleKind::Interval;
                form.interval_amount = amount.to_string();
                form.interval_unit = unit;
            }
            Some(Schedule::Daily { time }) => {
                form.kind = ScheduleKind::Daily;
                form.time = format_time(*time);
            }
            Some(Schedule::Weekly { days, time }) => {
                form.kind = ScheduleKind::Weekly;
                form.time = format_time(*time);
                form.days = WEEK.map(|day| days.contains(&day));
            }
            Some(Schedule::Cron { expr }) => {
                form.kind = ScheduleKind::Cron;
                form.cron = expr.clone();
            }
        }
        form
    }

    /// The schedule the form describes, `None` inside `Ok` for "manual only". `previous` is the schedule the
    /// task had: an interval that keeps its length keeps its anchor, so editing something else in the task
    /// does not shift the moments it fires at.
    pub fn to_schedule(
        &self,
        now: DateTime<Utc>,
        previous: Option<&Schedule>,
    ) -> Result<Option<Schedule>, Vec<FormError>> {
        let schedule = match self.kind {
            ScheduleKind::None => return Ok(None),
            ScheduleKind::Interval => {
                let every_secs = self.interval_secs().map_err(|problem| {
                    vec![FormError {
                        field: Field::ScheduleInterval,
                        problem,
                    }]
                })?;
                let anchor = match previous {
                    Some(Schedule::Interval {
                        every_secs: before,
                        anchor,
                    }) if *before == every_secs => *anchor,
                    _ => now,
                };
                Schedule::Interval { every_secs, anchor }
            }
            ScheduleKind::Daily => Schedule::Daily {
                time: self.parse_time().map_err(|problem| {
                    vec![FormError {
                        field: Field::ScheduleTime,
                        problem,
                    }]
                })?,
            },
            ScheduleKind::Weekly => {
                let mut errors = Vec::new();
                let time = self.parse_time().map_err(|problem| {
                    errors.push(FormError {
                        field: Field::ScheduleTime,
                        problem,
                    })
                });
                let days: Vec<Weekday> = WEEK
                    .iter()
                    .zip(self.days)
                    .filter_map(|(day, on)| on.then_some(*day))
                    .collect();
                if days.is_empty() {
                    errors.push(FormError {
                        field: Field::ScheduleDays,
                        problem: Problem::NoDaysChosen,
                    });
                }
                match time {
                    Ok(time) if errors.is_empty() => Schedule::Weekly { days, time },
                    _ => return Err(errors),
                }
            }
            ScheduleKind::Cron => Schedule::Cron {
                expr: self.cron.trim().to_owned(),
            },
        };
        // The daemon's own rules (an interval's range, whether the cron expression parses).
        schedule.validate().map_err(|_| {
            vec![match schedule {
                Schedule::Cron { .. } => FormError {
                    field: Field::ScheduleCron,
                    problem: Problem::BadCron,
                },
                _ => FormError {
                    field: Field::ScheduleInterval,
                    problem: Problem::OutOfRange,
                },
            }]
        })?;
        Ok(Some(schedule))
    }

    fn interval_secs(&self) -> Result<u64, Problem> {
        let amount = parse_positive(&self.interval_amount)?;
        amount
            .checked_mul(self.interval_unit.seconds())
            .ok_or(Problem::OutOfRange)
    }

    fn parse_time(&self) -> Result<NaiveTime, Problem> {
        let text = self.time.trim();
        NaiveTime::parse_from_str(text, "%H:%M:%S")
            .or_else(|_| NaiveTime::parse_from_str(text, "%H:%M"))
            .map_err(|_| Problem::BadTime)
    }
}

/// Which field a problem belongs to, so the editor can show it next to the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    ScriptPath,
    Timeout,
    ScheduleInterval,
    ScheduleTime,
    ScheduleDays,
    ScheduleCron,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    Required,
    /// The script's extension is not one the executor knows how to run.
    UnknownScriptType,
    NotANumber,
    MustBePositive,
    /// Valid text, but outside what the daemon accepts (or too big to represent).
    OutOfRange,
    BadTime,
    NoDaysChosen,
    BadCron,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormError {
    pub field: Field,
    pub problem: Problem,
}

/// A task as it is being edited.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskForm {
    pub name: String,
    pub description: String,
    pub script_path: String,
    /// One argument per line, so an argument with spaces needs no quoting.
    pub args: String,
    pub interpreter: String,
    pub working_dir: String,
    /// Empty means no time limit.
    pub timeout_amount: String,
    pub timeout_unit: TimeUnit,
    pub allow_concurrent: bool,
    pub capture_logs: bool,
    /// Where the script runs: as the service, or in the logged-on user's session (visible windows).
    pub run_as: RunAs,
    pub schedule_enabled: bool,
    pub schedule: ScheduleForm,
    /// The task being edited, `None` for a new one. What the form does not show (id, creation time) is
    /// taken from it.
    base: Option<Task>,
}

impl TaskForm {
    /// A blank form with the same defaults as `Task::new`.
    pub fn new_task() -> Self {
        let defaults = Task::new("", "");
        Self::from_parts(&defaults, None)
    }

    pub fn from_task(task: &Task) -> Self {
        Self::from_parts(task, Some(task.clone()))
    }

    fn from_parts(task: &Task, base: Option<Task>) -> Self {
        let (timeout_amount, timeout_unit) = match task.timeout_secs {
            Some(secs) => {
                let (amount, unit) = TimeUnit::fit(secs);
                (amount.to_string(), unit)
            }
            None => (String::new(), TimeUnit::Minutes),
        };
        Self {
            name: task.name.clone(),
            description: task.description.clone().unwrap_or_default(),
            script_path: task.script_path.to_string_lossy().into_owned(),
            args: task.args.join("\n"),
            interpreter: task
                .interpreter
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            working_dir: task
                .working_dir
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            timeout_amount,
            timeout_unit,
            allow_concurrent: task.allow_concurrent,
            capture_logs: task.capture_logs,
            run_as: task.run_as,
            schedule_enabled: task.schedule_enabled,
            schedule: ScheduleForm::from_schedule(task.schedule.as_ref()),
            base,
        }
    }

    /// `true` for a task that is being edited, `false` for a new one.
    pub fn is_edit(&self) -> bool {
        self.base.is_some()
    }

    /// The script type the path's extension means, shown next to the field.
    pub fn script_kind(&self) -> Option<ScriptKind> {
        ScriptKind::from_path(std::path::Path::new(self.script_path.trim()))
    }

    /// Everything wrong with the form right now (empty when it can be saved).
    pub fn errors(&self) -> Vec<FormError> {
        self.build(Utc::now()).err().unwrap_or_default()
    }

    /// The task the form describes, or every problem found.
    pub fn to_task(&self, now: DateTime<Utc>) -> Result<Task, Vec<FormError>> {
        self.build(now)
    }

    /// The next runs of the schedule as it is typed, for the preview. Empty when there is no schedule or it
    /// is not valid yet.
    pub fn next_runs<Tz: TimeZone>(&self, now: DateTime<Utc>, zone: &Tz, count: usize) -> Vec<DateTime<Utc>> {
        let previous = self.base.as_ref().and_then(|task| task.schedule.as_ref());
        match self.schedule.to_schedule(now, previous) {
            Ok(Some(schedule)) => preview(&schedule, now, zone, count),
            _ => Vec::new(),
        }
    }

    fn build(&self, now: DateTime<Utc>) -> Result<Task, Vec<FormError>> {
        let mut errors = Vec::new();

        let name = self.name.trim();
        if name.is_empty() {
            errors.push(FormError {
                field: Field::Name,
                problem: Problem::Required,
            });
        }

        let script_path = self.script_path.trim();
        let script_kind = ScriptKind::from_path(std::path::Path::new(script_path));
        if script_path.is_empty() {
            errors.push(FormError {
                field: Field::ScriptPath,
                problem: Problem::Required,
            });
        } else if script_kind.is_none() {
            errors.push(FormError {
                field: Field::ScriptPath,
                problem: Problem::UnknownScriptType,
            });
        }

        let timeout_secs = match self.timeout_secs() {
            Ok(timeout) => timeout,
            Err(problem) => {
                errors.push(FormError {
                    field: Field::Timeout,
                    problem,
                });
                None
            }
        };

        let previous = self.base.as_ref().and_then(|task| task.schedule.as_ref());
        let schedule = match self.schedule.to_schedule(now, previous) {
            Ok(schedule) => schedule,
            Err(found) => {
                errors.extend(found);
                None
            }
        };

        let Some(script_kind) = script_kind else {
            return Err(errors);
        };
        if !errors.is_empty() {
            return Err(errors);
        }

        let mut task = self.base.clone().unwrap_or_else(|| Task::new(name, script_path));
        task.name = name.to_owned();
        task.description = non_empty(&self.description);
        task.script_path = PathBuf::from(script_path);
        task.script_kind = script_kind;
        task.args = self
            .args
            .lines()
            .map(|line| line.trim_end_matches('\r'))
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect();
        task.interpreter = non_empty(&self.interpreter).map(PathBuf::from);
        task.working_dir = non_empty(&self.working_dir).map(PathBuf::from);
        task.timeout_secs = timeout_secs;
        task.allow_concurrent = self.allow_concurrent;
        task.capture_logs = self.capture_logs;
        task.run_as = self.run_as;
        task.schedule_enabled = self.schedule_enabled;
        task.schedule = schedule;
        task.updated_at = now;

        // The daemon's own check. Anything it refuses that the checks above missed is reported on the
        // field it most likely concerns, rather than letting the daemon bounce the save.
        task.validate().map_err(|_| {
            vec![FormError {
                field: Field::Name,
                problem: Problem::OutOfRange,
            }]
        })?;
        Ok(task)
    }

    fn timeout_secs(&self) -> Result<Option<u64>, Problem> {
        if self.timeout_amount.trim().is_empty() {
            return Ok(None);
        }
        let amount = parse_positive(&self.timeout_amount)?;
        amount
            .checked_mul(self.timeout_unit.seconds())
            .map(Some)
            .ok_or(Problem::OutOfRange)
    }
}

/// The next `count` runs of `schedule` after `now`, evaluating wall-clock rules in `zone` (D18).
pub fn preview<Tz: TimeZone>(schedule: &Schedule, now: DateTime<Utc>, zone: &Tz, count: usize) -> Vec<DateTime<Utc>> {
    let mut runs = Vec::with_capacity(count);
    let mut after = now;
    while runs.len() < count {
        match schedule.next_after(after, zone) {
            Some(next) => {
                runs.push(next);
                after = next;
            }
            None => break,
        }
    }
    runs
}

fn parse_positive(text: &str) -> Result<u64, Problem> {
    let text = text.trim();
    if text.is_empty() {
        return Err(Problem::Required);
    }
    let number = u64::from_str(text).map_err(|_| Problem::NotANumber)?;
    if number == 0 {
        return Err(Problem::MustBePositive);
    }
    Ok(number)
}

fn non_empty(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn format_time(time: NaiveTime) -> String {
    use chrono::Timelike;
    if time.second() == 0 {
        time.format("%H:%M").to_string()
    } else {
        time.format("%H:%M:%S").to_string()
    }
}

#[cfg(test)]
mod tests {
    use chrono::FixedOffset;

    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    fn valid() -> TaskForm {
        let mut form = TaskForm::new_task();
        form.name = "Backup".into();
        form.script_path = r"C:\scripts\backup.py".into();
        form
    }

    fn problems(form: &TaskForm) -> Vec<(Field, Problem)> {
        form.errors().into_iter().map(|e| (e.field, e.problem)).collect()
    }

    #[test]
    fn a_blank_form_asks_for_a_name_and_a_script() {
        let form = TaskForm::new_task();
        assert_eq!(
            problems(&form),
            [(Field::Name, Problem::Required), (Field::ScriptPath, Problem::Required)]
        );
        assert!(!form.is_edit());
    }

    #[test]
    fn a_minimal_form_becomes_a_manual_task_with_the_defaults() {
        let task = valid().to_task(at("2026-11-12T12:00:00Z")).unwrap();
        assert_eq!(task.name, "Backup");
        assert_eq!(task.script_kind, ScriptKind::Python);
        assert_eq!(task.schedule, None);
        assert!(task.capture_logs && !task.allow_concurrent && task.schedule_enabled);
        // Runs as the service unless the user asks for their session: nothing changes for existing tasks.
        assert_eq!(task.run_as, RunAs::Service);
        assert_eq!(task.timeout_secs, None);
        assert_eq!(task.args, Vec::<String>::new());
    }

    #[test]
    fn the_script_type_comes_from_the_extension() {
        let mut form = valid();
        for (path, kind) in [
            ("a.BAT", ScriptKind::Bat),
            ("a.cmd", ScriptKind::Cmd),
            ("a.ps1", ScriptKind::PowerShell),
            ("a.exe", ScriptKind::Exe),
        ] {
            form.script_path = path.into();
            assert_eq!(form.script_kind(), Some(kind));
            assert_eq!(form.to_task(Utc::now()).unwrap().script_kind, kind);
        }
        form.script_path = "notes.txt".into();
        assert_eq!(problems(&form), [(Field::ScriptPath, Problem::UnknownScriptType)]);
    }

    #[test]
    fn text_fields_are_trimmed_and_empty_ones_become_none() {
        let mut form = valid();
        form.name = "  Backup  ".into();
        form.description = "   ".into();
        form.interpreter = "  C:\\venv\\python.exe ".into();
        form.working_dir = "".into();
        let task = form.to_task(Utc::now()).unwrap();
        assert_eq!(task.name, "Backup");
        assert_eq!(task.description, None);
        assert_eq!(task.interpreter, Some(PathBuf::from("C:\\venv\\python.exe")));
        assert_eq!(task.working_dir, None);
    }

    #[test]
    fn arguments_are_one_per_line_and_keep_their_spaces() {
        let mut form = valid();
        form.args = "--name\nJoão da Silva\r\n\n  \n--verbose\n".into();
        let task = form.to_task(Utc::now()).unwrap();
        assert_eq!(task.args, ["--name", "João da Silva", "--verbose"]);
    }

    #[test]
    fn the_timeout_is_optional_positive_and_converted_to_seconds() {
        let mut form = valid();
        form.timeout_amount = "".into();
        assert_eq!(form.to_task(Utc::now()).unwrap().timeout_secs, None);

        form.timeout_amount = "30".into();
        form.timeout_unit = TimeUnit::Minutes;
        assert_eq!(form.to_task(Utc::now()).unwrap().timeout_secs, Some(1800));

        form.timeout_unit = TimeUnit::Hours;
        form.timeout_amount = "2".into();
        assert_eq!(form.to_task(Utc::now()).unwrap().timeout_secs, Some(7200));

        for (text, problem) in [
            ("0", Problem::MustBePositive),
            ("abc", Problem::NotANumber),
            ("-5", Problem::NotANumber),
            ("1.5", Problem::NotANumber),
            ("18446744073709551615", Problem::OutOfRange),
        ] {
            form.timeout_amount = text.into();
            assert_eq!(problems(&form), [(Field::Timeout, problem)], "for {text:?}");
        }
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let mut form = TaskForm::new_task();
        form.timeout_amount = "x".into();
        form.schedule.kind = ScheduleKind::Weekly;
        form.schedule.time = "25:99".into();
        form.schedule.days = [false; 7];
        let found = problems(&form);
        assert!(found.contains(&(Field::Name, Problem::Required)));
        assert!(found.contains(&(Field::ScriptPath, Problem::Required)));
        assert!(found.contains(&(Field::Timeout, Problem::NotANumber)));
        assert!(found.contains(&(Field::ScheduleTime, Problem::BadTime)));
        assert!(found.contains(&(Field::ScheduleDays, Problem::NoDaysChosen)));
    }

    // ---- schedule ----

    #[test]
    fn the_four_schedule_forms_build_the_right_schedule() {
        let now = at("2026-11-12T12:00:00Z");
        let mut form = valid();

        form.schedule.kind = ScheduleKind::Interval;
        form.schedule.interval_amount = "15".into();
        form.schedule.interval_unit = TimeUnit::Minutes;
        assert_eq!(
            form.to_task(now).unwrap().schedule,
            Some(Schedule::Interval {
                every_secs: 900,
                anchor: now
            })
        );

        form.schedule.kind = ScheduleKind::Daily;
        form.schedule.time = "9:30".into();
        assert_eq!(
            form.to_task(now).unwrap().schedule,
            Some(Schedule::Daily {
                time: NaiveTime::from_hms_opt(9, 30, 0).unwrap()
            })
        );

        form.schedule.kind = ScheduleKind::Weekly;
        form.schedule.time = "08:05:30".into();
        form.schedule.days = [true, false, true, false, true, false, false];
        assert_eq!(
            form.to_task(now).unwrap().schedule,
            Some(Schedule::Weekly {
                days: vec![Weekday::Mon, Weekday::Wed, Weekday::Fri],
                time: NaiveTime::from_hms_opt(8, 5, 30).unwrap()
            })
        );

        form.schedule.kind = ScheduleKind::Cron;
        form.schedule.cron = "  0 9 * * 1-5 ".into();
        assert_eq!(
            form.to_task(now).unwrap().schedule,
            Some(Schedule::Cron {
                expr: "0 9 * * 1-5".into()
            })
        );
    }

    #[test]
    fn schedule_mistakes_are_pinned_to_their_field() {
        let mut form = valid();
        form.schedule.kind = ScheduleKind::Interval;
        for (amount, problem) in [
            ("", Problem::Required),
            ("0", Problem::MustBePositive),
            ("x", Problem::NotANumber),
            // Ten years is the daemon's ceiling; one hour more is refused.
            ("87601", Problem::OutOfRange),
        ] {
            form.schedule.interval_amount = amount.into();
            form.schedule.interval_unit = TimeUnit::Hours;
            assert_eq!(problems(&form), [(Field::ScheduleInterval, problem)], "for {amount:?}");
        }

        form.schedule.kind = ScheduleKind::Daily;
        form.schedule.time = "7pm".into();
        assert_eq!(problems(&form), [(Field::ScheduleTime, Problem::BadTime)]);

        form.schedule.kind = ScheduleKind::Cron;
        form.schedule.cron = "not a cron".into();
        assert_eq!(problems(&form), [(Field::ScheduleCron, Problem::BadCron)]);
    }

    #[test]
    fn a_task_survives_a_trip_through_the_form_unchanged() {
        let mut task = Task::new("Relatório", r"C:\Meus Scripts\rel.ps1");
        task.description = Some("todo dia".into());
        task.args = vec!["--out".into(), "C:\\Pasta com espaço".into()];
        task.interpreter = Some(PathBuf::from(r"C:\pwsh\pwsh.exe"));
        task.working_dir = Some(PathBuf::from(r"C:\Meus Scripts"));
        task.timeout_secs = Some(5400);
        task.allow_concurrent = true;
        task.capture_logs = false;
        task.run_as = RunAs::UserSession;
        task.schedule_enabled = false;

        for schedule in [
            None,
            Some(Schedule::Interval {
                every_secs: 7200,
                anchor: at("2026-01-01T00:00:00Z"),
            }),
            Some(Schedule::Interval {
                every_secs: 90,
                anchor: at("2026-01-01T00:00:07Z"),
            }),
            Some(Schedule::Daily {
                time: NaiveTime::from_hms_opt(23, 59, 0).unwrap(),
            }),
            Some(Schedule::Weekly {
                days: vec![Weekday::Tue, Weekday::Sun],
                time: NaiveTime::from_hms_opt(6, 0, 0).unwrap(),
            }),
            Some(Schedule::Cron {
                expr: "*/5 * * * *".into(),
            }),
        ] {
            task.schedule = schedule;
            let form = TaskForm::from_task(&task);
            assert!(form.is_edit());
            let mut back = form.to_task(task.updated_at).unwrap();
            // Weekly days come back in week order, not in the order they were stored.
            if let Some(Schedule::Weekly { days, .. }) = &mut back.schedule {
                days.sort_by_key(|d| d.num_days_from_monday());
            }
            let mut expected = task.clone();
            if let Some(Schedule::Weekly { days, .. }) = &mut expected.schedule {
                days.sort_by_key(|d| d.num_days_from_monday());
            }
            assert_eq!(back, expected, "for {:?}", task.schedule);
        }
    }

    #[test]
    fn editing_other_fields_keeps_an_intervals_anchor_but_changing_its_length_resets_it() {
        let anchor = at("2026-01-01T00:00:00Z");
        let mut task = Task::new("t", "a.py");
        task.schedule = Some(Schedule::Interval {
            every_secs: 600,
            anchor,
        });
        let now = at("2026-11-12T12:00:00Z");

        let mut form = TaskForm::from_task(&task);
        form.name = "renamed".into();
        assert_eq!(form.to_task(now).unwrap().schedule, task.schedule);

        form.schedule.interval_amount = "20".into();
        assert_eq!(
            form.to_task(now).unwrap().schedule,
            Some(Schedule::Interval {
                every_secs: 1200,
                anchor: now
            })
        );
    }

    #[test]
    fn editing_keeps_the_identity_of_the_task() {
        let task = Task::new("t", "a.py");
        let mut form = TaskForm::from_task(&task);
        form.name = "novo nome".into();
        let saved = form.to_task(at("2030-01-01T00:00:00Z")).unwrap();
        assert_eq!(saved.id, task.id);
        assert_eq!(saved.created_at, task.created_at);
        assert_eq!(saved.updated_at, at("2030-01-01T00:00:00Z"));
    }

    #[test]
    fn units_fit_to_the_largest_that_divides_evenly() {
        assert_eq!(TimeUnit::fit(7200), (2, TimeUnit::Hours));
        assert_eq!(TimeUnit::fit(5400), (90, TimeUnit::Minutes));
        assert_eq!(TimeUnit::fit(45), (45, TimeUnit::Seconds));
        assert_eq!(TimeUnit::fit(0), (0, TimeUnit::Seconds));
    }

    // ---- preview ----

    #[test]
    fn the_preview_lists_the_next_runs_in_the_local_zone() {
        let brasilia = FixedOffset::west_opt(3 * 3600).unwrap();
        let mut form = valid();
        form.schedule.kind = ScheduleKind::Daily;
        form.schedule.time = "09:00".into();
        // 12:00 UTC is 09:00 in Brasília: 09:00 has just been reached, so the next is tomorrow.
        let now = at("2026-11-12T12:00:00Z");
        let runs = form.next_runs(now, &brasilia, PREVIEW_RUNS);
        assert_eq!(
            runs,
            [
                at("2026-11-13T12:00:00Z"),
                at("2026-11-14T12:00:00Z"),
                at("2026-11-15T12:00:00Z"),
                at("2026-11-16T12:00:00Z"),
                at("2026-11-17T12:00:00Z"),
            ]
        );
    }

    #[test]
    fn an_interval_previews_from_its_anchor_and_no_schedule_previews_nothing() {
        let mut form = valid();
        assert!(form.next_runs(Utc::now(), &Utc, PREVIEW_RUNS).is_empty());

        form.schedule.kind = ScheduleKind::Interval;
        form.schedule.interval_amount = "10".into();
        let now = at("2026-11-12T12:00:00Z");
        let runs = form.next_runs(now, &Utc, 3);
        assert_eq!(
            runs,
            [
                at("2026-11-12T12:10:00Z"),
                at("2026-11-12T12:20:00Z"),
                at("2026-11-12T12:30:00Z")
            ]
        );
    }

    #[test]
    fn an_invalid_schedule_previews_nothing() {
        let mut form = valid();
        form.schedule.kind = ScheduleKind::Cron;
        form.schedule.cron = "garbage".into();
        assert!(form.next_runs(Utc::now(), &Utc, PREVIEW_RUNS).is_empty());
    }

    #[test]
    fn a_form_is_dirty_exactly_when_it_differs_from_where_it_started() {
        let task = Task::new("t", "a.py");
        let initial = TaskForm::from_task(&task);
        let mut form = initial.clone();
        assert_eq!(form, initial);
        form.args.push('x');
        assert_ne!(form, initial);
    }
}
