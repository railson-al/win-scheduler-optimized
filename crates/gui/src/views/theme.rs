//! Colours shared by the screens, so a status looks the same everywhere. The pure part (which status gets
//! which [`Tone`]) is in `fmt.rs`; this maps a tone to a colour that reads well on light and dark themes.

use eframe::egui::{Color32, RichText, Ui};
use scriptrunner_core::ExecutionStatus;

use crate::fmt::{self, Tone};

pub fn tone_color(tone: Tone, dark: bool) -> Color32 {
    match (tone, dark) {
        (Tone::Neutral, true) => Color32::from_rgb(160, 160, 160),
        (Tone::Neutral, false) => Color32::from_rgb(100, 100, 100),
        (Tone::Active, true) => Color32::from_rgb(88, 166, 255),
        (Tone::Active, false) => Color32::from_rgb(9, 105, 218),
        (Tone::Good, true) => Color32::from_rgb(63, 185, 80),
        (Tone::Good, false) => Color32::from_rgb(26, 127, 55),
        (Tone::Bad, true) => Color32::from_rgb(248, 81, 73),
        (Tone::Bad, false) => Color32::from_rgb(207, 34, 46),
        (Tone::Warn, true) => Color32::from_rgb(210, 153, 34),
        (Tone::Warn, false) => Color32::from_rgb(154, 103, 0),
    }
}

/// The status name, coloured by its tone. Use it wherever a status is shown.
pub fn status_text(ui: &Ui, status: ExecutionStatus) -> RichText {
    let color = tone_color(fmt::status_tone(status), ui.visuals().dark_mode);
    RichText::new(fmt::status_label(status)).color(color)
}
