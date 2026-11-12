//! The execution detail and live log screen (task 3.8).
//!
//! The header says what the execution was and how it went. Below it is the execution's log, which the
//! backend reads and streams as the script writes it ([`LogView`]). The screen only draws: what it
//! decides (whether a run can be stopped, how long it has taken, what an export result says, which lines
//! pass the filter) is done by the functions below, which need no window and are tested directly.

use std::fmt::Display;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Local, TimeZone, Utc};
use eframe::egui::{self, RichText, TextStyle};
use scriptrunner_core::logs::{LogEntry, LogKind};
use scriptrunner_core::{Execution, ExecutionId, ExecutionStatus};

use super::{Outcome, theme, ui};
use crate::backend::{Action, Command, Failure};
use crate::fmt::{self, Tone};
use crate::i18n::log_view::{Text, t};
use crate::i18n::{Text as Shared, t as shared};
use crate::state::{AppState, LogView, NoticeKind, Screen, StreamFilter};

/// Characters Windows does not allow in a file name.
const FORBIDDEN_IN_FILE_NAME: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
/// Longest task name used in a suggested file name; the date follows it.
const MAX_FILE_STEM_CHARS: usize = 80;
/// How often a running execution's clock is redrawn.
const CLOCK_TICK: StdDuration = StdDuration::from_secs(1);

/// The execution screen's own state. The log itself is in [`AppState::log`].
#[derive(Default)]
pub struct LogScreen {
    /// The execution the screen was showing last. When another one is open, what is kept below starts again.
    shown: Option<ExecutionId>,
    /// The highest notice id already read for `shown`.
    notice_seen: u64,
    /// The result of the last export of `shown`, until another execution is opened.
    result: Option<ExportResult>,
    /// A save dialog on its own thread, and the execution it is for. Its answer comes back on the channel.
    export: Option<(ExecutionId, Receiver<Option<PathBuf>>)>,
    rows: VisibleRows,
}

impl LogScreen {
    pub fn show(&mut self, ui: &mut egui::Ui, state: &mut AppState) -> Outcome {
        let mut outcome = Outcome::default();
        self.follow_open_execution(state);
        self.read_notices(state);
        self.poll_export(&mut outcome);
        let connected = state.is_connected();

        // The page keeps the log clear of the window's edges; the log itself fills the rest.
        let retry = ui::page(ui, ui::FULL_WIDTH, |ui| {
            self.contents(ui, state, connected, &mut outcome)
        });
        if retry {
            for command in state.retry_log() {
                outcome.send(command);
            }
        }
        outcome
    }

    /// The screen's contents. Returns `true` when the user asks to retry following the log.
    fn contents(&mut self, ui: &mut egui::Ui, state: &mut AppState, connected: bool, outcome: &mut Outcome) -> bool {
        let Some(log) = state.log.as_mut() else {
            ui::header(ui, t(Text::Title), None, |_| {});
            ui.label(t(Text::NoExecutionOpen));
            if ui.button(t(Text::Back)).clicked() {
                outcome.goto(Screen::History);
            }
            return false;
        };

        let now = Utc::now();
        self.actions(ui, log, connected, outcome);
        match &log.execution {
            Some(execution) => header(ui, execution, now),
            None if !log.missing => {
                ui.label(t(Text::Loading));
            }
            None => {}
        }
        if let Some(result) = &self.result {
            let color = match result {
                ExportResult::Saved { .. } => theme::tone_color(Tone::Good, ui.visuals().dark_mode),
                ExportResult::Failed(_) => theme::tone_color(Tone::Bad, ui.visuals().dark_mode),
            };
            ui.label(RichText::new(result_text(result)).color(color));
        }
        if self.export.is_some() {
            ui.label(t(Text::ExportPending));
        }
        let retry = state_line(ui, log);
        filters(ui, log);
        if log.dropped > 0 {
            let warn = theme::tone_color(Tone::Warn, ui.visuals().dark_mode);
            ui.label(RichText::new(dropped_text(log.dropped)).color(warn));
        }
        self.body(ui, log);
        if log
            .execution
            .as_ref()
            .is_some_and(|execution| execution.status == ExecutionStatus::Running)
        {
            // The clock of a running execution only moves when the screen is drawn again.
            ui.ctx().request_repaint_after(CLOCK_TICK);
        }
        retry
    }

