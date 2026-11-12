//! The history screen (task 3.7): the executions of every task, newest first, one page at a time.
//!
//! The filter, the page and the rows live in [`AppState`]; this screen draws them and reports what the
//! user picked. Two things are the screen's own. The period shortcut: the filter stores a fixed instant
//! (`since`), so the shortcut is kept here, and the instant moves only when the user picks a shortcut. And
//! the clear-history confirmation, which is open or not for this screen alone.
//!
//! Those two are kept in egui's memory under one fixed id rather than in fields of [`HistoryView`]: the
//! shell builds this view as a plain value, and the view itself has nothing else to keep.

use chrono::{DateTime, Duration, Local, TimeZone, Utc};
use eframe::egui;
use scriptrunner_core::{Execution, ExecutionStatus, TaskId, TaskOverview};

use super::Outcome;
use super::theme;
use crate::backend::{Command, Failure};
use crate::fmt::{self, Tone};
use crate::i18n::history::{Text, t};
use crate::i18n::{Text as Common, t as common};
use crate::state::{AppState, HistoryFilter, Screen};

/// Shown where a value does not exist (a running execution has no duration yet).
const NO_VALUE: &str = "—";

const STATUSES: [ExecutionStatus; 6] = [
    ExecutionStatus::Queued,
    ExecutionStatus::Running,
    ExecutionStatus::Success,
    ExecutionStatus::Failed,
    ExecutionStatus::Interrupted,
    ExecutionStatus::Skipped,
];

/// The shortcuts of the period selector. `All` has no start; the others start a fixed span before now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Period {
    #[default]
    All,
    Last24Hours,
    Last7Days,
    Last30Days,
}

impl Period {
    const ALL: [Period; 4] = [Period::All, Period::Last24Hours, Period::Last7Days, Period::Last30Days];

    /// The earliest moment this shortcut admits, as of `now`. It is taken once, when the user picks the
    /// shortcut. A start that kept moving would change the filter on its own and query again every frame.
    pub fn since(self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let span = match self {
            Period::All => return None,
            Period::Last24Hours => Duration::hours(24),
            Period::Last7Days => Duration::days(7),
            Period::Last30Days => Duration::days(30),
        };
        Some(now - span)
    }

    fn label(self) -> &'static str {
        t(match self {
            Period::All => Text::PeriodAll,
            Period::Last24Hours => Text::PeriodLast24Hours,
            Period::Last7Days => Text::PeriodLast7Days,
            Period::Last30Days => Text::PeriodLast30Days,
        })
    }
}

/// What the three selectors show after this frame's clicks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub task: Option<TaskId>,
    pub status: Option<ExecutionStatus>,
    pub period: Period,
    /// Whether the user picked the period this frame. Only then does `since` move.
    pub period_changed: bool,
}

/// The filter the selectors ask for, or `None` when nothing the user did changes it.
///
/// Returning `None` for an unchanged selection is what stops a feedback loop: the screen runs every frame,
/// and a filter that were rebuilt every frame would be sent again every frame.
pub fn filter_after(current: &HistoryFilter, picked: &Selection, now: DateTime<Utc>) -> Option<HistoryFilter> {
    let since = if picked.period_changed {
        picked.period.since(now)
    } else {
        current.since
    };
    let next = HistoryFilter {
        task: picked.task.clone(),
        status: picked.status,
        since,
        until: current.until,
    };
    (next != *current).then_some(next)
}

/// The line above the list that says what the list is doing. Rows stay on screen whatever it says.
#[derive(Debug, PartialEq, Eq)]
pub enum Banner<'a> {
    Failed(&'a Failure),
    /// A query is in flight. It is shown beside the rows it will replace, not instead of them.
    Loading,
    Empty,
    Ready,
}

/// Which line to show. A failure comes first, so it is never hidden behind a spinner. "Empty" only
/// counts once nothing is loading, so a list that is still arriving is not reported as empty.
pub fn banner(loading: bool, failure: Option<&Failure>, rows: usize) -> Banner<'_> {
    if let Some(failure) = failure {
        Banner::Failed(failure)
    } else if loading {
        Banner::Loading
    } else if rows == 0 {
        Banner::Empty
    } else {
        Banner::Ready
    }
}

