//! The screens. A screen only draws: it reads [`AppState`](crate::state::AppState), shows it, and reports
//! what the user did as an [`Outcome`]. It never talks to the backend itself and never waits (D16, D19).
//!
//! Every screen has the same shape:
//!
//! ```ignore
//! pub fn show(&mut self, ui: &mut egui::Ui, state: &mut AppState) -> Outcome
//! ```
//!
//! * Things the user asked for go into the outcome: `outcome.send(Command::...)` for the backend,
//!   `outcome.goto(Screen::...)` to change screen. The shell sends and navigates.
//! * State rules live in `state.rs` (for example `state.history.set_filter(..)` returns the command to send).
//!   A screen calls them; it does not re-implement them.
//! * What only the screen cares about (a confirmation dialog, a half-typed field) is a field of its own
//!   view struct.
//! * Texts come from the screen's own file under `i18n/`, or from the common table in `i18n/mod.rs`.

use crate::backend::Command;
use crate::state::Screen;

pub mod dashboard;
pub mod editor;
pub mod history;
pub mod log_view;
pub mod settings;
pub mod status_bar;
pub mod theme;

#[cfg(test)]
pub mod testing;

/// What a screen asks the shell to do after a frame.
#[derive(Debug, Default)]
pub struct Outcome {
    pub commands: Vec<Command>,
    pub navigate: Option<Screen>,
}

impl Outcome {
    pub fn send(&mut self, command: Command) {
        self.commands.push(command);
    }

    pub fn goto(&mut self, screen: Screen) {
        self.navigate = Some(screen);
    }
}