    /// On the first frame for an execution, or after another one was opened, starts this screen's own
    /// state again. Notices from before the screen was shown are not its business.
    fn follow_open_execution(&mut self, state: &AppState) {
        let Some(log) = &state.log else { return };
        if self.shown.as_ref() == Some(&log.id) {
            return;
        }
        self.shown = Some(log.id.clone());
        self.notice_seen = latest_notice_id(state);
        self.result = None;
        self.export = None;
    }

    /// Reads the notices that came since the last frame and keeps the export result of this execution.
    fn read_notices(&mut self, state: &AppState) {
        let Some(log) = &state.log else { return };
        for notice in state.notices_since(self.notice_seen) {
            self.notice_seen = notice.id;
            if let Some(result) = export_result(&notice.kind, &log.id) {
                self.result = Some(result);
            }
        }
    }

    /// Takes the answer of the save dialog, if it has come, and asks the backend to write the file.
    fn poll_export(&mut self, outcome: &mut Outcome) {
        let Some((_, receiver)) = &self.export else { return };
        let answer = match receiver.try_recv() {
            Ok(answer) => answer,
            Err(TryRecvError::Empty) => return,
            // The dialog's thread ended without answering: treat it as cancelled.
            Err(TryRecvError::Disconnected) => None,
        };
        if let Some((id, _)) = self.export.take()
            && let Some(path) = answer
        {
            outcome.send(Command::ExportLog { id, path });
        }
    }

    /// Opens the save dialog on a thread of its own: it blocks until the user answers, and the UI must not wait.
    fn start_export(&mut self, ctx: &egui::Context, id: ExecutionId, file_name: String) {
        let (sender, receiver) = mpsc::channel();
        let ctx = ctx.clone();
        let filter_name = t(Text::FileFilterLog);
        thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_file_name(file_name)
                .add_filter(filter_name, &["log", "txt"])
                .save_file();
            // If the screen was left meanwhile nobody waits for the answer, and the send just fails.
            let _ = sender.send(picked);
            ctx.request_repaint();
        });
        self.export = Some((id, receiver));
    }

    fn actions(&mut self, ui: &mut egui::Ui, log: &LogView, connected: bool, outcome: &mut Outcome) {
        let exporting = self.export.is_some();
        let file_name = log.execution.as_ref().map(|execution| {
            suggested_file_name(
                &execution.task_name,
                execution.started_at.unwrap_or(execution.created_at),
                &Local,
            )
        });
        ui.horizontal_wrapped(|ui| {
            if ui.button(t(Text::Back)).clicked() {
                outcome.goto(Screen::History);
            }
            if let Some(execution) = &log.execution
                && can_stop(execution.status)
                && ui
                    .add_enabled(connected, egui::Button::new(shared(Shared::Stop)))
                    .clicked()
            {
                // The backend stops every queued and running execution of the task, not only this one.
                outcome.send(Command::Stop(execution.task_id.clone()));
            }
            if ui.button(shared(Shared::Copy)).clicked() {
                ui.ctx().copy_text(log.visible_text());
            }
            let can_export = connected && !exporting && file_name.is_some();
            if ui
                .add_enabled(can_export, egui::Button::new(t(Text::ExportToFile)))
                .clicked()
                && let Some(name) = file_name
            {
                self.start_export(ui.ctx(), log.id.clone(), name);
            }
            if !connected {
                ui.label(shared(Shared::NotConnected));
            }
        });
    }

    fn body(&mut self, ui: &mut egui::Ui, log: &LogView) {
        let visible = self.rows.refresh(log);
        if visible.is_empty() {
            ui.label(t(if log.entries.is_empty() {
                Text::NoLines
            } else {
                Text::NoLinesMatch
            }));
            return;
        }
        let dark = ui.visuals().dark_mode;
        let plain = ui.visuals().text_color();
        // Every row has the same height, so only the rows on screen need drawing, even for a long log.
        let row_height = ui.text_style_height(&TextStyle::Monospace);
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .stick_to_bottom(log.stick_to_bottom)
            .show_rows(ui, row_height, visible.len(), |ui, range| {
                for &index in &visible[range] {
                    let Some(entry) = log.entries.get(index) else { continue };
                    let color = line_tone(entry.kind).map_or(plain, |tone| theme::tone_color(tone, dark));
                    // Never wrapped: a wrapped line would be taller than its row and throw the others off.
                    let text = RichText::new(line_text(entry, &Local)).monospace().color(color);
                    ui.add(egui::Label::new(text).wrap_mode(egui::TextWrapMode::Extend));
                }
            });
    }
}