/// The text for a failed query. The daemon's own message is kept as it is.
pub fn failure_text(failure: &Failure) -> String {
    match failure {
        Failure::NotConnected => common(Common::NotConnected).to_owned(),
        Failure::Remote { message, .. } => format!("{} {message}", t(Text::LoadFailed)),
        Failure::Other(message) => format!("{} {message}", t(Text::LoadFailed)),
    }
}

/// When the execution started, or when it was registered if it never reached the point of starting
/// (a queued run, or one skipped before its process was spawned).
pub fn started_text<Tz>(execution: &Execution, zone: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: std::fmt::Display,
{
    fmt::date_time(execution.started_at.unwrap_or(execution.created_at), zone)
}

/// How long the execution took. Empty until it has finished: a running one has no duration yet.
pub fn duration_text(execution: &Execution) -> String {
    execution.duration().map_or_else(|| NO_VALUE.to_owned(), fmt::duration)
}

pub fn exit_code_text(execution: &Execution) -> String {
    execution
        .exit_code
        .map_or_else(|| NO_VALUE.to_owned(), |code| code.to_string())
}

/// The name of a task, from the task list or, for a deleted task, from an execution still on screen.
/// Executions keep their task's name at the time they ran, so the history never needs the task to exist.
pub fn known_task_name<'a>(id: &TaskId, tasks: &'a [TaskOverview], rows: &'a [Execution]) -> Option<&'a str> {
    tasks
        .iter()
        .find(|row| row.task.id == *id)
        .map(|row| row.task.name.as_str())
        .or_else(|| {
            rows.iter()
                .find(|row| row.task_id == *id)
                .map(|row| row.task_name.as_str())
        })
}

/// What the task selector shows: "Todas", or the chosen task's name.
fn task_label(task: Option<&TaskId>, tasks: &[TaskOverview], rows: &[Execution]) -> String {
    match task {
        None => t(Text::AllTasks).to_owned(),
        Some(id) => known_task_name(id, tasks, rows)
            .unwrap_or(t(Text::UnknownTask))
            .to_owned(),
    }
}

/// What the user has confirmed or is still deciding about clearing one task's history.
#[derive(Debug, Clone)]
struct PendingClear {
    task: TaskId,
    /// Kept from when the user clicked, so the dialog still names the task if it was deleted meanwhile.
    name: String,
}

/// The screen's own state, kept between frames in egui's memory.
#[derive(Debug, Clone, Default)]
struct Memo {
    period: Period,
    pending_clear: Option<PendingClear>,
}

#[derive(Default)]
pub struct HistoryView;

impl HistoryView {
    pub fn show(&mut self, ui: &mut egui::Ui, state: &mut AppState) -> Outcome {
        let mut outcome = Outcome::default();
        let memo_id = egui::Id::unique("history_screen");
        let mut memo: Memo = ui.ctx().data_mut(|data| data.get_temp(memo_id)).unwrap_or_default();

        ui.heading(t(Text::Title));
        filter_bar(ui, state, &mut memo, &mut outcome);
        ui.separator();
        // The footer is added before the list: a panel at the bottom takes its space first, and the list
        // fills whatever is left.
        pages_footer(ui, state, &mut outcome);
        banner_line(ui, state);
        // Both directions: a long task name or failure reason makes the table wider than the window, and it
        // must stay reachable.
        egui::ScrollArea::both().show(ui, |ui| table(ui, state, &mut outcome));
        clear_dialog(ui.ctx(), &mut memo, &mut outcome);

        ui.ctx().data_mut(|data| data.insert_temp(memo_id, memo));
        outcome
    }
}

fn filter_bar(ui: &mut egui::Ui, state: &mut AppState, memo: &mut Memo, outcome: &mut Outcome) {
    let current = state.history.filter.clone();
    // The selectors edit copies. The filter itself changes only through `set_filter`, and only if the
    // selection really differs from it.
    let mut task = current.task.clone();
    let mut status = current.status;
    let mut period = memo.period;

    ui.horizontal_wrapped(|ui| {
        ui.label(common(Common::ColName));
        egui::ComboBox::from_id_salt("history_task")
            .selected_text(task_label(task.as_ref(), &state.tasks, &state.history.rows))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut task, None, t(Text::AllTasks));
                for row in &state.tasks {
                    ui.selectable_value(&mut task, Some(row.task.id.clone()), row.task.name.as_str());
                }
            });

        ui.label(t(Text::ColStatus));
        egui::ComboBox::from_id_salt("history_status")
            .selected_text(status.map_or(t(Text::AllStatuses), fmt::status_label))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut status, None, t(Text::AllStatuses));
                for option in STATUSES {
                    ui.selectable_value(&mut status, Some(option), fmt::status_label(option));
                }
            });

        ui.label(t(Text::FilterPeriod));
        egui::ComboBox::from_id_salt("history_period")
            .selected_text(period.label())
            .show_ui(ui, |ui| {
                for option in Period::ALL {
                    ui.selectable_value(&mut period, option, option.label());
                }
            });

        if ui.button(common(Common::Refresh)).clicked() {
            outcome.send(state.history.request());
        }
        // Clearing applies to one task, so the button is only offered once a task is chosen.
        let can_clear = current.task.is_some();
        if ui
            .add_enabled(can_clear, egui::Button::new(t(Text::ClearHistory)))
            .clicked()
            && let Some(id) = current.task.clone()
        {
            let name = known_task_name(&id, &state.tasks, &state.history.rows)
                .unwrap_or(t(Text::UnknownTask))
                .to_owned();
            memo.pending_clear = Some(PendingClear { task: id, name });
        }
    });

    let picked = Selection {
        task,
        status,
        period,
        period_changed: period != memo.period,
    };
    memo.period = period;
    if let Some(next) = filter_after(&current, &picked, Utc::now()) {
        outcome.send(state.history.set_filter(next));
    }
}

