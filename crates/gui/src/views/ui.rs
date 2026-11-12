//! Layout helpers shared by the screens: a centred page with responsive side padding, the cards that group a
//! form's sections, a label-and-field row, a page header and a row of dialog buttons.
//!
//! They only place widgets. What a screen shows, and what the user can do, stays in the screen.

use eframe::egui::{
    self, Align, Color32, CornerRadius, Layout, Margin, Rect, RichText, Stroke, Ui, UiBuilder, pos2, vec2,
};

use super::theme;

/// The widest a form reads well.
pub const FORM_WIDTH: f32 = 800.0;
/// The widest a table grows before it is centred with margins.
pub const TABLE_WIDTH: f32 = 1200.0;
/// No cap: the page takes the window's width less its side padding. For screens that fill the window.
pub const FULL_WIDTH: f32 = f32::INFINITY;
/// The width of a confirmation dialog.
pub const DIALOG_WIDTH: f32 = 380.0;
/// Below this window width a field's label goes above its field instead of beside it.
const STACK_BELOW: f32 = 560.0;
/// Below this width the actions of a header go under its title instead of beside it.
const HEADER_STACKS_BELOW: f32 = 600.0;
/// The label column of a field row, so that the labels of one form line up.
const LABEL_WIDTH: f32 = 176.0;
/// The gap above a page's contents.
const PAGE_TOP: f32 = 16.0;
/// The gap between two sections.
pub const SECTION_GAP: f32 = 16.0;

/// The side padding of a window this wide: 16 px on a narrow window, growing to 48 px on a wide one.
pub fn side_padding(width: f32) -> f32 {
    if width < 720.0 {
        16.0
    } else if width < 1100.0 {
        32.0
    } else {
        48.0
    }
}

/// Whether a field's label goes above its field, for a window this wide.
pub fn stacks(width: f32) -> bool {
    width < STACK_BELOW
}

/// Where a page goes inside `available`: centred, at most `max_width` wide, and never closer to the edges
/// than the side padding of the window.
pub fn content_rect(available: Rect, max_width: f32) -> Rect {
    let padding = side_padding(available.width());
    let width = (available.width() - 2.0 * padding).clamp(0.0, max_width);
    let left = available.left() + (available.width() - width) / 2.0;
    Rect::from_min_size(pos2(left, available.top()), vec2(width, available.height()))
}

/// Draws `add_contents` in the space left, centred and at most `max_width` wide, with the side padding.
/// It does not scroll: a screen that needs to wraps the page in a `ScrollArea`.
pub fn page<R>(ui: &mut Ui, max_width: f32, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    let rect = content_rect(ui.available_rect_before_wrap(), max_width);
    ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
        ui.add_space(PAGE_TOP);
        add_contents(ui)
    })
    .inner
}

/// A card that groups one part of a form: a title, an optional small caption, then the contents.
pub fn section<R>(ui: &mut Ui, title: &str, caption: Option<&str>, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    let (fill, stroke) = card_look(ui.visuals());
    let inner = egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new(title).strong().size(16.0));
            if let Some(caption) = caption {
                ui.label(RichText::new(caption).small().weak());
            }
            ui.add_space(8.0);
            add_contents(ui)
        });
    ui.add_space(SECTION_GAP);
    inner.inner
}

/// The fill and border of a card. Both come from the theme's own colours, so a card suits either theme.
fn card_look(visuals: &egui::Visuals) -> (Color32, Stroke) {
    let fill = visuals.panel_fill.lerp_to_gamma(visuals.extreme_bg_color, 0.5);
    (fill, visuals.widgets.noninteractive.bg_stroke)
}

/// A labelled field. Beside a wide window the label sits in a column of its own, so the labels of a form
/// line up; beside a narrow one it sits above the field. `add_field` draws the control, `add_note` what goes
/// under it (a problem, a help line).
pub fn field(ui: &mut Ui, label: &str, add_field: impl FnOnce(&mut Ui), add_note: impl FnOnce(&mut Ui)) {
    if stacks(ui.available_width()) {
        ui.label(label);
        add_field(ui);
        add_note(ui);
    } else {
        ui.horizontal(|ui| {
            let height = ui.spacing().interact_size.y;
            ui.allocate_ui_with_layout(vec2(LABEL_WIDTH, height), Layout::left_to_right(Align::Center), |ui| {
                ui.label(label);
            });
            ui.vertical(|ui| {
                add_field(ui);
                add_note(ui);
            });
        });
    }
}

/// The top of a screen: the title with an optional caption on the left, the actions on the right. On a
/// narrow window the actions go under the title, on the right. The first action `add_actions` adds is the
/// rightmost one, so the primary action goes first.
pub fn header(ui: &mut Ui, title: &str, caption: Option<&str>, add_actions: impl FnOnce(&mut Ui)) {
    let title_block = |ui: &mut Ui| {
        ui.vertical(|ui| {
            ui.heading(title);
            if let Some(caption) = caption {
                ui.label(RichText::new(caption).weak());
            }
        });
    };
    if ui.available_width() < HEADER_STACKS_BELOW {
        title_block(ui);
        buttons_right(ui, add_actions);
    } else {
        egui::containers::Sides::new().show(ui, title_block, add_actions);
    }
    ui.add_space(12.0);
}

/// A row of buttons pushed to the right edge. `add_buttons` draws right to left: the first button it adds is
/// the rightmost, so the primary one goes first.
pub fn buttons_right<R>(ui: &mut Ui, add_buttons: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| ui.with_layout(Layout::right_to_left(Align::Center), add_buttons).inner)
        .inner
}

/// The button of the action a screen expects the user to take.
pub fn primary_button(ui: &Ui, text: &str) -> egui::Button<'static> {
    let fill = theme::accent_fill(ui.visuals().dark_mode);
    egui::Button::new(RichText::new(text).strong().color(Color32::WHITE)).fill(fill)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::testing::frame;

    #[test]
    fn the_padding_grows_with_the_window_and_stays_inside_it() {
        assert_eq!(side_padding(480.0), 16.0);
        assert_eq!(side_padding(900.0), 32.0);
        assert_eq!(side_padding(1400.0), 48.0);
    }

    #[test]
    fn a_page_is_centred_and_capped_at_its_width() {
        let window = Rect::from_min_size(pos2(0.0, 0.0), vec2(1400.0, 700.0));
        let rect = content_rect(window, FORM_WIDTH);
        assert_eq!(rect.width(), FORM_WIDTH);
        assert!((rect.left() - (1400.0 - FORM_WIDTH) / 2.0).abs() < 0.5);
    }

    #[test]
    fn a_narrow_window_keeps_the_side_padding_and_stacks_labels() {
        let window = Rect::from_min_size(pos2(0.0, 0.0), vec2(480.0, 700.0));
        let rect = content_rect(window, FORM_WIDTH);
        assert_eq!(rect.left(), 16.0);
        assert_eq!(rect.width(), 448.0);
        assert!(stacks(480.0));
        assert!(!stacks(900.0));
    }

    #[test]
    fn the_helpers_draw_without_panicking() {
        frame(|ui| {
            page(ui, FORM_WIDTH, |ui| {
                header(ui, "Título", Some("legenda"), |ui| {
                    drop(ui.button("Um"));
                    drop(ui.button("Dois"));
                });
                section(ui, "Seção", None, |ui| {
                    field(ui, "Campo", |ui| drop(ui.button("x")), |ui| drop(ui.weak("ajuda")));
                });
                buttons_right(ui, |ui| ui.button("Ok").clicked());
            });
        });
    }
}
