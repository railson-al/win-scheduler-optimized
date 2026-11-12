//! The settings screen (task 3.9). It edits a draft of the daemon's [`Settings`] and sends
//! [`Command::UpdateSettings`] when the user saves. The rules (units, validation, what counts as a change,
//! how the daemon's new values reach the draft) are in plain code below, so they are tested without a window.
//!
//! The draft lives in egui's temporary memory rather than in a field of [`SettingsView`], so the view stays
//! a unit struct that the shell builds with a literal.

use std::path::{Path, PathBuf};

use eframe::egui::{self, RichText};
use scriptrunner_core::Settings;

use super::Outcome;
use super::theme;
use crate::backend::Command;
use crate::fmt::Tone;
use crate::i18n::{
    self,
    settings::{Text, t},
};
use crate::state::AppState;

const BYTES_PER_MIB: f64 = 1024.0 * 1024.0;
/// What the timeout box offers the first time it is ticked on a daemon with no timeout set: 30 minutes.
const DEFAULT_TIMEOUT_SECS: u64 = 30 * 60;
/// Where the draft is kept between frames.
const FORM_ID: &str = "settings_form";

#[derive(Default)]
pub struct SettingsView;

impl SettingsView {
    pub fn show(&mut self, ui: &mut egui::Ui, state: &mut AppState) -> Outcome {
        let id = egui::Id::unique(FORM_ID);
        let mut form = ui
            .ctx()
            .data(|data| data.get_temp::<SettingsForm>(id))
            .unwrap_or_default();
        let mut outcome = Outcome::default();

        form.sync(state.settings.as_ref());
        let connected = state.is_connected();
        let service_changed = form.service_changed;

        ui.heading(t(Text::Title));
        let discard = match (state.settings.as_ref(), form.draft.as_mut()) {
            (Some(base), Some(draft)) => draw_form(ui, connected, draft, base, service_changed, &mut outcome),
            _ if connected => {
                ui.label(t(Text::Loading));
                false
            }
            _ => {
                ui.label(i18n::t(i18n::Text::NotConnected));
                false
            }
        };
        if discard {
            form.discard();
        }

        ui.ctx().data_mut(|data| data.insert_temp(id, form));
        outcome
    }
}

/// Units, validation and change tracking for the settings form. Sizes are shown in MiB, the timeout in
/// seconds; `None` for a path is an empty box.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsDraft {
    pub history_retention_days: u32,
    pub log_per_execution_mib: f64,
    pub log_total_mib: f64,
    pub timeout_enabled: bool,
    /// Kept while the box is off, so ticking it again brings the last value back.
    pub timeout_secs: u64,
    pub scripts_dir: String,
    pub python: String,
    pub max_concurrent: u32,
}

/// A field of the form, for placing its error next to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    MaxConcurrent,
    LogPerExecution,
    LogTotal,
    Timeout,
}

/// One rule the draft breaks. `message` is a pt-BR text, never the core's English one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldError {
    pub field: Field,
    pub message: Text,
}