/// The header: task, status and the execution's times and results.
fn header(ui: &mut egui::Ui, execution: &Execution, now: DateTime<Utc>) {
    ui.heading(execution.task_name.as_str());
    let dark = ui.visuals().dark_mode;
    ui.horizontal(|ui| {
        ui.label(t(Text::Status));
        ui.label(theme::status_text(ui, execution.status));
    });
    egui::Grid::new("log_header_details")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            for (label, value) in detail_rows(execution, now, &Local) {
                ui.label(t(label));
                ui.label(value);
                ui.end_row();
            }
            if let Some(reason) = &execution.failure_reason {
                ui.label(t(Text::FailureReason));
                let bad = theme::tone_color(Tone::Bad, dark);
                ui.label(RichText::new(reason.as_str()).strong().color(bad));
                ui.end_row();
            }
        });
    if execution.log_truncated {
        let warn = theme::tone_color(Tone::Warn, dark);
        ui.label(RichText::new(t(Text::LogTruncated)).color(warn));
    }
}

/// The line that says whether the log is still being followed. Returns `true` when the user asks to retry.
fn state_line(ui: &mut egui::Ui, log: &LogView) -> bool {
    let state = log_state(log);
    let dark = ui.visuals().dark_mode;
    let mut retry = false;
    ui.horizontal(|ui| {
        if state == LogState::Live {
            ui.spinner();
        }
        if let LogState::Failed(_) = state {
            let bad = theme::tone_color(Tone::Bad, dark);
            ui.label(RichText::new(state_text(&state)).color(bad));
            retry = ui.button(t(Text::RetryFollowing)).clicked();
        } else {
            ui.label(state_text(&state));
        }
    });
    retry
}

fn filters(ui: &mut egui::Ui, log: &mut LogView) {
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut log.streams.stdout, t(Text::ShowOutput));
        ui.checkbox(&mut log.streams.stderr, t(Text::ShowErrors));
        ui.checkbox(&mut log.streams.meta, t(Text::ShowMeta));
        ui.checkbox(&mut log.stick_to_bottom, t(Text::KeepScrolled));
    });
}

// ---- rules: no window needed ----

/// How long an execution has run, or ran. `None` before it started (still queued). A running one counts up
/// to `now`. Never negative: a clock that moved back reads as no time.
pub fn elapsed(execution: &Execution, now: DateTime<Utc>) -> Option<Duration> {
    let started = execution.started_at?;
    let span = match execution.finished_at {
        Some(_) => execution.duration()?,
        None => now - started,
    };
    Some(span.max(Duration::zero()))
}

/// [`elapsed`] as text, or the "no value" mark while queued.
pub fn duration_text(execution: &Execution, now: DateTime<Utc>) -> String {
    match elapsed(execution, now) {
        Some(span) => fmt::duration(span),
        None => t(Text::NoValue).to_owned(),
    }
}

