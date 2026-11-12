//! The task list screen (task 3.5): one row per task, with its schedule, its next run, its last result and
//! the actions the user can take on it.
//!
//! The rules (which actions a row offers, what the schedule column says, what an empty list says) are pure
//! functions at the top, tested without a window. The drawing code below only places their results.

use chrono::{Local, TimeZone};
use eframe::egui::{self, Id, RichText};
use scriptrunner_core::{Execution, ExecutionStatus, Schedule, TaskId, TaskOverview};

use super::{Outcome, theme};
use crate::backend::{Command, ConnectionState};
use crate::fmt;
use crate::i18n::dashboard::{Text, t};
use crate::i18n::{self, Lang, Text as Common};
use crate::state::{AppState, Screen};

/// What a schedule button does to a task that has a schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleAction {
    Pause,
    Resume,
}

impl ScheduleAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pause => i18n::t(Common::Pause),
            Self::Resume => i18n::t(Common::Resume),
        }
    }

    /// The `schedule_enabled` value the command carries: pausing switches the schedule off.
    pub fn target_enabled(self) -> bool {
        matches!(self, Self::Resume)
    }
}

/// The actions one row offers, decided from the task's state alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowActions {
    /// Stop is offered only while the task has something running.
    pub can_stop: bool,
    /// Present only for a task with a schedule: pause while the schedule is on, resume while it is off.
    pub schedule: Option<ScheduleAction>,
}

pub fn row_actions(row: &TaskOverview) -> RowActions {
    RowActions {
        can_stop: row.running,
        schedule: row.task.schedule.as_ref().map(|_| {
            if row.task.schedule_enabled {
                ScheduleAction::Pause
            } else {
                ScheduleAction::Resume
            }
        }),
    }
}

/// The Agenda column: the schedule's summary, with `(pausada)` after it while the schedule is off.
pub fn schedule_text(schedule: Option<&Schedule>, enabled: bool) -> String {
    match schedule {
        None => i18n::t(Common::NoSchedule).to_owned(),
        Some(schedule) => {
            let summary = fmt::schedule_summary(Lang::default(), schedule);
            if enabled {
                summary
            } else {
                format!("{summary} ({})", i18n::t(Common::SchedulePaused))
            }
        }
    }
}

/// The line under a result's status: how long it took (only once it has finished), then when it started,
/// or was registered if it never started. Shown in `zone`.
pub fn result_details<Tz>(execution: &Execution, zone: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: std::fmt::Display,
{
    let when = execution.started_at.unwrap_or(execution.created_at);
    let date = fmt::date_time(when, zone);
    match execution.duration() {
        Some(span) => format!("{} · {date}", fmt::duration(span)),
        None => date,
    }
}

/// Why the table is replaced by a message instead of rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyState {
    /// No list from the daemon yet, and the connection is still being made (or is up).
    Loading,
    /// No list from the daemon yet, and the connection is down: the list will not come until it is back.
    Disconnected,
    /// The daemon's list is empty.
    NoTasks,
    /// The list has tasks, but the search text matches none of them.
    NoMatches,
}

/// `None` when the table has rows to show.
pub fn empty_state(state: &AppState) -> Option<EmptyState> {
    if !state.tasks_loaded {
        // "Connecting" is the first attempt, not a failure: only a lost connection is said to be down.
        return Some(match state.connection {
            ConnectionState::Down { .. } => EmptyState::Disconnected,
            ConnectionState::Connecting | ConnectionState::Connected { .. } => EmptyState::Loading,
        });
    }
    if state.tasks.is_empty() {
        return Some(EmptyState::NoTasks);
    }
    if state.visible_tasks().is_empty() {
        return Some(EmptyState::NoMatches);
    }
    None
}

/// The summary above the table: `3 tarefas · 1 executando`.
pub fn summary_line(tasks: usize, running: usize) -> String {
    let noun = if tasks == 1 {
        t(Text::TaskOne)
    } else {
        t(Text::TaskMany)
    };
    format!("{tasks} {noun} · {running} {}", t(Text::RunningLower))
}

/// The question in the delete dialog, with the task's name in it.
pub fn delete_question(name: &str) -> String {
    format!("{} “{name}” {}", t(Text::DeleteBefore), t(Text::DeleteAfter))
}

