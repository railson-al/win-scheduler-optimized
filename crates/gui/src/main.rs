//! ScriptRunner's window (skeleton, task 3.2).
//!
//! It already wires the pieces together: a [`Backend`] thread talks to the daemon, and the window
//! shows the connection in a status bar and the tasks as a plain list. The real screens (3.5 to 3.9)
//! replace the list; the wiring stays.
//!
//! On Windows this program asks for elevation (see `build.rs`), because the daemon's pipe only admits
//! SYSTEM and Administrators.

use std::time::Instant;

use eframe::egui;
use scriptrunner_core::paths;
use scriptrunner_gui::backend::{Backend, BackendConfig, ConnectionState};
use scriptrunner_gui::fmt;
use scriptrunner_gui::i18n::{Text, t};
use scriptrunner_gui::state::AppState;
use scriptrunner_ipc::transport::Endpoint;

struct App {
    backend: Backend,
    state: AppState,
}

impl App {
    /// Moves everything the backend has said into the state, and sends what the state asks for in answer.
    fn pump(&mut self) {
        let now = Instant::now();
        while let Some(update) = self.backend.try_recv() {
            for command in self.state.apply(update, now) {
                self.backend.send(command);
            }
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(fmt::connection_summary(&self.state.connection));
            if let ConnectionState::Connected { version, .. } = &self.state.connection {
                ui.separator();
                ui.label(format!("{} {version}", t(Text::DaemonVersion)));
            }
            if let Some(status) = &self.state.status {
                ui.separator();
                let uptime = status.uptime_secs(Instant::now());
                ui.label(format!("{} {}", t(Text::Uptime), fmt::seconds(uptime)));
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump();
        // The uptime ticks without any update arriving.
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));

        egui::Panel::bottom("status_bar").show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading(t(Text::AppTitle));
            for row in self.state.visible_tasks() {
                ui.horizontal(|ui| {
                    ui.label(&row.task.name);
                    if row.running {
                        ui.label(t(Text::StatusRunning));
                    } else if let Some(last) = &row.last_execution {
                        ui.label(fmt::status_label(last.status));
                    }
                    if let Some(next) = row.next_run {
                        ui.label(fmt::date_time_local(next));
                    }
                });
            }
        });
    }
}

fn main() -> eframe::Result {
    let endpoint = Endpoint::for_data_dir(&paths::data_dir(None));
    let options = eframe::NativeOptions {
        // wgpu is the provisional choice (D14), pending the spike on a Windows machine without a GPU.
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title(t(Text::AppTitle))
            .with_inner_size([960.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        t(Text::AppTitle),
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let backend = Backend::spawn(endpoint, BackendConfig::default(), move || ctx.request_repaint())?;
            Ok(Box::new(App {
                backend,
                state: AppState::default(),
            }))
        }),
    )
}