impl SettingsDraft {
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            history_retention_days: settings.history_retention_days,
            log_per_execution_mib: bytes_to_mib(settings.max_log_bytes_per_execution),
            log_total_mib: bytes_to_mib(settings.max_total_log_bytes),
            timeout_enabled: settings.default_timeout_secs.is_some(),
            timeout_secs: settings.default_timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
            scripts_dir: path_text(settings.default_scripts_dir.as_deref()),
            python: path_text(settings.default_python.as_deref()),
            max_concurrent: settings.max_concurrent_executions,
        }
    }

    /// The settings to save, or `None` while the draft breaks a rule.
    pub fn to_settings(&self) -> Option<Settings> {
        if !self.errors().is_empty() {
            return None;
        }
        Some(self.build())
    }

    /// Every rule the draft breaks, in form order. Empty when the draft can be saved.
    pub fn errors(&self) -> Vec<FieldError> {
        let mut errors = Vec::new();
        if self.max_concurrent == 0 {
            errors.push(FieldError {
                field: Field::MaxConcurrent,
                message: Text::ErrConcurrency,
            });
        }
        let per_execution = limit_error(self.log_per_execution_mib);
        if let Some(message) = per_execution {
            errors.push(FieldError {
                field: Field::LogPerExecution,
                message,
            });
        }
        let total = limit_error(self.log_total_mib);
        if let Some(message) = total {
            errors.push(FieldError {
                field: Field::LogTotal,
                message,
            });
        }
        // Only comparable once both are numbers; otherwise the field's own error is what the user needs.
        if per_execution.is_none() && total.is_none() && self.log_per_execution_mib > self.log_total_mib {
            errors.push(FieldError {
                field: Field::LogPerExecution,
                message: Text::ErrLogPerAboveTotal,
            });
        }
        if self.timeout_enabled && self.timeout_secs == 0 {
            errors.push(FieldError {
                field: Field::Timeout,
                message: Text::ErrTimeout,
            });
        }
        errors
    }

    /// `true` when saving would change `base`. Compared field by field in draft units, so a value the user
    /// never touched is not a change, whatever its unit rounding; a timeout that is off is compared as off.
    pub fn is_dirty(&self, base: &Settings) -> bool {
        !self.same_edits(&Self::from_settings(base))
    }

    /// The daemon sent `new` while the draft was based on `old`. Fields the user has not edited follow the
    /// daemon; fields the user edited keep their value.
    pub fn rebase(&mut self, old: &Settings, new: &Settings) {
        let before = Self::from_settings(old);
        let after = Self::from_settings(new);
        if self.history_retention_days == before.history_retention_days {
            self.history_retention_days = after.history_retention_days;
        }
        if self.log_per_execution_mib == before.log_per_execution_mib {
            self.log_per_execution_mib = after.log_per_execution_mib;
        }
        if self.log_total_mib == before.log_total_mib {
            self.log_total_mib = after.log_total_mib;
        }
        if self.timeout() == before.timeout() {
            self.timeout_enabled = after.timeout_enabled;
            self.timeout_secs = after.timeout_secs;
        }
        if self.scripts_dir == before.scripts_dir {
            self.scripts_dir = after.scripts_dir;
        }
        if self.python == before.python {
            self.python = after.python;
        }
        if self.max_concurrent == before.max_concurrent {
            self.max_concurrent = after.max_concurrent;
        }
    }

    fn timeout(&self) -> Option<u64> {
        self.timeout_enabled.then_some(self.timeout_secs)
    }

    fn same_edits(&self, other: &Self) -> bool {
        self.history_retention_days == other.history_retention_days
            && self.log_per_execution_mib == other.log_per_execution_mib
            && self.log_total_mib == other.log_total_mib
            && self.timeout() == other.timeout()
            && path_value(&self.scripts_dir) == path_value(&other.scripts_dir)
            && path_value(&self.python) == path_value(&other.python)
            && self.max_concurrent == other.max_concurrent
    }

    /// Whether the Save button is enabled: connected, something changed, and no rule broken.
    pub fn can_save(&self, connected: bool, base: &Settings) -> bool {
        connected && self.is_dirty(base) && self.errors().is_empty()
    }

    /// The settings the draft says, with no rule checked.
    fn build(&self) -> Settings {
        Settings {
            history_retention_days: self.history_retention_days,
            max_log_bytes_per_execution: mib_to_bytes(self.log_per_execution_mib),
            max_total_log_bytes: mib_to_bytes(self.log_total_mib),
            default_timeout_secs: self.timeout(),
            default_scripts_dir: path_value(&self.scripts_dir),
            default_python: path_value(&self.python),
            max_concurrent_executions: self.max_concurrent,
        }
    }
}