fn pages_footer(ui: &mut egui::Ui, state: &mut AppState, outcome: &mut Outcome) {
    egui::Panel::bottom("history_pages").show(ui, |ui| {
        ui.horizontal(|ui| {
            let can_go_back = state.history.page > 0;
            if ui
                .add_enabled(can_go_back, egui::Button::new(t(Text::PagePrevious)))
                .clicked()
                && let Some(command) = state.history.previous_page()
            {
                outcome.send(command);
            }
            ui.label(format!("{} {}", t(Text::PageNumber), state.history.page + 1));
            if ui
                .add_enabled(state.history.has_next, egui::Button::new(t(Text::PageNext)))
                .clicked()
                && let Some(command) = state.history.next_page()
            {
                outcome.send(command);
            }
        });
    });
}

fn banner_line(ui: &mut egui::Ui, state: &AppState) {
    match banner(
        state.history.loading,
        state.history.failure.as_ref(),
        state.history.rows.len(),
    ) {
        Banner::Failed(failure) => {
            let color = theme::tone_color(Tone::Bad, ui.visuals().dark_mode);
            ui.label(egui::RichText::new(failure_text(failure)).color(color));
        }
        Banner::Loading => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(t(Text::Loading));
            });
        }
        Banner::Empty => {
            ui.label(t(Text::Empty));
            ui.small(t(Text::EmptyHint));
        }
        Banner::Ready => {}
    }
}

