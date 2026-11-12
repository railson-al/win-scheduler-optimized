//! Drawing a screen without a window, to check that it does not panic for a given state.
//!
//! This renders to egui's in-memory output: no GPU, no display. It does not click or type; what the
//! user's actions turn into is covered by the tests of `state.rs` and, later, by interface tests.

use eframe::egui;

/// Runs `draw` for two frames (the first lays widgets out, the second draws them with their sizes).
pub fn frame(mut draw: impl FnMut(&mut egui::Ui)) {
    let ctx = egui::Context::default();
    for _ in 0..2 {
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| draw(ui));
        // Nothing paints these textures, and egui asserts that none are dropped unhandled.
        output.textures_delta.clear();
    }
}