/// `<task>-<date>.log`, for the save dialog. Characters Windows forbids become `_`, the name is cut to a
/// reasonable length, and a task with no usable name falls back to a fixed word.
pub fn suggested_file_name<Tz>(task_name: &str, at: DateTime<Utc>, zone: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    let cleaned: String = task_name
        .chars()
        .map(|c| {
            if FORBIDDEN_IN_FILE_NAME.contains(&c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .take(MAX_FILE_STEM_CHARS)
        .collect();
    // Windows does not accept a name that ends in a dot or a space.
    let stem = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    let stem = if stem.is_empty() {
        t(Text::DefaultFileName)
    } else {
        stem
    };
    let date = at.with_timezone(zone).format("%Y-%m-%d_%H-%M-%S");
    format!("{stem}-{date}.log")
}

/// The colour of a log line by where it came from. `None` is the normal text colour: the script's own
/// output is not coloured, so that a red line can only mean an error stream.
pub fn line_tone(kind: LogKind) -> Option<Tone> {
    match kind {
        LogKind::Stdout => None,
        LogKind::Stderr => Some(Tone::Bad),
        LogKind::Meta => Some(Tone::Neutral),
    }
}

/// The tag that marks where a line came from, as in the plain-text export.
const fn kind_tag(kind: LogKind) -> &'static str {
    match kind {
        LogKind::Stdout => "OUT",
        LogKind::Stderr => "ERR",
        LogKind::Meta => "---",
    }
}

/// One line on screen: time, origin, text. Never empty, even for a blank line of the script, because the
/// time and the tag come first.
pub fn line_text<Tz>(entry: &LogEntry, zone: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    format!(
        "{} {} {}",
        fmt::log_time(entry.at, zone),
        kind_tag(entry.kind),
        entry.text
    )
}

/// What the status line of the log says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogState {
    /// The daemon has no such execution.
    Missing,
    /// Following failed; the message is shown with a retry.
    Failed(String),
    /// The execution is over and its whole log has been read.
    Finished,
    /// Still being followed.
    Live,
}

/// Which state the log is in. A missing execution comes first; a failure hides whether the log had finished.
pub fn log_state(log: &LogView) -> LogState {
    if log.missing {
        LogState::Missing
    } else if let Some(failure) = &log.failure {
        LogState::Failed(failure_text(failure))
    } else if log.finished {
        LogState::Finished
    } else {
        LogState::Live
    }
}

pub fn state_text(state: &LogState) -> String {
    match state {
        LogState::Missing => t(Text::NotFound).to_owned(),
        LogState::Failed(message) => message.clone(),
        LogState::Finished => t(Text::Complete).to_owned(),
        LogState::Live => t(Text::Live).to_owned(),
    }
}

/// A failure in words. The daemon's own message is in English, but it is the only one there is.
pub fn failure_text(failure: &Failure) -> String {
    match failure {
        Failure::NotConnected => shared(Shared::NotConnected).to_owned(),
        Failure::Remote { message, .. } => message.clone(),
        Failure::Other(message) => message.clone(),
    }
}

/// Only a run that has not finished can be stopped.
pub fn can_stop(status: ExecutionStatus) -> bool {
    matches!(status, ExecutionStatus::Queued | ExecutionStatus::Running)
}

/// The result of a save of this screen's log, as the screen shows it.
#[derive(Debug, Clone, PartialEq)]
pub enum ExportResult {
    Saved { path: PathBuf, bytes: u64 },
    Failed(String),
}

/// What a notice says about the export of `log`. Exports of other executions, and notices of other kinds,
/// say nothing to this screen.
pub fn export_result(kind: &NoticeKind, log: &ExecutionId) -> Option<ExportResult> {
    match kind {
        NoticeKind::Exported { id, path, bytes } if id == log => Some(ExportResult::Saved {
            path: path.clone(),
            bytes: *bytes,
        }),
        NoticeKind::Error {
            action: Action::ExportLog,
            failure,
        } => Some(ExportResult::Failed(failure_text(failure))),
        _ => None,
    }
}

pub fn result_text(result: &ExportResult) -> String {
    match result {
        ExportResult::Saved { path, bytes } => fill(
            t(Text::Exported),
            &[("{path}", path.display().to_string()), ("{size}", fmt::bytes(*bytes))],
        ),
        ExportResult::Failed(reason) => fill(t(Text::ExportFailed), &[("{reason}", reason.clone())]),
    }
}

fn dropped_text(count: usize) -> String {
    fill(t(Text::DroppedLines), &[("{count}", count.to_string())])
}

/// The header rows: label and value. Status, failure reason and truncation are drawn apart, because they
/// have their own colours.
pub fn detail_rows<Tz>(execution: &Execution, now: DateTime<Utc>, zone: &Tz) -> Vec<(Text, String)>
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    let when = |at: Option<DateTime<Utc>>| match at {
        Some(at) => fmt::date_time(at, zone),
        None => t(Text::NoValue).to_owned(),
    };
    let mut rows = vec![(Text::Trigger, fmt::trigger_label(execution.trigger).to_owned())];
    if let Some(slot) = execution.scheduled_for {
        rows.push((Text::ScheduledFor, fmt::date_time(slot, zone)));
    }
    rows.push((Text::Started, when(execution.started_at)));
    rows.push((Text::Finished, when(execution.finished_at)));
    rows.push((Text::Duration, duration_text(execution, now)));
    rows.push((
        Text::ExitCode,
        match execution.exit_code {
            Some(code) => code.to_string(),
            None => t(Text::NoValue).to_owned(),
        },
    ));
    rows.push((Text::LogSize, fmt::bytes(execution.log_bytes)));
    rows
}