fn table(ui: &mut egui::Ui, state: &AppState, outcome: &mut Outcome) {
    egui::Grid::new("history_table")
        .striped(true)
        .spacing([16.0, 6.0])
        .show(ui, |ui| {
            ui.strong(common(Common::ColName));
            ui.strong(t(Text::ColTrigger));
            ui.strong(t(Text::ColStatus));
            ui.strong(t(Text::ColStarted));
            ui.strong(t(Text::ColDuration));
            ui.strong(t(Text::ColExitCode));
            ui.label("");
            ui.end_row();

            for execution in &state.history.rows {
                ui.label(execution.task_name.as_str());
                ui.label(fmt::trigger_label(execution.trigger));
                let status = ui.label(theme::status_text(ui, execution.status));
                // The reason is a hover hint on the status: it is rarely short, and most rows have none.
                if let Some(reason) = &execution.failure_reason {
                    status.on_hover_text(reason.as_str());
                }
                ui.label(started_text(execution, &Local));
                ui.label(duration_text(execution));
                ui.label(exit_code_text(execution));
                // The id keeps each row's button distinct; otherwise every "Ver" would share one.
                ui.push_id(&execution.id, |ui| {
                    if ui.button(t(Text::OpenExecution)).clicked() {
                        outcome.goto(Screen::Execution(execution.id.clone()));
                    }
                });
                ui.end_row();
            }
        });
}

fn clear_dialog(ctx: &egui::Context, memo: &mut Memo, outcome: &mut Outcome) {
    let Some(pending) = memo.pending_clear.clone() else {
        return;
    };
    let response = egui::Modal::new(egui::Id::unique("history_clear")).show(ctx, |ui| {
        ui.heading(t(Text::ClearTitle));
        ui.label(t(Text::ClearBody));
        ui.add_space(8.0);
        ui.strong(pending.name.as_str());
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let confirm = ui.button(t(Text::ClearConfirm)).clicked();
            let cancel = ui.button(common(Common::Cancel)).clicked();
            (confirm, cancel)
        })
        .inner
    });

    let (confirm, cancel) = response.inner;
    if confirm {
        outcome.send(Command::ClearHistory(pending.task));
        memo.pending_clear = None;
    } else if cancel || response.should_close() {
        memo.pending_clear = None;
    }
}

#[cfg(test)]
mod tests {
    use chrono::FixedOffset;
    use scriptrunner_core::{Task, Trigger};

    use super::*;
    use crate::views::testing;

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    fn execution(task: &Task, status: ExecutionStatus) -> Execution {
        let mut execution = Execution::new(task, Trigger::Manual, None);
        execution.status = status;
        execution
    }

    fn overview(task: &Task) -> TaskOverview {
        TaskOverview {
            task: task.clone(),
            running: false,
            last_execution: None,
            next_run: None,
        }
    }

    // ---- period shortcut ----

    #[test]
    fn each_shortcut_starts_a_fixed_span_before_now() {
        let now = utc("2026-11-12T12:00:00Z");
        assert_eq!(Period::All.since(now), None);
        assert_eq!(Period::Last24Hours.since(now), Some(utc("2026-11-11T12:00:00Z")));
        assert_eq!(Period::Last7Days.since(now), Some(utc("2026-11-05T12:00:00Z")));
        assert_eq!(Period::Last30Days.since(now), Some(utc("2026-10-13T12:00:00Z")));
    }

    // ---- filter built from the selectors ----

    #[test]
    fn an_unchanged_selection_asks_for_nothing() {
        let current = HistoryFilter::default();
        let picked = Selection {
            task: None,
            status: None,
            period: Period::All,
            period_changed: false,
        };
        assert_eq!(filter_after(&current, &picked, Utc::now()), None);
    }

    #[test]
    fn a_picked_task_or_status_is_built_into_the_filter_and_keeps_the_period_start() {
        let task = Task::new("Backup", "a.sh");
        let start = utc("2026-11-11T12:00:00Z");
        let current = HistoryFilter {
            since: Some(start),
            ..HistoryFilter::default()
        };
        let picked = Selection {
            task: Some(task.id.clone()),
            status: Some(ExecutionStatus::Failed),
            period: Period::Last24Hours,
            period_changed: false,
        };
        let next = filter_after(&current, &picked, utc("2026-11-12T18:00:00Z")).unwrap();
        assert_eq!(next.task, Some(task.id));
        assert_eq!(next.status, Some(ExecutionStatus::Failed));
        // The start stays where the period was picked; it does not slide with the clock.
        assert_eq!(next.since, Some(start));
        assert_eq!(next.until, None);
    }

