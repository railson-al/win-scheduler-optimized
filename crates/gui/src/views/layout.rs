//! Every screen, drawn for many frames in windows of several sizes, with typical data and with data built
//! to stress the layout (very long names, paths and messages). A screen that keeps growing frame after
//! frame fails here instead of on someone's desktop. (Seen once on Windows: the editor's script box kept
//! widening until it could not be used.)
//!
//! Two different things are checked, and they must not be confused:
//!
//! * **It settles** (every screen, any data): the width stops changing. A layout that feeds on itself
//!   never does.
//! * **It fits** (typical data, wide windows): the screen stays inside the window. Not asked of a screen
//!   that scrolls sideways on purpose, or of a narrow window the content legitimately exceeds.

use std::time::Instant;

use chrono::Utc;
use eframe::egui;
use scriptrunner_core::logs::{LogChunk, LogEntry, LogKind};
use scriptrunner_core::{Execution, ExecutionId, ExecutionStatus, Schedule, Settings, Task, TaskOverview, Trigger};

use super::dashboard::DashboardView;
use super::history::HistoryView;
use super::log_view::LogScreen;
use super::settings::SettingsView;
use super::testing::{assert_fits, assert_settles, widths_over_time};
use crate::backend::{Command, ConnectionState, LogUpdate, Update};
use crate::state::{AppState, Screen};

/// From a narrow window to a wide one.
const WINDOWS: [f32; 3] = [480.0, 900.0, 1400.0];
/// The windows a screen with typical data is expected to fit.
const WIDE: f32 = 1400.0;
const FRAMES: usize = 15;

/// How long the names, paths and messages are.
#[derive(Clone, Copy)]
enum Data {
    /// What people really type.
    Typical,
    /// Far beyond it.
    Extreme,
}

impl Data {
    fn text(self, prefix: &str) -> String {
        let pad = match self {
            Self::Typical => 20,
            Self::Extreme => 200,
        };
        format!("{prefix}{}", "a".repeat(pad))
    }
}

fn connected(state: &mut AppState) {
    state.connection = ConnectionState::Connected {
        version: "0.1.0".into(),
        protocol: 2,
    };
}

fn run_of(task: &Task, status: ExecutionStatus, reason: Option<String>) -> Execution {
    let mut run = Execution::new(task, Trigger::Manual, None);
    run.status = status;
    run.started_at = Some(Utc::now());
    run.finished_at = Some(Utc::now());
    run.exit_code = Some(1);
    run.failure_reason = reason;
    run
}