/// Replaces each `{name}` in `template` with its value.
fn fill(template: &str, values: &[(&str, String)]) -> String {
    values
        .iter()
        .fold(template.to_owned(), |text, (key, value)| text.replace(key, value))
}

fn latest_notice_id(state: &AppState) -> u64 {
    state.notices_since(0).last().map_or(0, |notice| notice.id)
}

/// The indices, into `LogView::entries`, of the lines the stream filter lets through. Kept between frames:
/// rebuilding walks the whole log, so it happens only when what they depend on has changed.
#[derive(Default)]
struct VisibleRows {
    key: Option<RowsKey>,
    indices: Vec<usize>,
}

#[derive(PartialEq)]
struct RowsKey {
    /// Another log can have as many lines as this one, so the id is part of the key too.
    id: ExecutionId,
    len: usize,
    dropped: usize,
    streams: StreamFilter,
}

impl VisibleRows {
    fn refresh(&mut self, log: &LogView) -> &[usize] {
        self.update(log);
        &self.indices
    }

    /// Brings the rows up to date. Returns `true` when they had to be rebuilt.
    fn update(&mut self, log: &LogView) -> bool {
        let key = RowsKey {
            id: log.id.clone(),
            len: log.entries.len(),
            dropped: log.dropped,
            streams: log.streams,
        };
        if self.key.as_ref() == Some(&key) {
            return false;
        }
        self.indices = log
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| log.streams.shows(entry.kind))
            .map(|(index, _)| index)
            .collect();
        self.key = Some(key);
        true
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use std::time::Instant;

    use scriptrunner_core::{Task, Trigger};