fn empty_texts(kind: EmptyState) -> (&'static str, Option<&'static str>) {
    match kind {
        EmptyState::Loading => (t(Text::Loading), None),
        EmptyState::Disconnected => (i18n::t(Common::NotConnected), Some(t(Text::DisconnectedHint))),
        EmptyState::NoTasks => (t(Text::NoTasks), Some(t(Text::NoTasksHint))),
        EmptyState::NoMatches => (t(Text::NoMatches), Some(t(Text::NoMatchesHint))),
    }
}

/// The task list screen. It has no fields: the shell builds it as a unit value, so the one thing it must
/// remember (the task whose deletion waits for confirmation) is kept in egui's memory, under an id of
/// this screen. See [`pending_delete`].
#[derive(Default)]
pub struct DashboardView;

impl DashboardView {
    pub fn show(&mut self, ui: &mut egui::Ui, state: &mut AppState) -> Outcome {
        let mut outcome = Outcome::default();
        let connected = state.is_connected();

        ui.horizontal(|ui| {
            ui.heading(t(Text::Title));
            ui.label(i18n::t(Common::Search));
            ui.add(
                egui::TextEdit::singleline(&mut state.search)
                    .hint_text(t(Text::SearchHint))
                    .desired_width(200.0),
            );
            if ui.button(t(Text::NewTask)).clicked() {
                outcome.goto(Screen::TaskEditor { task: None });
            }
            if ui
                .add_enabled(connected, egui::Button::new(i18n::t(Common::Refresh)))
                .clicked()
            {
                outcome.send(Command::Refresh);
            }
        });
        ui.label(summary_line(state.tasks.len(), state.running_count()));
        ui.separator();

        match empty_state(state) {
            Some(kind) => draw_empty(ui, &mut outcome, kind),
            None => {
                let rows = state.visible_tasks();
                table(ui, &mut outcome, &rows, connected);
            }
        }

        delete_dialog(ui, &mut outcome, state);
        outcome
    }
}

/// The rows. A delete request is stored in memory, not sent: the confirmation dialog sends it.
fn table(ui: &mut egui::Ui, outcome: &mut Outcome, rows: &[&TaskOverview], connected: bool) {
    let mut asked_delete = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("dashboard_tasks")
            .striped(true)
            .spacing([18.0, 8.0])
            .show(ui, |ui| {
                for header in [
                    i18n::t(Common::ColName),
                    i18n::t(Common::ColSchedule),
                    i18n::t(Common::ColNextRun),
                    i18n::t(Common::ColLastResult),
                    t(Text::ColActions),
                ] {
                    ui.label(RichText::new(header).strong());
                }
                ui.end_row();

                for row in rows {
                    table_row(ui, outcome, &mut asked_delete, row, connected);
                    ui.end_row();
                }
            });
    });
    if let Some(id) = asked_delete {
        set_pending_delete(ui, Some(id));
    }
}

/// The task whose deletion the user asked for and has not confirmed yet, if any.
fn pending_delete(ui: &egui::Ui) -> Option<TaskId> {
    ui.ctx().data(|data| data.get_temp::<TaskId>(pending_key(ui)))
}

fn set_pending_delete(ui: &egui::Ui, id: Option<TaskId>) {
    let key = pending_key(ui);
    ui.ctx().data_mut(|data| {
        if let Some(id) = id {
            data.insert_temp(key, id);
        } else {
            data.remove::<TaskId>(key);
        }
    });
}

fn pending_key(ui: &egui::Ui) -> Id {
    ui.scope_id().with("dashboard_pending_delete")
}

fn delete_dialog(ui: &mut egui::Ui, outcome: &mut Outcome, state: &AppState) {
    let Some(id) = pending_delete(ui) else {
        return;
    };
    let Some(task) = state.task(&id) else {
        // The task is gone (deleted elsewhere): there is nothing left to confirm.
        set_pending_delete(ui, None);
        return;
    };
    let connected = state.is_connected();

    // `Some(true)` confirms, `Some(false)` cancels; a click outside the dialog cancels too.
    let mut answer = None;
    let modal = egui::Modal::new(ui.scope_id().with("dashboard_delete_modal")).show(ui.ctx(), |ui| {
        ui.set_max_width(360.0);
        ui.heading(t(Text::DeleteTitle));
        ui.label(delete_question(&task.name));
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(connected, egui::Button::new(i18n::t(Common::Delete)))
                .clicked()
            {
                answer = Some(true);
            }
            if ui.button(i18n::t(Common::Cancel)).clicked() {
                answer = Some(false);
            }
        });
    });
    if modal.backdrop_response.clicked() {
        answer = Some(false);
    }

    match answer {
        Some(true) => {
            outcome.send(Command::DeleteTask(id));
            set_pending_delete(ui, None);
        }
        Some(false) => set_pending_delete(ui, None),
        None => {}
    }
}