/// Three tasks: one with a big name, path and failed last run, one paused, one manual only.
fn tasks(data: Data) -> Vec<(Task, Option<Execution>, bool)> {
    let mut big = Task::new(data.text("Tarefa "), data.text(r"C:\caminho\"));
    big.description = Some(data.text("descrição "));
    big.schedule = Some(Schedule::Cron {
        expr: "0 9 * * 1-5".into(),
    });
    let failed = run_of(&big, ExecutionStatus::Failed, Some(data.text("falhou porque ")));

    let mut paused = Task::new("Pausada", r"C:\a\b.bat");
    paused.schedule = Some(Schedule::every_minutes(15, Utc::now()));
    paused.schedule_enabled = false;

    let manual = Task::new("Manual", r"C:\a\c.py");
    vec![(big, Some(failed), true), (paused, None, false), (manual, None, false)]
}

fn crowded_state(data: Data) -> AppState {
    let mut state = AppState::default();
    connected(&mut state);
    let rows = tasks(data)
        .into_iter()
        .map(|(task, last_execution, running)| TaskOverview {
            task,
            running,
            last_execution,
            next_run: Some(Utc::now()),
        })
        .collect();
    state.apply(Update::Overview(rows), Instant::now());
    state.apply(Update::Settings(Settings::default()), Instant::now());
    state
}

/// Draws a screen in every window size, and checks it. `fits_from` is the narrowest window in which the
/// screen is also expected to stay inside the window (`None`: it scrolls sideways, so only settling counts).
fn check(what: &str, fits_from: Option<f32>, mut draw: impl FnMut(&mut egui::Ui, f32)) {
    for width in WINDOWS {
        let widths = widths_over_time(egui::vec2(width, 700.0), FRAMES, |ui| draw(ui, width));
        match fits_from {
            Some(from) if width >= from => assert_fits(&format!("{what} at {width} px"), width, &widths),
            _ => assert_settles(&format!("{what} at {width} px"), &widths),
        }
    }
}

#[test]
fn the_dashboard_settles_and_fits_a_wide_window() {
    for (label, data, fits_from) in [("typical", Data::Typical, Some(WIDE)), ("extreme", Data::Extreme, None)] {
        let mut state = crowded_state(data);
        let mut view: DashboardView = Default::default();
        check(&format!("the dashboard, {label} data"), fits_from, |ui, _| {
            view.show(ui, &mut state);
        });
    }
}

#[test]
fn the_history_settles_and_fits_a_wide_window() {
    for (label, data, fits_from) in [("typical", Data::Typical, Some(WIDE)), ("extreme", Data::Extreme, None)] {
        let mut state = crowded_state(data);
        let commands = state.go(Screen::History);
        let Some(Command::QueryHistory { tag, .. }) = commands.into_iter().next() else {
            panic!("opening the history must query it");
        };
        let rows = tasks(data)
            .into_iter()
            .map(|(task, _, _)| run_of(&task, ExecutionStatus::Failed, Some(data.text("motivo "))))
            .collect();
        state.apply(Update::History { tag, rows }, Instant::now());
        let mut view: HistoryView = Default::default();
        check(&format!("the history, {label} data"), fits_from, |ui, _| {
            view.show(ui, &mut state);
        });
    }
}

#[test]
fn the_settings_fit_every_window() {
    for data in [Data::Typical, Data::Extreme] {
        let mut state = crowded_state(data);
        state.settings = Some(Settings {
            default_scripts_dir: Some(data.text(r"C:\scripts\").into()),
            default_python: Some(data.text(r"C:\python\").into()),
            ..Settings::default()
        });
        let mut view: SettingsView = Default::default();
        // The settings have no sideways scroll: they must fit whatever the paths say.
        check("the settings", Some(0.0), |ui, _| {
            view.show(ui, &mut state);
        });
    }
}

/// The task editor has no sideways scroll, so it must fit every window. (Its height is checked in its own
/// tests: it has to be measured inside its own scroll area, which only it can see.)
#[test]
fn the_editor_fits_every_window_and_stops_growing() {
    for data in [Data::Typical, Data::Extreme] {
        let mut state = crowded_state(data);
        state.screen = Screen::TaskEditor { task: None };
        let mut view: super::editor::TaskEditorView = Default::default();
        check("the editor", Some(0.0), |ui, _| {
            view.show(ui, &mut state);
        });
    }
}

/// The log scrolls sideways on purpose (a line can be longer than the window), so only settling counts.
#[test]
fn the_log_settles_whatever_its_lines_say() {
    for data in [Data::Typical, Data::Extreme] {
        let mut state = crowded_state(data);
        let task = Task::new("t", "a.sh");
        let id = ExecutionId::new();
        state.go(Screen::Execution(id.clone()));
        state.apply(
            Update::ExecutionLoaded {
                id: id.clone(),
                execution: Some(run_of(&task, ExecutionStatus::Failed, Some(data.text("falhou ")))),
            },
            Instant::now(),
        );
        let entries = (0..200)
            .map(|n| LogEntry {
                at: Utc::now(),
                kind: if n % 3 == 0 { LogKind::Stderr } else { LogKind::Stdout },
                text: data.text(&format!("linha {n} ")),
            })
            .collect();
        state.apply(
            Update::Log(LogUpdate::Chunk {
                id,
                chunk: LogChunk {
                    entries,
                    next_offset: 10_000,
                    eof: true,
                },
            }),
            Instant::now(),
        );
        let mut view: LogScreen = Default::default();
        check("the log screen", None, |ui, _| {
            view.show(ui, &mut state);
        });
    }
}