    use super::*;
    use crate::backend::{ConnectionState, LogUpdate, Update};
    use crate::views::testing::frame;
    use scriptrunner_core::logs::LogChunk;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 12, 10, 0, 0).unwrap()
    }

    fn run(status: ExecutionStatus, started: Option<DateTime<Utc>>, finished: Option<DateTime<Utc>>) -> Execution {
        let mut execution = Execution::new(&Task::new("Backup noturno", "backup.ps1"), Trigger::Manual, None);
        execution.status = status;
        execution.started_at = started;
        execution.finished_at = finished;
        execution
    }

    fn entry(kind: LogKind, text: &str) -> LogEntry {
        LogEntry {
            at: t0(),
            kind,
            text: text.into(),
        }
    }

    /// A state with `id` open on its execution screen.
    fn open(id: &ExecutionId) -> AppState {
        let mut state = AppState::default();
        state.connection = ConnectionState::Connected {
            version: "0.1.0".into(),
            protocol: 2,
        };
        state.go(Screen::Execution(id.clone()));
        state
    }

    /// Appends lines to the open log, the way the backend delivers them.
    fn feed(state: &mut AppState, id: &ExecutionId, entries: Vec<LogEntry>) {
        let chunk = LogChunk {
            next_offset: state.log.as_ref().map_or(0, |log| log.next_offset) + entries.len() as u64 + 1,
            entries,
            eof: true,
        };
        state.apply(Update::Log(LogUpdate::Chunk { id: id.clone(), chunk }), Instant::now());
    }

    fn set_header(state: &mut AppState, execution: Execution) {
        let id = execution.id.clone();
        state.apply(
            Update::ExecutionLoaded {
                id,
                execution: Some(execution),
            },
            Instant::now(),
        );
    }

    // ---- rules ----

    #[test]
    fn a_concluded_run_shows_the_time_it_recorded_whatever_the_clock_says() {
        let execution = run(
            ExecutionStatus::Success,
            Some(t0()),
            Some(t0() + Duration::milliseconds(4_200)),
        );
        assert_eq!(duration_text(&execution, t0() + Duration::hours(1)), "4,2 s");
    }

    #[test]
    fn a_running_run_counts_up_to_now() {
        let execution = run(ExecutionStatus::Running, Some(t0()), None);
        assert_eq!(duration_text(&execution, t0() + Duration::seconds(75)), "1 min 15 s");
    }

    #[test]
    fn a_queued_run_has_no_time_yet() {
        let execution = run(ExecutionStatus::Queued, None, None);
        assert_eq!(duration_text(&execution, t0()), t(Text::NoValue));
        assert_eq!(elapsed(&execution, t0()), None);
    }

    #[test]
    fn a_clock_that_moved_back_reads_as_zero_not_negative() {
        let running = run(ExecutionStatus::Running, Some(t0()), None);
        assert_eq!(duration_text(&running, t0() - Duration::seconds(5)), "0 ms");
    }

    #[test]
    fn the_suggested_name_has_no_character_windows_forbids() {
        assert_eq!(
            suggested_file_name("Backup: noturno/2?", t0(), &Utc),
            "Backup_ noturno_2_-2026-11-12_10-00-00.log"
        );
        assert_eq!(suggested_file_name("a\tb", t0(), &Utc), "a_b-2026-11-12_10-00-00.log");
    }

    #[test]
    fn the_suggested_name_falls_back_when_the_task_has_no_usable_name() {
        let fallback = t(Text::DefaultFileName);
        assert_eq!(
            suggested_file_name("", t0(), &Utc),
            format!("{fallback}-2026-11-12_10-00-00.log")
        );
        assert_eq!(
            suggested_file_name("...  ", t0(), &Utc),
            format!("{fallback}-2026-11-12_10-00-00.log")
        );
    }

    #[test]
    fn the_suggested_name_drops_trailing_dots_and_spaces_and_caps_the_length() {
        assert_eq!(
            suggested_file_name("Relatório. ", t0(), &Utc),
            "Relatório-2026-11-12_10-00-00.log"
        );
        let long = "a".repeat(200);
        let name = suggested_file_name(&long, t0(), &Utc);
        let stem = name.strip_suffix("-2026-11-12_10-00-00.log").unwrap();
        assert_eq!(stem.chars().count(), MAX_FILE_STEM_CHARS);
    }

    #[test]
    fn an_error_stream_is_red_and_meta_is_neutral_and_script_output_is_plain() {
        assert_eq!(line_tone(LogKind::Stdout), None);
        assert_eq!(line_tone(LogKind::Stderr), Some(Tone::Bad));
        assert_eq!(line_tone(LogKind::Meta), Some(Tone::Neutral));
    }

    #[test]
    fn a_blank_line_still_has_its_time_and_tag() {
        let line = line_text(&entry(LogKind::Stdout, ""), &Utc);
        assert_eq!(line, "10:00:00.000 OUT ");
        assert!(!line.trim().is_empty());
    }

    #[test]
    fn only_a_queued_or_running_run_can_be_stopped() {
        assert!(can_stop(ExecutionStatus::Queued));
        assert!(can_stop(ExecutionStatus::Running));
        for status in [
            ExecutionStatus::Success,
            ExecutionStatus::Failed,
            ExecutionStatus::Interrupted,
            ExecutionStatus::Skipped,
        ] {
            assert!(!can_stop(status), "{status:?}");
        }
    }

    #[test]
    fn the_rows_on_screen_are_rebuilt_only_when_their_key_changes() {
        let id = ExecutionId::new();
        let mut state = open(&id);
        feed(
            &mut state,
            &id,
            vec![
                entry(LogKind::Stdout, "a"),
                entry(LogKind::Stderr, "b"),
                entry(LogKind::Meta, "c"),
                entry(LogKind::Stdout, "d"),
            ],
        );
        let mut rows = VisibleRows::default();
        let log = state.log.as_ref().unwrap();
        assert!(rows.update(log), "the first frame builds the rows");
        assert_eq!(rows.indices, [0, 1, 2, 3]);
        assert!(!rows.update(log), "an unchanged log is not walked again");

        state.log.as_mut().unwrap().streams.stderr = false;
        assert!(rows.update(state.log.as_ref().unwrap()));
        assert_eq!(rows.indices, [0, 2, 3]);
        assert!(!rows.update(state.log.as_ref().unwrap()));

        feed(&mut state, &id, vec![entry(LogKind::Stdout, "e")]);
        assert!(rows.update(state.log.as_ref().unwrap()), "a new line changes the key");
        assert_eq!(rows.indices, [0, 2, 3, 4]);
    }

    #[test]
    fn another_log_with_as_many_lines_is_not_mistaken_for_the_same_rows() {
        let first = ExecutionId::new();
        let mut state = open(&first);
        feed(&mut state, &first, vec![entry(LogKind::Stdout, "x")]);
        let mut rows = VisibleRows::default();
        rows.update(state.log.as_ref().unwrap());

        let second = ExecutionId::new();
        state.go(Screen::Dashboard);
        state.go(Screen::Execution(second.clone()));
        feed(&mut state, &second, vec![entry(LogKind::Stdout, "y")]);
        assert!(rows.update(state.log.as_ref().unwrap()));
    }

    #[test]
    fn an_export_result_is_taken_only_for_its_own_execution() {
        let mine = ExecutionId::new();
        let other = ExecutionId::new();
        let saved = |id: &ExecutionId| NoticeKind::Exported {
            id: id.clone(),
            path: PathBuf::from("a.log"),
            bytes: 2_048,
        };
        assert_eq!(
            export_result(&saved(&mine), &mine),
            Some(ExportResult::Saved {
                path: PathBuf::from("a.log"),
                bytes: 2_048
            })
        );
        assert_eq!(export_result(&saved(&other), &mine), None);
    }

    #[test]
    fn an_export_failure_is_shown_and_other_errors_are_not() {
        let mine = ExecutionId::new();
        let failed = NoticeKind::Error {
            action: Action::ExportLog,
            failure: Failure::Other("disco cheio".into()),
        };
        assert_eq!(
            export_result(&failed, &mine),
            Some(ExportResult::Failed("disco cheio".into()))
        );
        let other_error = NoticeKind::Error {
            action: Action::QueryHistory,
            failure: Failure::Other("x".into()),
        };
        assert_eq!(export_result(&other_error, &mine), None);
    }

    #[test]
    fn other_notices_say_nothing_to_the_log_screen() {
        let mine = ExecutionId::new();
        assert_eq!(export_result(&NoticeKind::Done(Action::ExportLog), &mine), None);
        assert_eq!(export_result(&NoticeKind::Done(Action::Stop), &mine), None);
    }

    #[test]
    fn the_export_result_names_the_file_and_its_size() {
        let result = ExportResult::Saved {
            path: PathBuf::from("a.log"),
            bytes: 2_048,
        };
        assert_eq!(result_text(&result), "Exportado para a.log (2,0 KB)");
    }

    #[test]
    fn the_log_state_line_says_live_complete_failed_or_missing() {
        let id = ExecutionId::new();
        let mut state = open(&id);
        let log = state.log.as_mut().unwrap();
        assert_eq!(state_text(&log_state(log)), "acompanhando ao vivo");

        log.finished = true;
        assert_eq!(state_text(&log_state(log)), "log completo");

        log.failure = Some(Failure::NotConnected);
        assert_eq!(state_text(&log_state(log)), "Sem conexão com o serviço.");

        log.missing = true;
        assert_eq!(state_text(&log_state(log)), "Execução não encontrada.");
    }

    #[test]
    fn a_failure_message_is_the_daemons_own() {
        assert_eq!(failure_text(&Failure::Other("boom".into())), "boom");
    }

    #[test]
    fn the_details_show_a_dash_for_what_has_not_happened_yet() {
        let execution = run(ExecutionStatus::Queued, None, None);
        let rows = detail_rows(&execution, t0(), &Utc);
        let value = |key: Text| rows.iter().find(|(label, _)| *label == key).map(|(_, v)| v.clone());
        assert_eq!(value(Text::Started).as_deref(), Some("—"));
        assert_eq!(value(Text::Finished).as_deref(), Some("—"));
        assert_eq!(value(Text::Duration).as_deref(), Some("—"));
        assert_eq!(value(Text::ExitCode).as_deref(), Some("—"));
        assert_eq!(value(Text::ScheduledFor), None);
    }

    #[test]
    fn a_scheduled_run_shows_the_slot_it_was_for() {
        let mut execution = run(ExecutionStatus::Queued, None, None);
        execution.trigger = Trigger::Scheduled;
        execution.scheduled_for = Some(t0());
        let rows = detail_rows(&execution, t0(), &Utc);
        assert!(
            rows.iter()
                .any(|(label, value)| *label == Text::ScheduledFor && value == "12/11/2026 10:00:00")
        );
    }

    // ---- drawing ----

    fn draw(screen: &mut LogScreen, state: &mut AppState) {
        frame(|ui| {
            screen.show(ui, state);
        });
    }

    #[test]
    fn it_draws_without_an_open_execution() {
        let mut screen = LogScreen::default();
        let mut state = AppState::default();
        draw(&mut screen, &mut state);
    }

    #[test]
    fn it_draws_while_the_header_is_loading() {
        let id = ExecutionId::new();
        let mut screen = LogScreen::default();
        let mut state = open(&id);
        draw(&mut screen, &mut state);
    }

    #[test]
    fn it_draws_a_concluded_run_with_five_thousand_lines_without_trouble() {
        let id = ExecutionId::new();
        let mut screen = LogScreen::default();
        let mut state = open(&id);
        let mut execution = run(ExecutionStatus::Failed, Some(t0()), Some(t0() + Duration::seconds(90)));
        execution.id = id.clone();
        execution.exit_code = Some(3);
        execution.failure_reason = Some("código de saída 3".into());
        execution.log_bytes = 5 * 1024 * 1024;
        execution.log_truncated = true;
        set_header(&mut state, execution);
        let lines = (0..5_000)
            .map(|n| match n % 3 {
                0 => entry(LogKind::Stdout, &format!("linha {n}")),
                1 => entry(LogKind::Stderr, &format!("erro {n}")),
                _ => entry(LogKind::Meta, ""),
            })
            .collect();
        feed(&mut state, &id, lines);
        state.log.as_mut().unwrap().finished = true;
        draw(&mut screen, &mut state);
        assert_eq!(screen.rows.indices.len(), 5_000);
    }

    #[test]
    fn it_draws_a_run_in_progress_and_a_followed_log_that_failed() {
        let id = ExecutionId::new();
        let mut screen = LogScreen::default();
        let mut state = open(&id);
        set_header(&mut state, run(ExecutionStatus::Running, Some(t0()), None));
        feed(&mut state, &id, vec![entry(LogKind::Stdout, "em andamento")]);
        draw(&mut screen, &mut state);

        state.log.as_mut().unwrap().failure = Some(Failure::Other("o serviço fechou a conexão".into()));
        draw(&mut screen, &mut state);
    }

    #[test]
    fn it_draws_a_missing_execution_and_a_log_with_dropped_lines() {
        let id = ExecutionId::new();
        let mut screen = LogScreen::default();
        let mut state = open(&id);
        state.log.as_mut().unwrap().missing = true;
        draw(&mut screen, &mut state);

        let id = ExecutionId::new();
        state.go(Screen::Dashboard);
        state.go(Screen::Execution(id.clone()));
        feed(&mut state, &id, vec![entry(LogKind::Stdout, "resta uma")]);
        state.log.as_mut().unwrap().dropped = 12_345;
        draw(&mut screen, &mut state);
    }

    #[test]
    fn it_draws_when_the_filter_hides_every_line_and_when_lines_are_blank() {
        let id = ExecutionId::new();
        let mut screen = LogScreen::default();
        let mut state = open(&id);
        feed(
            &mut state,
            &id,
            vec![entry(LogKind::Stdout, ""), entry(LogKind::Stderr, "")],
        );
        let log = state.log.as_mut().unwrap();
        log.streams = StreamFilter {
            stdout: false,
            stderr: false,
            meta: false,
        };
        draw(&mut screen, &mut state);
        assert!(screen.rows.indices.is_empty());

        state.log.as_mut().unwrap().streams = StreamFilter::default();
        draw(&mut screen, &mut state);
        assert_eq!(screen.rows.indices.len(), 2);
    }

    #[test]
    fn an_export_result_appears_for_its_execution_and_goes_when_another_is_opened() {
        let id = ExecutionId::new();
        let mut screen = LogScreen::default();
        let mut state = open(&id);
        draw(&mut screen, &mut state);

        state.apply(
            Update::Exported {
                id: id.clone(),
                path: PathBuf::from("a.log"),
                bytes: 10,
            },
            Instant::now(),
        );
        draw(&mut screen, &mut state);
        assert_eq!(
            screen.result,
            Some(ExportResult::Saved {
                path: PathBuf::from("a.log"),
                bytes: 10
            })
        );

        let next = ExecutionId::new();
        state.go(Screen::Dashboard);
        state.go(Screen::Execution(next));
        draw(&mut screen, &mut state);
        assert_eq!(screen.result, None);
    }
}