/// A size in MiB, as typed on the screen, to bytes. The float is exact for any size below 2^53 bytes, so a
/// size read from the daemon and written back comes out the same. Out of range values saturate (a float to
/// integer cast saturates in Rust): NaN and negatives become 0, infinity becomes `u64::MAX`.
pub fn mib_to_bytes(mib: f64) -> u64 {
    if mib.is_nan() || mib <= 0.0 {
        return 0;
    }
    (mib * BYTES_PER_MIB).round() as u64
}

/// A size in bytes to MiB, for the screen.
pub fn bytes_to_mib(bytes: u64) -> f64 {
    bytes as f64 / BYTES_PER_MIB
}

fn limit_error(mib: f64) -> Option<Text> {
    if !mib.is_finite() {
        Some(Text::ErrNotNumber)
    } else if mib <= 0.0 {
        Some(Text::ErrLogLimit)
    } else {
        None
    }
}

/// An empty box (or only spaces) is "no path", which the daemon stores as `None`.
fn path_value(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

fn path_text(path: Option<&Path>) -> String {
    path.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The draft and what it was based on, kept between frames.
#[derive(Debug, Clone, Default)]
struct SettingsForm {
    draft: Option<SettingsDraft>,
    /// The daemon's settings the draft was last synced with.
    base: Option<Settings>,
    /// The daemon changed its settings while the user had unsaved edits; the edits were kept.
    service_changed: bool,
}

impl SettingsForm {
    /// Follows the daemon. The first settings make the draft; later ones reach the fields the user has not
    /// edited, and the user's edits stay.
    fn sync(&mut self, current: Option<&Settings>) {
        let Some(current) = current else { return };
        if self.base.as_ref() == Some(current) {
            return;
        }
        let rebased = match (self.draft.as_mut(), self.base.as_ref()) {
            (Some(draft), Some(old)) => {
                draft.rebase(old, current);
                true
            }
            _ => false,
        };
        if !rebased {
            self.draft = Some(SettingsDraft::from_settings(current));
        }
        // After our own save the draft equals what the daemon sent back, so there is nothing to report.
        self.service_changed = self.draft.as_ref().is_some_and(|draft| draft.is_dirty(current));
        self.base = Some(current.clone());
    }

    fn discard(&mut self) {
        if let Some(base) = self.base.as_ref() {
            self.draft = Some(SettingsDraft::from_settings(base));
        }
        self.service_changed = false;
    }
}

/// Draws the form and reports what the user asked for. Returns `true` when the user discards their edits.
fn draw_form(
    ui: &mut egui::Ui,
    connected: bool,
    draft: &mut SettingsDraft,
    base: &Settings,
    service_changed: bool,
    outcome: &mut Outcome,
) -> bool {
    let errors = draft.errors();
    let dirty = draft.is_dirty(base);
    let dark = ui.visuals().dark_mode;
    let mut discard = false;

    ui.weak(t(Text::Intro));
    if service_changed {
        ui.colored_label(theme::tone_color(Tone::Warn, dark), t(Text::ServiceChanged));
    }
    ui.add_space(8.0);

    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.strong(t(Text::SectionHistory));
        field(
            ui,
            &errors,
            None,
            |ui| {
                ui.add(egui::DragValue::new(&mut draft.history_retention_days).range(0..=u32::MAX));
            },
            t(Text::RetentionLabel),
            t(Text::RetentionHelp),
        );

        ui.add_space(8.0);
        ui.strong(t(Text::SectionLogs));
        field(
            ui,
            &errors,
            Some(Field::LogPerExecution),
            |ui| {
                ui.add(
                    egui::DragValue::new(&mut draft.log_per_execution_mib)
                        .range(0.0..=f64::MAX)
                        .speed(0.5)
                        .max_decimals(2)
                        .suffix(t(Text::UnitMib)),
                );
            },
            t(Text::LogPerExecutionLabel),
            t(Text::LogPerExecutionHelp),
        );
        field(
            ui,
            &errors,
            Some(Field::LogTotal),
            |ui| {
                ui.add(
                    egui::DragValue::new(&mut draft.log_total_mib)
                        .range(0.0..=f64::MAX)
                        .speed(1.0)
                        .max_decimals(2)
                        .suffix(t(Text::UnitMib)),
                );
            },
            t(Text::LogTotalLabel),
            t(Text::LogTotalHelp),
        );

        ui.add_space(8.0);
        ui.strong(t(Text::SectionRuns));
        field(
            ui,
            &errors,
            Some(Field::MaxConcurrent),
            |ui| {
                ui.add(egui::DragValue::new(&mut draft.max_concurrent).range(1..=u32::MAX));
            },
            t(Text::ConcurrentLabel),
            t(Text::ConcurrentHelp),
        );

        field(
            ui,
            &errors,
            Some(Field::Timeout),
            |ui| {
                ui.checkbox(&mut draft.timeout_enabled, t(Text::TimeoutCheck));
                let enabled = draft.timeout_enabled;
                ui.add_enabled(
                    enabled,
                    egui::DragValue::new(&mut draft.timeout_secs)
                        .range(0..=u64::MAX)
                        .suffix(t(Text::UnitSeconds)),
                );
            },
            t(Text::TimeoutLabel),
            t(Text::TimeoutHelp),
        );

        field(
            ui,
            &errors,
            None,
            |ui| {
                ui.add(egui::TextEdit::singleline(&mut draft.scripts_dir).desired_width(360.0));
            },
            t(Text::ScriptsDirLabel),
            t(Text::ScriptsDirHelp),
        );
        field(
            ui,
            &errors,
            None,
            |ui| {
                ui.add(egui::TextEdit::singleline(&mut draft.python).desired_width(360.0));
            },
            t(Text::PythonLabel),
            t(Text::PythonHelp),
        );
    });

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                draft.can_save(connected, base),
                egui::Button::new(i18n::t(i18n::Text::Save)),
            )
            .clicked()
            && let Some(settings) = draft.to_settings()
        {
            outcome.send(Command::UpdateSettings(settings));
        }
        if ui.add_enabled(dirty, egui::Button::new(t(Text::Discard))).clicked() {
            discard = true;
        }
        if !connected {
            ui.weak(t(Text::ConnectToSave));
        }
    });
    discard
}