/// Draws the message that replaces the table.
fn draw_empty(ui: &mut egui::Ui, outcome: &mut Outcome, kind: EmptyState) {
    let (title, hint) = empty_texts(kind);
    ui.add_space(24.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(title).strong());
        if let Some(hint) = hint {
            ui.weak(hint);
        }
        if kind == EmptyState::NoTasks && ui.button(t(Text::NewTask)).clicked() {
            outcome.goto(Screen::TaskEditor { task: None });
        }
    });
}

/// One task's cells. The caller ends the row. Each interactive cell gets its own id scope, so the buttons
/// of one row never clash with those of another.
fn table_row(
    ui: &mut egui::Ui,
    outcome: &mut Outcome,
    asked_delete: &mut Option<TaskId>,
    row: &TaskOverview,
    connected: bool,
) {
    let task = &row.task;

    let name = ui.label(RichText::new(task.name.as_str()).strong());
    if let Some(description) = task.description.as_deref() {
        name.on_hover_text(description);
    }

    ui.label(schedule_text(task.schedule.as_ref(), task.schedule_enabled));

    let next_run = match row.next_run {
        Some(at) => fmt::date_time_local(at),
        None => t(Text::NoValue).to_owned(),
    };
    ui.label(next_run);

    ui.push_id(task.id.as_str(), |ui| {
        ui.vertical(|ui| {
            if row.running {
                ui.label(theme::status_text(ui, ExecutionStatus::Running));
            } else if let Some(execution) = &row.last_execution {
                // The status is a link: it opens the execution it describes.
                if ui.link(theme::status_text(ui, execution.status)).clicked() {
                    outcome.goto(Screen::Execution(execution.id.clone()));
                }
                ui.small(result_details(execution, &Local));
            } else {
                ui.weak(i18n::t(Common::Never));
            }
        });
    });

    let actions = row_actions(row);
    ui.push_id(task.id.as_str(), |ui| {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(connected, egui::Button::new(i18n::t(Common::RunNow)))
                .clicked()
            {
                outcome.send(Command::RunNow(task.id.clone()));
            }
            if actions.can_stop
                && ui
                    .add_enabled(connected, egui::Button::new(i18n::t(Common::Stop)))
                    .clicked()
            {
                outcome.send(Command::Stop(task.id.clone()));
            }
            if let Some(action) = actions.schedule
                && ui.add_enabled(connected, egui::Button::new(action.label())).clicked()
            {
                outcome.send(Command::SetScheduleEnabled {
                    id: task.id.clone(),
                    enabled: action.target_enabled(),
                });
            }
            if ui
                .add_enabled(connected, egui::Button::new(i18n::t(Common::Edit)))
                .clicked()
            {
                outcome.goto(Screen::TaskEditor {
                    task: Some(task.id.clone()),
                });
            }
            if ui
                .add_enabled(connected, egui::Button::new(i18n::t(Common::Duplicate)))
                .clicked()
            {
                outcome.send(Command::DuplicateTask(task.id.clone()));
            }
            // Deleting asks first: the dialog in `delete_dialog` sends the command.
            if ui
                .add_enabled(connected, egui::Button::new(i18n::t(Common::Delete)))
                .clicked()
            {
                *asked_delete = Some(task.id.clone());
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Duration, NaiveTime, Utc};
    use scriptrunner_core::{Task, Trigger};
    use scriptrunner_ipc::PROTOCOL_VERSION;

    use super::*;
    use crate::views::testing;

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    fn daily_nine_thirty() -> Schedule {
        Schedule::Daily {
            time: NaiveTime::from_hms_opt(9, 30, 0).unwrap(),
        }
    }

    fn overview(task: Task, running: bool) -> TaskOverview {
        TaskOverview {
            task,
            running,
            last_execution: None,
            next_run: None,
        }
    }

    fn with_schedule(mut task: Task, schedule: Schedule, enabled: bool) -> Task {
        task.schedule = Some(schedule);
        task.schedule_enabled = enabled;
        task
    }

    fn connected(tasks: Vec<TaskOverview>) -> AppState {
        let mut state = AppState::default();
        state.connection = ConnectionState::Connected {
            version: "0.1.0".into(),
            protocol: PROTOCOL_VERSION,
        };
        state.tasks = tasks;
        state.tasks_loaded = true;
        state
    }

    /// A task list with every kind of row the screen has to draw.
    fn varied_tasks() -> Vec<TaskOverview> {
        let mut running = with_schedule(Task::new("Backup diário", "backup.ps1"), daily_nine_thirty(), true);
        running.description = Some("Copia as pastas de trabalho".into());
        let running_row = overview(running.clone(), true);

        let paused = with_schedule(
            Task::new("Relatório semanal", "report.py"),
            Schedule::every_hours(2, utc("2026-01-01T00:00:00Z")),
            false,
        );
        let mut paused_row = overview(paused.clone(), false);
        let failed = Execution {
            status: ExecutionStatus::Failed,
            started_at: Some(utc("2026-11-11T20:50:03Z")),
            finished_at: Some(utc("2026-11-11T20:50:07Z")),
            ..Execution::new(&paused, Trigger::Scheduled, None)
        };
        paused_row.last_execution = Some(failed);
        paused_row.next_run = None;

        let manual = overview(Task::new("Limpar temporários", "clean.bat"), false);

        vec![running_row, paused_row, manual]
    }

    /// Draws the screen for two frames. `pending` stands for a click on "Excluir" before this frame. Returns
    /// the delete the screen still waits to confirm once the frame is drawn.
    fn draw(state: &mut AppState, pending: Option<TaskId>) -> Option<TaskId> {
        let mut waiting = None;
        testing::frame(|ui| {
            if let Some(id) = &pending {
                set_pending_delete(ui, Some(id.clone()));
            }
            DashboardView.show(ui, state);
            waiting = pending_delete(ui);
        });
        waiting
    }

    // Rules of the rows.

    #[test]
    fn a_running_task_offers_stop_and_a_manual_task_offers_nothing_extra() {
        let running = overview(Task::new("a", "a.ps1"), true);
        let actions = row_actions(&running);
        assert!(actions.can_stop);
        assert_eq!(actions.schedule, None);

        let idle = overview(Task::new("b", "b.ps1"), false);
        assert!(!row_actions(&idle).can_stop);
    }

    #[test]
    fn a_scheduled_task_offers_pause_while_on_and_resume_while_off() {
        let on = overview(with_schedule(Task::new("a", "a.ps1"), daily_nine_thirty(), true), false);
        let actions = row_actions(&on);
        assert_eq!(actions.schedule, Some(ScheduleAction::Pause));
        assert!(!actions.schedule.unwrap().target_enabled());

        let off = overview(
            with_schedule(Task::new("a", "a.ps1"), daily_nine_thirty(), false),
            false,
        );
        let actions = row_actions(&off);
        assert_eq!(actions.schedule, Some(ScheduleAction::Resume));
        assert!(actions.schedule.unwrap().target_enabled());
    }

    #[test]
    fn the_schedule_column_says_manual_or_paused() {
        assert_eq!(schedule_text(None, true), "Somente manual");
        assert_eq!(schedule_text(None, false), "Somente manual");
        assert_eq!(schedule_text(Some(&daily_nine_thirty()), true), "Todo dia às 09:30");
        assert_eq!(
            schedule_text(Some(&daily_nine_thirty()), false),
            "Todo dia às 09:30 (pausada)"
        );
    }

    #[test]
    fn result_details_show_duration_only_once_finished() {
        let task = Task::new("a", "a.ps1");
        let mut execution = Execution::new(&task, Trigger::Manual, None);
        execution.created_at = utc("2026-11-11T20:50:00Z");
        execution.started_at = Some(utc("2026-11-11T20:50:03Z"));
        assert_eq!(result_details(&execution, &Utc), "11/11/2026 20:50:03");

        execution.finished_at = Some(utc("2026-11-11T20:50:07Z") + Duration::milliseconds(300));
        assert_eq!(result_details(&execution, &Utc), "4,3 s · 11/11/2026 20:50:03");
    }

    #[test]
    fn a_result_that_never_started_is_dated_by_its_creation() {
        let task = Task::new("a", "a.ps1");
        let mut execution = Execution::new(&task, Trigger::Manual, None);
        execution.created_at = utc("2026-11-11T20:50:00Z");
        assert_eq!(result_details(&execution, &Utc), "11/11/2026 20:50:00");
    }

    #[test]
    fn the_summary_counts_in_the_right_words() {
        assert_eq!(summary_line(1, 0), "1 tarefa · 0 executando");
        assert_eq!(summary_line(0, 0), "0 tarefas · 0 executando");
        assert_eq!(summary_line(3, 1), "3 tarefas · 1 executando");
    }

    #[test]
    fn the_delete_question_names_the_task() {
        assert_eq!(
            delete_question("Backup"),
            "A tarefa “Backup” será excluída. Esta ação não pode ser desfeita."
        );
    }

    // Which empty message applies.

    #[test]
    fn before_the_first_list_the_message_depends_on_the_connection() {
        let mut state = AppState::default();
        assert_eq!(empty_state(&state), Some(EmptyState::Loading));

        state.connection = ConnectionState::Connecting;
        assert_eq!(empty_state(&state), Some(EmptyState::Loading));

        state.connection = ConnectionState::Down {
            reason: crate::backend::Reason::Lost,
            retry_in: std::time::Duration::from_secs(2),
        };
        assert_eq!(empty_state(&state), Some(EmptyState::Disconnected));

        // The first list is still on its way once the connection is back.
        state.connection = ConnectionState::Connected {
            version: "0.1.0".into(),
            protocol: PROTOCOL_VERSION,
        };
        assert_eq!(empty_state(&state), Some(EmptyState::Loading));
    }

    #[test]
    fn a_loaded_empty_list_says_there_are_no_tasks() {
        let mut state = connected(Vec::new());
        assert_eq!(empty_state(&state), Some(EmptyState::NoTasks));

        // A search cannot make an empty list "no matches": there is nothing to match.
        state.search = "qualquer".into();
        assert_eq!(empty_state(&state), Some(EmptyState::NoTasks));
    }

    #[test]
    fn a_search_with_no_hits_is_told_apart_from_an_empty_list() {
        let mut state = connected(varied_tasks());
        assert_eq!(empty_state(&state), None);

        state.search = "inexistente".into();
        assert_eq!(empty_state(&state), Some(EmptyState::NoMatches));

        state.search = "BACKUP".into();
        assert_eq!(empty_state(&state), None);
    }

    // The screen draws without panicking for each state.

    #[test]
    fn draws_the_loading_state_by_default() {
        let mut state = AppState::default();
        draw(&mut state, None);
    }

    #[test]
    fn draws_while_disconnected() {
        let mut state = AppState::default();
        state.connection = ConnectionState::Down {
            reason: crate::backend::Reason::NotRunning,
            retry_in: std::time::Duration::from_secs(4),
        };
        draw(&mut state, None);

        state.tasks = varied_tasks();
        state.tasks_loaded = true;
        draw(&mut state, None);
    }

    #[test]
    fn draws_an_empty_list() {
        let mut state = connected(Vec::new());
        draw(&mut state, None);
    }

    #[test]
    fn draws_varied_tasks() {
        let mut state = connected(varied_tasks());
        draw(&mut state, None);
    }

    #[test]
    fn draws_a_search_without_results() {
        let mut state = connected(varied_tasks());
        state.search = "inexistente".into();
        draw(&mut state, None);
    }

    #[test]
    fn draws_the_delete_dialog() {
        let mut state = connected(varied_tasks());
        let id = state.tasks[0].task.id.clone();
        // Nothing is answered by drawing the dialog, so the delete is still waiting afterwards.
        assert_eq!(draw(&mut state, Some(id.clone())), Some(id));
    }

    #[test]
    fn draws_the_delete_dialog_while_disconnected() {
        let mut state = connected(varied_tasks());
        let id = state.tasks[0].task.id.clone();
        state.connection = ConnectionState::Connecting;
        assert_eq!(draw(&mut state, Some(id.clone())), Some(id));
    }

    #[test]
    fn a_delete_for_a_task_that_is_gone_is_dropped() {
        let mut state = connected(varied_tasks());
        let gone = Task::new("fantasma", "x.ps1").id;
        assert_eq!(draw(&mut state, Some(gone)), None);
    }
}
