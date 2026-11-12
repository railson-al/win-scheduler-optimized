//! The bar at the bottom: connection, the daemon's version and its uptime.

use std::time::Instant;

use eframe::egui::{self, Margin, RichText};

use crate::backend::ConnectionState;
use crate::fmt;
use crate::i18n::{Text, t};
use crate::state::AppState;

pub fn show(ui: &mut egui::Ui, state: &AppState, now: Instant) {
    egui::Frame::new()
        .inner_margin(Margin::symmetric(16, 6))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(muted(fmt::connection_summary(&state.connection)));
                if let ConnectionState::Connected { version, .. } = &state.connection {
                    ui.separator();
                    ui.label(muted(format!("{} {version}", t(Text::DaemonVersion))));
                }
                if let Some(status) = &state.status {
                    ui.separator();
                    ui.label(muted(format!(
                        "{} {}",
                        t(Text::Uptime),
                        fmt::seconds(status.uptime_secs(now))
                    )));
                }
            });
        });
}

/// The bar's text: small and muted, so it informs without competing with the screen.
fn muted(text: String) -> RichText {
    RichText::new(text).small().weak()
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
