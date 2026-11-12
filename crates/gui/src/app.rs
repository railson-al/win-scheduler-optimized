//! The window's shell: it owns the backend and the state, moves updates from one to the other, draws the
//! navigation and the status bar, and shows the screen the state says is current. Screens return what
//! the user asked for; the shell is the only place that sends commands or changes screen.

use std::time::{Duration, Instant};

use eframe::egui;

use crate::backend::Backend;
use crate::i18n::{Text, t};
use crate::state::{AppState, Screen};
use crate::views::dashboard::DashboardView;
use crate::views::editor::TaskEditorView;
use crate::views::history::HistoryView;
use crate::views::log_view::LogScreen;
use crate::views::settings::SettingsView;
use crate::views::{Outcome, status_bar};

pub struct App {
    backend: Backend,
    pub state: AppState,
    dashboard: DashboardView,
    editor: TaskEditorView,
    history: HistoryView,
    log: LogScreen,
    settings: SettingsView,
}

impl App {
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            state: AppState::default(),
            // `Default::default()` and not `Type::default()`: a screen that is still a unit struct would make
            // clippy object to the latter, and the screens gain fields as they need them.
            dashboard: Default::default(),
            editor: Default::default(),
            history: Default::default(),
            log: Default::default(),
            settings: Default::default(),
        }
    }

    /// Moves everything the backend has said into the state, and sends what the state asks for in answer.
    pub fn pump(&mut self) {
        let now = Instant::now();
        while let Some(update) = self.backend.try_recv() {
            for command in self.state.apply(update, now) {
                self.backend.send(command);
            }
        }
    }

    /// Sends what a screen asked for, then changes screen if it asked to.
    fn dispatch(&mut self, outcome: Outcome) {
        for command in outcome.commands {
            self.backend.send(command);
        }
        if let Some(screen) = outcome.navigate {
            self.go(screen);
        }
    }

    fn go(&mut self, screen: Screen) {
        for command in self.state.go(screen) {
            self.backend.send(command);
        }
    }

    fn navigation(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let on = &self.state.screen;
            let tabs = [
                (
                    Text::NavDashboard,
                    Screen::Dashboard,
                    matches!(on, Screen::Dashboard | Screen::TaskEditor { .. }),
                ),
                (
                    Text::NavHistory,
                    Screen::History,
                    matches!(on, Screen::History | Screen::Execution(_)),
                ),
                (Text::NavSettings, Screen::Settings, matches!(on, Screen::Settings)),
            ];
            let mut target = None;
            for (label, screen, selected) in tabs {
                if ui.selectable_label(selected, t(label)).clicked() && !selected {
                    target = Some(screen);
                }
            }
            if let Some(screen) = target {
                self.go(screen);
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump();
        // The uptime ticks without any update arriving.
        ui.ctx().request_repaint_after(Duration::from_secs(1));

        egui::Panel::top("navigation").show(ui, |ui| self.navigation(ui));
        egui::Panel::bottom("status_bar").show(ui, |ui| status_bar::show(ui, &self.state, Instant::now()));
        let outcome = egui::CentralPanel::default()
            .show(ui, |ui| match self.state.screen.clone() {
                Screen::Dashboard => self.dashboard.show(ui, &mut self.state),
                Screen::TaskEditor { .. } => self.editor.show(ui, &mut self.state),
                Screen::History => self.history.show(ui, &mut self.state),
                Screen::Execution(_) => self.log.show(ui, &mut self.state),
                Screen::Settings => self.settings.show(ui, &mut self.state),
            })
            .inner;
        self.dispatch(outcome);
    }
}
