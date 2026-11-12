//! The bar at the bottom: connection, the daemon's version and its uptime.

use std::time::Instant;

use eframe::egui;

use crate::backend::ConnectionState;
use crate::fmt;
use crate::i18n::{Text, t};
use crate::state::AppState;

pub fn show(ui: &mut egui::Ui, state: &AppState, now: Instant) {
    ui.horizontal_wrapped(|ui| {
        ui.label(fmt::connection_summary(&state.connection));
        if let ConnectionState::Connected { version, .. } = &state.connection {
            ui.separator();
            ui.label(format!("{} {version}", t(Text::DaemonVersion)));
        }
        if let Some(status) = &state.status {
            ui.separator();
            ui.label(format!("{} {}", t(Text::Uptime), fmt::seconds(status.uptime_secs(now))));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::testing::frame;

    #[test]
    fn it_draws_while_connecting() {
        let state = AppState::default();
        frame(|ui| show(ui, &state, Instant::now()));
    }
}
