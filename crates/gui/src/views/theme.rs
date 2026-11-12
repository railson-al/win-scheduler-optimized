//! Colours shared by the screens, so a status looks the same everywhere. The pure part (which status gets
//! which [`Tone`]) is in `fmt.rs`; this maps a tone to a colour that reads well on light and dark themes.

use eframe::egui::{Color32, Context, CornerRadius, FontId, Margin, RichText, Stroke, TextStyle, Ui, vec2};
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

/// The fill of the button for the action a screen expects the user to take (Salvar, Nova tarefa).
pub fn accent_fill(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(33, 110, 210)
    } else {
        Color32::from_rgb(9, 105, 218)
    }
}

/// Gives every screen the same sizes, spacing and corners. The shapes are fixed here; the colours come from
/// each theme's own visuals, so light and dark get the same look. Call it once, when the context exists.
pub fn apply_style(ctx: &Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = vec2(8.0, 10.0);
        style.spacing.button_padding = vec2(12.0, 6.0);
        style.spacing.interact_size.y = 28.0;
        style.spacing.window_margin = Margin::same(16);

        style.text_styles.insert(TextStyle::Heading, FontId::proportional(20.0));
        style.text_styles.insert(TextStyle::Body, FontId::proportional(14.0));
        style.text_styles.insert(TextStyle::Button, FontId::proportional(14.0));
        style.text_styles.insert(TextStyle::Small, FontId::proportional(12.0));
        style.text_styles.insert(TextStyle::Monospace, FontId::monospace(13.0));

        let visuals = &mut style.visuals;
        // The borders are a step away from the background: visible, but not competing with the text.
        let quiet = visuals.panel_fill.lerp_to_gamma(visuals.text_color(), 0.15);
        visuals.window_stroke = Stroke::new(1.0, quiet);
        visuals.window_corner_radius = CornerRadius::same(8);
        visuals.menu_corner_radius = CornerRadius::same(6);
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, quiet);
        for widget in [
            &mut visuals.widgets.noninteractive,
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
            &mut visuals.widgets.open,
        ] {
            widget.corner_radius = CornerRadius::same(6);
        }
    });
}