/// One setting: its name, the control, a short help line and, when the draft breaks a rule for it, the
/// error in red. `error_for` is the field whose errors to show; `None` for fields without rules.
fn field(
    ui: &mut egui::Ui,
    errors: &[FieldError],
    error_for: Option<Field>,
    control: impl FnOnce(&mut egui::Ui),
    label: &str,
    help: &str,
) {
    ui.label(RichText::new(label));
    ui.horizontal(|ui| control(ui));
    ui.weak(help);
    if let Some(field) = error_for
        && let Some(error) = errors.iter().find(|error| error.field == field)
    {
        ui.colored_label(theme::tone_color(Tone::Bad, ui.visuals().dark_mode), t(error.message));
    }
    ui.add_space(4.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::ConnectionState;
    use crate::views::testing::frame;

    const MIB: u64 = 1024 * 1024;

    fn connected() -> ConnectionState {
        ConnectionState::Connected {
            version: "0.1.0".into(),
            protocol: 2,
        }
    }

    fn draft_errors(draft: &SettingsDraft) -> Vec<(Field, Text)> {
        draft.errors().into_iter().map(|e| (e.field, e.message)).collect()
    }

    #[test]
    fn defaults_come_back_unchanged() {
        let settings = Settings::default();
        let draft = SettingsDraft::from_settings(&settings);
        assert_eq!(draft.to_settings(), Some(settings));
        assert!(!draft.is_dirty(&Settings::default()));
    }

    #[test]
    fn large_values_and_paths_round_trip() {
        let settings = Settings {
            history_retention_days: u32::MAX,
            max_log_bytes_per_execution: 3 * 1024 * 1024 * 1024 + 7,
            max_total_log_bytes: u64::MAX,
            default_timeout_secs: Some(u64::MAX),
            default_scripts_dir: Some(PathBuf::from("C:\\Scripts")),
            default_python: Some(PathBuf::from("/usr/bin/python3")),
            max_concurrent_executions: u32::MAX,
        };
        let draft = SettingsDraft::from_settings(&settings);
        assert_eq!(draft.to_settings(), Some(settings.clone()));
        assert!(!draft.is_dirty(&settings));
    }

    #[test]
    fn sizes_that_are_not_whole_mib_come_back_exactly() {
        let settings = Settings {
            max_log_bytes_per_execution: 5_000_000,
            ..Settings::default()
        };
        let draft = SettingsDraft::from_settings(&settings);
        assert!(draft.errors().is_empty(), "a sub-MiB limit is valid");
        assert!(!draft.is_dirty(&settings), "showing it is not a change");
        assert_eq!(draft.to_settings(), Some(settings));
    }

    #[test]
    fn no_timeout_round_trips_as_none() {
        let settings = Settings {
            default_timeout_secs: None,
            ..Settings::default()
        };
        let draft = SettingsDraft::from_settings(&settings);
        assert!(!draft.timeout_enabled);
        assert_eq!(draft.to_settings().and_then(|s| s.default_timeout_secs), None);
    }

    #[test]
    fn mib_converts_both_ways_without_overflow() {
        assert_eq!(mib_to_bytes(5.0), 5 * MIB);
        assert_eq!(bytes_to_mib(512 * MIB), 512.0);
        assert_eq!(mib_to_bytes(bytes_to_mib(u64::MAX)), u64::MAX);
        assert_eq!(mib_to_bytes(f64::MAX), u64::MAX);
        assert_eq!(mib_to_bytes(f64::INFINITY), u64::MAX);
        assert_eq!(mib_to_bytes(1e300), u64::MAX);
        assert_eq!(mib_to_bytes(-3.0), 0);
        assert_eq!(mib_to_bytes(f64::NAN), 0);
    }

    #[test]
    fn a_zero_concurrency_is_refused() {
        let mut draft = SettingsDraft::from_settings(&Settings::default());
        draft.max_concurrent = 0;
        assert_eq!(draft_errors(&draft), vec![(Field::MaxConcurrent, Text::ErrConcurrency)]);
        assert_eq!(draft.to_settings(), None);
    }

    #[test]
    fn log_limits_must_be_positive_numbers() {
        let mut draft = SettingsDraft::from_settings(&Settings::default());
        draft.log_per_execution_mib = 0.0;
        assert_eq!(draft_errors(&draft), vec![(Field::LogPerExecution, Text::ErrLogLimit)]);

        draft.log_per_execution_mib = 1.0;
        draft.log_total_mib = -1.0;
        assert_eq!(draft_errors(&draft), vec![(Field::LogTotal, Text::ErrLogLimit)]);

        draft.log_total_mib = f64::NAN;
        assert_eq!(draft_errors(&draft), vec![(Field::LogTotal, Text::ErrNotNumber)]);
    }

    #[test]
    fn one_execution_cannot_log_more_than_all_of_them() {
        let mut draft = SettingsDraft::from_settings(&Settings::default());
        draft.log_per_execution_mib = 600.0;
        draft.log_total_mib = 512.0;
        assert_eq!(
            draft_errors(&draft),
            vec![(Field::LogPerExecution, Text::ErrLogPerAboveTotal)]
        );

        draft.log_per_execution_mib = 512.0;
        assert!(draft.errors().is_empty(), "equal limits are fine");

        // A broken total is reported on its own; the comparison would be meaningless.
        draft.log_total_mib = 0.0;
        assert_eq!(draft_errors(&draft), vec![(Field::LogTotal, Text::ErrLogLimit)]);
    }

    #[test]
    fn a_timeout_that_is_on_must_be_at_least_one_second() {
        let mut draft = SettingsDraft::from_settings(&Settings::default());
        draft.timeout_enabled = true;
        draft.timeout_secs = 0;
        assert_eq!(draft_errors(&draft), vec![(Field::Timeout, Text::ErrTimeout)]);

        draft.timeout_enabled = false;
        assert!(draft.errors().is_empty(), "a timeout that is off is not checked");
        assert_eq!(draft.to_settings().and_then(|s| s.default_timeout_secs), None);
    }

    #[test]
    fn empty_paths_become_none() {
        assert_eq!(path_value(""), None);
        assert_eq!(path_value("   "), None);
        assert_eq!(path_value(" /srv/x "), Some(PathBuf::from("/srv/x")));

        let mut draft = SettingsDraft::from_settings(&Settings::default());
        draft.scripts_dir = "  ".into();
        draft.python = String::new();
        let saved = draft.to_settings().expect("valid draft");
        assert_eq!(saved.default_scripts_dir, None);
        assert_eq!(saved.default_python, None);
    }

    #[test]
    fn dirty_only_when_something_changed() {
        let base = Settings::default();
        let mut draft = SettingsDraft::from_settings(&base);
        assert!(!draft.is_dirty(&base));

        draft.max_concurrent += 1;
        assert!(draft.is_dirty(&base));
        draft.max_concurrent -= 1;
        assert!(!draft.is_dirty(&base), "going back is not a change");

        draft.timeout_secs = 5;
        assert!(!draft.is_dirty(&base), "a timeout that is off is not a change");
        draft.timeout_enabled = true;
        assert!(draft.is_dirty(&base));
    }

    #[test]
    fn clearing_or_setting_a_path_is_a_change() {
        let base = Settings {
            default_python: Some(PathBuf::from("/usr/bin/python3")),
            ..Settings::default()
        };
        let mut draft = SettingsDraft::from_settings(&base);
        assert!(!draft.is_dirty(&base));
        draft.python.clear();
        assert!(draft.is_dirty(&base));
    }

    #[test]
    fn save_needs_a_connection_a_change_and_no_errors() {
        let base = Settings::default();
        let mut draft = SettingsDraft::from_settings(&base);
        draft.max_concurrent = 8;
        assert!(draft.can_save(true, &base));
        assert!(!draft.can_save(false, &base));
        draft.max_concurrent = 0;
        assert!(!draft.can_save(true, &base));
        draft.max_concurrent = 4;
        assert!(!draft.can_save(true, &base), "back to the saved value: nothing to save");
    }

    #[test]
    fn rebase_follows_the_daemon_except_where_the_user_edited() {
        let old = Settings::default();
        let new = Settings {
            history_retention_days: 30,
            max_concurrent_executions: 2,
            ..Settings::default()
        };
        let mut draft = SettingsDraft::from_settings(&old);
        draft.max_concurrent = 8;

        draft.rebase(&old, &new);
        assert_eq!(draft.max_concurrent, 8, "the user's edit stays");
        assert_eq!(draft.history_retention_days, 30, "an untouched field follows");
        assert!(draft.is_dirty(&new));
    }

    #[test]
    fn rebase_of_an_untouched_draft_is_the_new_settings() {
        let old = Settings::default();
        let new = Settings {
            default_timeout_secs: Some(120),
            default_python: Some(PathBuf::from("/opt/py")),
            ..Settings::default()
        };
        let mut draft = SettingsDraft::from_settings(&old);
        draft.rebase(&old, &new);
        assert_eq!(draft, SettingsDraft::from_settings(&new));
        assert!(!draft.is_dirty(&new));
    }

    #[test]
    fn form_takes_the_first_settings_and_flags_nothing() {
        let mut form = SettingsForm::default();
        form.sync(None);
        assert!(form.draft.is_none());

        form.sync(Some(&Settings::default()));
        assert_eq!(form.draft, Some(SettingsDraft::from_settings(&Settings::default())));
        assert!(!form.service_changed);
    }

    #[test]
    fn form_keeps_pending_edits_and_says_the_service_changed() {
        let old = Settings::default();
        let mut form = SettingsForm::default();
        form.sync(Some(&old));
        form.draft.as_mut().expect("draft").max_concurrent = 9;

        let changed = Settings {
            history_retention_days: 7,
            ..Settings::default()
        };
        form.sync(Some(&changed));
        let draft = form.draft.as_ref().expect("draft");
        assert_eq!(draft.max_concurrent, 9);
        assert_eq!(draft.history_retention_days, 7);
        assert!(form.service_changed);
    }

    #[test]
    fn form_does_not_flag_its_own_save() {
        let old = Settings::default();
        let mut form = SettingsForm::default();
        form.sync(Some(&old));
        form.draft.as_mut().expect("draft").max_concurrent = 9;

        // What the daemon sends back after `UpdateSettings` is what the draft held.
        let saved = form.draft.as_ref().and_then(SettingsDraft::to_settings).expect("valid");
        form.sync(Some(&saved));
        assert!(!form.service_changed);
        assert!(!form.draft.as_ref().expect("draft").is_dirty(&saved));
    }

    #[test]
    fn discard_goes_back_to_the_daemon_settings() {
        let base = Settings::default();
        let mut form = SettingsForm::default();
        form.sync(Some(&base));
        form.draft.as_mut().expect("draft").max_concurrent = 9;
        form.service_changed = true;

        form.discard();
        assert_eq!(form.draft, Some(SettingsDraft::from_settings(&base)));
        assert!(!form.service_changed);
    }

    #[test]
    fn draws_without_connection_or_settings() {
        let mut state = AppState::default();
        let mut view = SettingsView;
        frame(|ui| {
            let _ = view.show(ui, &mut state);
        });
    }

    #[test]
    fn draws_while_loading() {
        let mut state = AppState::default();
        state.connection = connected();
        let mut view = SettingsView;
        frame(|ui| {
            let _ = view.show(ui, &mut state);
        });
    }

    #[test]
    fn draws_the_loaded_defaults() {
        let mut state = AppState::default();
        state.connection = connected();
        state.settings = Some(Settings::default());
        let mut view = SettingsView;
        frame(|ui| {
            let _ = view.show(ui, &mut state);
        });
    }

    #[test]
    fn draws_a_draft_with_errors_and_a_service_change() {
        let base = Settings::default();
        let mut draft = SettingsDraft::from_settings(&base);
        draft.max_concurrent = 0;
        draft.log_per_execution_mib = 900.0;
        draft.timeout_enabled = true;
        draft.timeout_secs = 0;
        let form = SettingsForm {
            draft: Some(draft),
            base: Some(base.clone()),
            service_changed: true,
        };

        let mut state = AppState::default();
        state.connection = connected();
        state.settings = Some(base);
        let mut view = SettingsView;
        frame(|ui| {
            ui.ctx()
                .data_mut(|data| data.insert_temp(egui::Id::unique(FORM_ID), form.clone()));
            let _ = view.show(ui, &mut state);
        });
    }

    #[test]
    fn draws_a_changed_draft() {
        let base = Settings::default();
        let mut draft = SettingsDraft::from_settings(&base);
        draft.max_concurrent = 6;
        draft.scripts_dir = "C:\\Scripts".into();
        let form = SettingsForm {
            draft: Some(draft),
            base: Some(base.clone()),
            service_changed: false,
        };

        let mut state = AppState::default();
        state.connection = connected();
        state.settings = Some(base);
        let mut view = SettingsView;
        frame(|ui| {
            ui.ctx()
                .data_mut(|data| data.insert_temp(egui::Id::unique(FORM_ID), form.clone()));
            let _ = view.show(ui, &mut state);
        });
    }
}