    #[test]
    fn picking_a_period_moves_the_start_once_and_the_next_frame_asks_for_nothing() {
        let now = utc("2026-11-12T12:00:00Z");
        let current = HistoryFilter::default();
        let picked = Selection {
            task: None,
            status: None,
            period: Period::Last7Days,
            period_changed: true,
        };
        let next = filter_after(&current, &picked, now).unwrap();
        assert_eq!(next.since, Some(utc("2026-11-05T12:00:00Z")));

        // Next frame the user has changed nothing: the filter just applied is not sent again, even though
        // the clock has moved on since.
        let later = utc("2026-11-12T13:00:00Z");
        let same = Selection {
            period_changed: false,
            ..picked
        };
        assert_eq!(filter_after(&next, &same, later), None);
    }

    // ---- which message the list shows ----

    #[test]
    fn a_failure_is_shown_before_anything_else() {
        let failure = Failure::NotConnected;
        assert_eq!(banner(true, Some(&failure), 3), Banner::Failed(&failure));
        assert_eq!(banner(false, Some(&failure), 0), Banner::Failed(&failure));
    }

    #[test]
    fn a_query_in_flight_is_a_loading_line_and_is_not_reported_as_empty() {
        assert_eq!(banner(true, None, 0), Banner::Loading);
        assert_eq!(banner(true, None, 5), Banner::Loading);
    }

    #[test]
    fn a_settled_query_with_no_rows_is_empty_and_with_rows_is_ready() {
        assert_eq!(banner(false, None, 0), Banner::Empty);
        assert_eq!(banner(false, None, 1), Banner::Ready);
    }

    #[test]
    fn failure_texts_name_the_daemon_message_or_say_there_is_no_connection() {
        assert_eq!(failure_text(&Failure::NotConnected), common(Common::NotConnected));
        let remote = failure_text(&Failure::Remote {
            code: scriptrunner_ipc::ErrorCode::NotFound,
            message: "tarefa não existe".into(),
        });
        assert!(remote.starts_with(t(Text::LoadFailed)) && remote.ends_with("tarefa não existe"));
        assert!(failure_text(&Failure::Other("boom".into())).ends_with("boom"));
    }

    // ---- columns ----

    #[test]
    fn a_start_without_started_at_falls_back_to_when_it_was_registered() {
        let task = Task::new("t", "a.sh");
        let mut run = execution(&task, ExecutionStatus::Queued);
        run.created_at = utc("2026-11-11T20:50:03Z");
        assert_eq!(started_text(&run, &Utc), "11/11/2026 20:50:03");

        run.started_at = Some(utc("2026-11-11T20:51:00Z"));
        assert_eq!(started_text(&run, &Utc), "11/11/2026 20:51:00");
        let brasilia = FixedOffset::west_opt(3 * 3600).unwrap();
        assert_eq!(started_text(&run, &brasilia), "11/11/2026 17:51:00");
    }

    #[test]
    fn a_duration_shows_only_once_the_execution_has_finished() {
        let task = Task::new("t", "a.sh");
        let mut run = execution(&task, ExecutionStatus::Running);
        run.started_at = Some(utc("2026-11-11T20:00:00Z"));
        assert_eq!(duration_text(&run), NO_VALUE, "still running");

        run.finished_at = Some(utc("2026-11-11T20:00:04Z") + Duration::milliseconds(200));
        run.status = ExecutionStatus::Success;
        assert_eq!(duration_text(&run), "4,2 s");

        run.started_at = None;
        assert_eq!(duration_text(&run), NO_VALUE, "never started");
    }

    #[test]
    fn an_exit_code_shows_its_number_or_a_dash() {
        let task = Task::new("t", "a.sh");
        let mut run = execution(&task, ExecutionStatus::Failed);
        assert_eq!(exit_code_text(&run), NO_VALUE);
        run.exit_code = Some(3);
        assert_eq!(exit_code_text(&run), "3");
    }

