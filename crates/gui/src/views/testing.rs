//! Drawing a screen without a window, to check that it does not panic for a given state.
//!
//! This renders to egui's in-memory output: no GPU, no display. It does not click or type; what the
//! user's actions turn into is covered by the tests of `state.rs` and, later, by interface tests.

use eframe::egui;

use super::theme;

/// Draws `draw` inside a central panel (as the shell does) for `frames` frames in a window of a fixed size,
/// in one context, and returns the width used after each frame. A screen that keeps growing (a field that
/// asks for infinite width inside a grid in a scroll area) shows up as widths past the window, or still
/// changing at the end.
pub fn widths_over_time(window: egui::Vec2, frames: usize, mut draw: impl FnMut(&mut egui::Ui)) -> Vec<f32> {
    let ctx = egui::Context::default();
    theme::apply_style(&ctx);
    let mut widths = Vec::new();
    for _ in 0..frames {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, window)),
            ..egui::RawInput::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| draw(ui));
        });
        output.textures_delta.clear();
        widths.push(ctx.globally_used_rect().width());
    }
    widths
}

/// Panics unless the width had stopped changing by the last frames. A layout that feeds on itself keeps
/// growing, so this is what catches it, however wide the content legitimately is.
pub fn assert_settles(what: &str, widths: &[f32]) {
    let n = widths.len();
    assert!(
        n < 3 || ((widths[n - 3] - widths[n - 1]).abs() < 0.5 && (widths[n - 2] - widths[n - 1]).abs() < 0.5),
        "{what} was still changing width at the last frames: {widths:?}"
    );
}

/// [`assert_settles`], and the screen must also have stayed inside a window `window_width` wide. Only for
/// screens that do not scroll sideways, or content that is expected to fit.
pub fn assert_fits(what: &str, window_width: f32, widths: &[f32]) {
    assert!(
        widths.iter().all(|width| *width <= window_width + 0.5),
        "{what} outgrew a {window_width} px window: {widths:?}"
    );
    assert_settles(what, widths);
}

/// Runs `draw` for two frames (the first lays widgets out, the second draws them with their sizes).
pub fn frame(mut draw: impl FnMut(&mut egui::Ui)) {
    let ctx = egui::Context::default();
    theme::apply_style(&ctx);
    for _ in 0..2 {
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| draw(ui));
        // Nothing paints these textures, and egui asserts that none are dropped unhandled.
        output.textures_delta.clear();
    }
}