    #[test]
    fn a_task_name_comes_from_the_list_first_and_from_the_rows_once_deleted() {
        let task = Task::new("Backup", "a.sh");
        let deleted = Task::new("Antiga", "b.sh");
        let tasks = vec![overview(&task)];
        let rows = vec![execution(&deleted, ExecutionStatus::Success)];
        assert_eq!(known_task_name(&task.id, &tasks, &rows), Some("Backup"));
        assert_eq!(known_task_name(&deleted.id, &tasks, &rows), Some("Antiga"));
        assert_eq!(known_task_name(&TaskId::new(), &tasks, &rows), None);
    }

    // ---- drawing without a window ----

    /// Draws the screen for `state`, with the screen's own memory seeded by `memo` on every frame.
    fn draw(state: &mut AppState, memo: Option<Memo>) {
        let mut view = HistoryView;
        testing::frame(|ui| {
            if let Some(memo) = memo.clone() {
                ui.ctx()
                    .data_mut(|data| data.insert_temp(egui::Id::unique("history_screen"), memo));
            }
            let _ = view.show(ui, state);
        });
    }

    fn with_rows(state: &mut AppState) {
        let task = Task::new("Backup", "a.sh");
        let mut ok = execution(&task, ExecutionStatus::Success);
        ok.started_at = Some(utc("2026-11-11T20:00:00Z"));
        ok.finished_at = Some(utc("2026-11-11T20:00:03Z"));
        ok.exit_code = Some(0);

        let mut failed = execution(&task, ExecutionStatus::Failed);
        failed.started_at = Some(utc("2026-11-11T20:10:00Z"));
        failed.finished_at = Some(utc("2026-11-11T20:10:09Z"));
        failed.exit_code = Some(2);
        failed.failure_reason = Some("código de saída 2".into());

        let mut running = execution(&task, ExecutionStatus::Running);
        running.started_at = Some(utc("2026-11-11T20:20:00Z"));

        let mut never_started = execution(&task, ExecutionStatus::Skipped);
        never_started.started_at = None;

        state.tasks = vec![overview(&task)];
        state.history.rows = vec![ok, failed, running, never_started];
        state.history.has_next = true;
    }

    #[test]
    fn the_default_state_draws() {
        let mut state = AppState::default();
        draw(&mut state, None);
    }

    #[test]
    fn a_loading_query_draws_with_the_old_rows_on_screen() {
        let mut state = AppState::default();
        with_rows(&mut state);
        state.history.loading = true;
        draw(&mut state, None);
    }

    #[test]
    fn a_failed_query_draws_for_no_connection_and_for_a_daemon_refusal() {
        let mut state = AppState::default();
        state.history.failure = Some(Failure::NotConnected);
        draw(&mut state, None);

        state.history.failure = Some(Failure::Remote {
            code: scriptrunner_ipc::ErrorCode::NotFound,
            message: "sem histórico".into(),
        });
        draw(&mut state, None);
    }

    #[test]
    fn an_empty_list_draws() {
        let mut state = AppState::default();
        state.tasks_loaded = true;
        draw(&mut state, None);
    }

    #[test]
    fn rows_of_every_kind_draw() {
        let mut state = AppState::default();
        with_rows(&mut state);
        draw(&mut state, None);
    }

    #[test]
    fn a_chosen_task_draws_with_its_clear_button_enabled() {
        let mut state = AppState::default();
        with_rows(&mut state);
        state.history.filter.task = state.tasks.first().map(|row| row.task.id.clone());
        draw(&mut state, None);
    }

    #[test]
    fn a_pending_clear_draws_its_confirmation() {
        let mut state = AppState::default();
        with_rows(&mut state);
        let task = state.tasks[0].task.id.clone();
        let memo = Memo {
            period: Period::Last7Days,
            pending_clear: Some(PendingClear {
                task,
                name: "Backup".into(),
            }),
        };
        draw(&mut state, Some(memo));
    }
}
