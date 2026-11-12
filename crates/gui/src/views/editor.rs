//! The task editor (task 3.6): makes a new task or changes one.
//!
//! What is valid, and how the text becomes a `Task`, is decided by [`form`]. This screen shows the form,
//! the problems it reports, and what the user asks for: a save, a file to choose, leaving. The decisions
//! that need no window are plain functions below, and they are the part that is tested directly.

use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};

use chrono::{DateTime, Local, TimeZone, Timelike, Utc};
use eframe::egui::{self, Id, RichText, Ui};
use scriptrunner_core::{RunAs, ScriptKind, TaskId};

use super::{Outcome, theme, ui};
use crate::backend::{Action, Command, Failure};
use crate::fmt::{self, Tone};
use crate::form::{Field, FormError, PREVIEW_RUNS, Problem, ScheduleForm, ScheduleKind, TaskForm, TimeUnit, WEEK};
use crate::i18n::editor::{Text, t};
use crate::i18n::{self, Text as Common};
use crate::state::{AppState, NoticeKind, Screen};

/// The extensions the script filter offers. They must be the ones `ScriptKind::from_path` knows; a test
/// checks that.
const SCRIPT_EXTENSIONS: [&str; 6] = ["py", "pyw", "bat", "cmd", "ps1", "exe"];

#[derive(Default)]
pub struct TaskEditorView {
    /// The form in progress. `None` when the editor is not open.
    session: Option<Session>,
}

/// One visit to the editor: the form being typed and what is needed to act on it.
struct Session {
    /// The task being edited, `None` for a new one.
    target: Option<TaskId>,
    /// The frame in which the editor was last drawn; see [`must_restart`].
    last_frame: u64,
    form: TaskForm,
    /// The form as it was when the visit began, to tell whether leaving would lose anything.
    initial: TaskForm,
    /// Save was pressed while the form had problems (it then reveals them all).
    attempted: bool,
    /// Fields the user has changed, so that "required" problems show next to them.
    touched: Vec<Field>,
    /// A file dialog that is open, if any.
    picker: Option<Picker>,
    /// The id of the task whose save has been sent and not answered yet.
    saving: Option<TaskId>,
    save_error: Option<String>,
    /// The highest notice id handled so far. Notices from before the visit are not for it.
    last_notice: u64,
    preview: Option<Preview>,
    confirm_discard: bool,
    /// How tall the form was in the last frame. Not used to draw: tests read it, because a form whose height
    /// keeps growing is a layout fault that no other number shows.
    content_height: f32,
}

impl Session {
    fn new(target: Option<TaskId>, form: TaskForm, frame: u64, state: &AppState) -> Self {
        // Notices that already exist were made before the editor opened (a save from an earlier visit
        // must not close this one), so the visit starts after the newest of them.
        let last_notice = state.notices_since(0).map(|notice| notice.id).max().unwrap_or(0);
        Self {
            target,
            last_frame: frame,
            initial: form.clone(),
            form,
            attempted: false,
            touched: Vec::new(),
            picker: None,
            saving: None,
            save_error: None,
            content_height: 0.0,
            last_notice,
            preview: None,
            confirm_discard: false,
        }
    }
}

/// A file dialog running on a thread of its own, and the field its answer is for.
struct Picker {
    pick: Pick,
    receiver: mpsc::Receiver<Option<PathBuf>>,
}

/// The three fields that take a path from a dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Script,
    Interpreter,
    WorkingDir,
}

/// The preview of the next runs, and the schedule it was computed for.
pub struct Preview {
    schedule: ScheduleForm,
    runs: Vec<DateTime<Utc>>,
}

/// Which task the editor shows, decided from the screen's target and the task list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    New,
    Existing,
    /// The task list has not arrived yet, so the task may still come.
    Loading,
    NotFound,
}

/// Decides what the editor shows. `requested` is the task the screen names (`None` for a new one), and
/// `found` says whether it is in the task list.
pub fn decide_target(requested: Option<&TaskId>, found: bool, loaded: bool) -> Target {
    match requested {
        None => Target::New,
        Some(_) if found => Target::Existing,
        Some(_) if !loaded => Target::Loading,
        Some(_) => Target::NotFound,
    }
}

/// Whether the form has to be made again.
///
/// The form is kept while the editor stays on the same task. A frame in which the editor was not drawn
/// means the user left it and came back: what they typed before is not resumed. `current` is the target
/// and the last frame of the form in progress, if there is one.
pub fn must_restart(current: Option<(&Option<TaskId>, u64)>, wanted: &Option<TaskId>, frame: u64) -> bool {
    match current {
        None => true,
        Some((target, last_frame)) => target != wanted || frame != last_frame.saturating_add(1),
    }
}

/// What a notice means for a save in progress.
#[derive(Debug, Clone, PartialEq)]
pub enum SaveEvent {
    Saved,
    Failed(Failure),
}

/// The answer to a save, if this notice is one. Only the save we are waiting for counts: a notice about
/// another task is not ours to act on.
pub fn save_event(kind: &NoticeKind, saving: &TaskId) -> Option<SaveEvent> {
    match kind {
        NoticeKind::TaskSaved(task) if task.id == *saving => Some(SaveEvent::Saved),
        NoticeKind::Error {
            action: Action::CreateTask | Action::UpdateTask,
            failure,
        } => Some(SaveEvent::Failed(failure.clone())),
        _ => None,
    }
}

/// The text shown for a failed save. The daemon's message is passed on as it is.
pub fn failure_text(failure: &Failure) -> String {
    match failure {
        Failure::NotConnected => i18n::t(Common::NotConnected).to_owned(),
        Failure::Remote { message, .. } => message.clone(),
        Failure::Other(message) => message.clone(),
    }
}

/// Whether the save button can be pressed: the form is valid, the daemon is there, and no save is pending.
pub fn can_save(valid: bool, connected: bool, saving: bool) -> bool {
    valid && connected && !saving
}

/// Whether leaving the editor would throw away typed text.
pub fn has_changes(form: &TaskForm, initial: &TaskForm) -> bool {
    form != initial
}

/// Puts the path a dialog chose into the field it was opened for. A cancelled dialog changes nothing.
pub fn apply_pick(form: &mut TaskForm, pick: Pick, chosen: Option<PathBuf>) {
    let Some(path) = chosen else {
        return;
    };
    let text = path.to_string_lossy().into_owned();
    match pick {
        Pick::Script => form.script_path = text,
        Pick::Interpreter => form.interpreter = text,
        Pick::WorkingDir => form.working_dir = text,
    }
}

/// The problem to show under `field`, if any. A "required" problem is shown only once the user has tried to
/// save or has changed that field, so a new form does not start out red. Every other problem shows at once.
pub fn visible_problem(errors: &[FormError], field: Field, attempted: bool, touched: &[Field]) -> Option<Problem> {
    let reveal_required = attempted || touched.contains(&field);
    errors
        .iter()
        .filter(|error| error.field == field)
        .map(|error| error.problem)
        .find(|problem| *problem != Problem::Required || reveal_required)
}

/// The message for one problem of one field. Every pair the form can report has its own text.
pub fn problem_text(field: Field, problem: Problem) -> Text {
    match (field, problem) {
        (Field::Name, Problem::Required) => Text::ProblemRequiredName,
        (Field::ScriptPath, Problem::Required) => Text::ProblemRequiredScript,
        (_, Problem::Required) => Text::ProblemRequiredField,
        (_, Problem::UnknownScriptType) => Text::ProblemUnknownScript,
        (_, Problem::NotANumber) => Text::ProblemNotNumber,
        (_, Problem::MustBePositive) => Text::ProblemNotPositive,
        (_, Problem::OutOfRange) => Text::ProblemOutOfRange,
        (_, Problem::BadTime) => Text::ProblemBadTime,
        (_, Problem::NoDaysChosen) => Text::ProblemNoDays,
        (_, Problem::BadCron) => Text::ProblemBadCron,
    }
}

pub fn unit_text(unit: TimeUnit) -> Text {
    match unit {
        TimeUnit::Seconds => Text::UnitSeconds,
        TimeUnit::Minutes => Text::UnitMinutes,
        TimeUnit::Hours => Text::UnitHours,
    }
}

pub fn script_kind_text(kind: ScriptKind) -> Text {
    match kind {
        ScriptKind::Python => Text::TypePython,
        ScriptKind::Bat => Text::TypeBat,
        ScriptKind::Cmd => Text::TypeCmd,
        ScriptKind::PowerShell => Text::TypePowerShell,
        ScriptKind::Exe => Text::TypeExe,
    }
}

/// The preview's instant is the current time rounded down to the minute, so the list does not change
/// second by second.
fn round_to_minute(now: DateTime<Utc>) -> DateTime<Utc> {
    now.with_second(0).and_then(|t| t.with_nanosecond(0)).unwrap_or(now)
}

/// The next runs to show. They are computed once for a given schedule and reused while the schedule does
/// not change, so changing a name or an argument does not recompute them. The runs are cloned out because
/// the cache belongs to the session, which the drawing code also needs.
pub fn preview_runs<Tz: TimeZone>(
    cache: &mut Option<Preview>,
    form: &TaskForm,
    now: DateTime<Utc>,
    zone: &Tz,
) -> Vec<DateTime<Utc>> {
    let stale = cache.as_ref().is_none_or(|preview| preview.schedule != form.schedule);
    if stale {
        let at = round_to_minute(now);
        *cache = Some(Preview {
            schedule: form.schedule.clone(),
            runs: form.next_runs(at, zone, PREVIEW_RUNS),
        });
    }
    cache.as_ref().map(|preview| preview.runs.clone()).unwrap_or_default()
}

/// What the user asked for with the bottom buttons in this frame.
#[derive(Default)]
struct Clicks {
    save: bool,
    cancel: bool,
}

/// How the visit ends after a frame.
enum Flow {
    Stay,
    Leave,
}

/// The answer to "discard the changes?".
enum Answer {
    Discard,
    Keep,
    Open,
}

impl TaskEditorView {
    pub fn show(&mut self, ui: &mut Ui, state: &mut AppState) -> Outcome {
        let mut outcome = Outcome::default();
        let Screen::TaskEditor { task: requested } = state.screen.clone() else {
            return outcome;
        };

        let task = requested.as_ref().and_then(|id| state.task(id));
        match decide_target(requested.as_ref(), task.is_some(), state.tasks_loaded) {
            Target::Loading => {
                ui::page(ui, ui::FORM_WIDTH, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t(Text::Loading));
                    });
                });
                return outcome;
            }
            Target::NotFound => {
                ui::page(ui, ui::FORM_WIDTH, |ui| {
                    ui.heading(t(Text::NotFound));
                    ui.label(t(Text::NotFoundHint));
                    if ui.button(t(Text::Back)).clicked() {
                        outcome.goto(Screen::Dashboard);
                    }
                });
                return outcome;
            }
            Target::New | Target::Existing => {}
        }

        let frame = ui.ctx().cumulative_frame_nr();
        let wanted = requested;
        if must_restart(self.session.as_ref().map(|s| (&s.target, s.last_frame)), &wanted, frame) {
            let form = match task {
                Some(task) => TaskForm::from_task(task),
                None => TaskForm::new_task(),
            };
            self.session = Some(Session::new(wanted, form, frame, state));
        }
        let Some(session) = self.session.as_mut() else {
            return outcome;
        };
        session.last_frame = frame;

        match draw(ui, session, state, &mut outcome) {
            Flow::Stay => {}
            Flow::Leave => {
                self.session = None;
                outcome.goto(Screen::Dashboard);
            }
        }
        outcome
    }
}

fn draw(ui: &mut Ui, session: &mut Session, state: &AppState, outcome: &mut Outcome) -> Flow {
    poll_picker(session);
    match read_notices(session, state) {
        Some(SaveEvent::Saved) => return Flow::Leave,
        Some(SaveEvent::Failed(failure)) => {
            session.saving = None;
            session.save_error = Some(failure_text(&failure));
        }
        None => {}
    }

    let errors = session.form.errors();
    // The title and the buttons stay in place; only the form below them scrolls.
    let clicks = ui::page(ui, ui::FORM_WIDTH, |ui| actions(ui, session, state, errors.is_empty()));

    let area = egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui::page(ui, ui::FORM_WIDTH, |ui| form_ui(ui, session, &errors))
    });
    session.content_height = area.content_size.y;
    let request = area.inner;
    if let Some(pick) = request
        && session.picker.is_none()
    {
        session.picker = start_pick(ui.ctx(), pick);
    }

    if clicks.save {
        save(session, outcome);
    }
    if clicks.cancel {
        if !has_changes(&session.form, &session.initial) {
            return Flow::Leave;
        }
        session.confirm_discard = true;
    }
    if session.confirm_discard {
        match discard_dialog(ui.ctx()) {
            Answer::Discard => return Flow::Leave,
            Answer::Keep => session.confirm_discard = false,
            Answer::Open => {}
        }
    }
    Flow::Stay
}

/// Sends the save. The task is built from the form, so the id it is saved under is known before the answer.
fn save(session: &mut Session, outcome: &mut Outcome) {
    match session.form.to_task(Utc::now()) {
        Ok(task) => {
            let id = task.id.clone();
            let command = if session.target.is_some() {
                Command::UpdateTask(task)
            } else {
                Command::CreateTask(task)
            };
            outcome.send(command);
            session.saving = Some(id);
            session.save_error = None;
        }
        // The button is off while the form has problems, so this is not expected; it still must not panic.
        Err(_) => session.attempted = true,
    }
}

/// Handles the notices that arrived since the last frame. Returns the save's answer, if one came.
fn read_notices(session: &mut Session, state: &AppState) -> Option<SaveEvent> {
    let mut event = None;
    for notice in state.notices_since(session.last_notice) {
        session.last_notice = session.last_notice.max(notice.id);
        if event.is_none()
            && let Some(saving) = &session.saving
        {
            event = save_event(&notice.kind, saving);
        }
    }
    event
}

/// Takes the answer of a finished dialog, if it has come.
fn poll_picker(session: &mut Session) {
    let Some(picker) = session.picker.as_ref() else {
        return;
    };
    let pick = picker.pick;
    let finished = match picker.receiver.try_recv() {
        Ok(chosen) => {
            apply_pick(&mut session.form, pick, chosen);
            true
        }
        Err(TryRecvError::Empty) => false,
        // The dialog's thread ended without an answer: treat it as cancelled.
        Err(TryRecvError::Disconnected) => true,
    };
    if finished {
        session.picker = None;
    }
}

/// Opens a file dialog on a thread of its own. The dialog blocks until the user answers, so it must not
/// run on the thread that draws. Returns `None` if the thread could not be started.
fn start_pick(ctx: &egui::Context, pick: Pick) -> Option<Picker> {
    let (sender, receiver) = mpsc::channel();
    let ctx = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("scriptrunner-file-dialog".into())
        .spawn(move || {
            let chosen = dialog(pick);
            // If the user has already left the editor nobody reads the answer, and that is fine.
            let _ = sender.send(chosen);
            ctx.request_repaint();
        });
    spawned.ok().map(|_| Picker { pick, receiver })
}

fn dialog(pick: Pick) -> Option<PathBuf> {
    match pick {
        Pick::Script => rfd::FileDialog::new()
            .set_title(t(Text::ChooseScriptTitle))
            .add_filter(t(Text::FilterScripts), &SCRIPT_EXTENSIONS)
            .pick_file(),
        Pick::Interpreter => rfd::FileDialog::new()
            .set_title(t(Text::ChooseInterpreterTitle))
            .add_filter(t(Text::FilterExecutables), &["exe"])
            .pick_file(),
        Pick::WorkingDir => rfd::FileDialog::new()
            .set_title(t(Text::ChooseFolderTitle))
            .pick_folder(),
    }
}

fn discard_dialog(ctx: &egui::Context) -> Answer {
    let response = egui::Modal::new(Id::unique("editor_discard_changes")).show(ctx, |ui| {
        ui.set_max_width(ui::DIALOG_WIDTH);
        ui.heading(t(Text::DiscardTitle));
        ui.label(t(Text::DiscardBody));
        ui.add_space(12.0);
        ui::buttons_right(ui, |ui| {
            let discard = ui.button(t(Text::Discard)).clicked();
            let keep = ui.button(t(Text::KeepEditing)).clicked();
            (discard, keep)
        })
    });
    let (discard, keep) = response.inner;
    if discard {
        Answer::Discard
    } else if keep || response.should_close() {
        Answer::Keep
    } else {
        Answer::Open
    }
}

/// The title of the screen with the save and cancel buttons on the right, then the lines that say what holds
/// the save back.
fn actions(ui: &mut Ui, session: &Session, state: &AppState, valid: bool) -> Clicks {
    let saving = session.saving.is_some();
    let mut clicks = Clicks::default();
    let title = t(if session.target.is_some() {
        Text::EditTitle
    } else {
        Text::NewTitle
    });
    let save_enabled = can_save(valid, state.is_connected(), saving);
    let save = ui::primary_button(ui, i18n::t(Common::Save));
    ui::header(ui, title, None, |ui| {
        if ui.add_enabled(save_enabled, save).clicked() {
            clicks.save = true;
        }
        if ui
            .add_enabled(!saving, egui::Button::new(i18n::t(Common::Cancel)))
            .clicked()
        {
            clicks.cancel = true;
        }
    });
    status_lines(ui, session, state, valid);
    clicks
}

/// What holds the save back, and how a save that failed went. Shown under the title.
fn status_lines(ui: &mut Ui, session: &Session, state: &AppState, valid: bool) {
    let bad = theme::tone_color(Tone::Bad, ui.visuals().dark_mode);
    if let Some(error) = &session.save_error {
        ui.label(RichText::new(t(Text::SaveFailed)).color(bad));
        ui.label(RichText::new(error.as_str()).color(bad));
    }
    if session.saving.is_some() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(t(Text::Saving));
        });
    }
    if session.picker.is_some() {
        ui.label(RichText::new(t(Text::PickerOpen)).weak());
    } else if !valid {
        ui.label(RichText::new(t(Text::FixToSave)).weak());
    }
    if !state.is_connected() {
        ui.label(RichText::new(t(Text::NotConnectedHint)).weak());
    }
}

/// The room for a row made of a text field with a button on its right: all the width, one button tall.
///
/// The height matters. A `right_to_left` layout with `Align::Center` asked for directly takes all the height
/// that is left, and a scroll area has none that ends: the row, then the form, grew taller every frame
/// (seen on Windows). Giving the layout a rectangle of a button's height keeps it a row.
fn row_size(ui: &Ui) -> egui::Vec2 {
    egui::vec2(ui.available_width(), ui.spacing().interact_size.y)
}

/// Draws the form, one card per part. Returns the file dialog the user asked for, if any.
fn form_ui(ui: &mut Ui, session: &mut Session, errors: &[FormError]) -> Option<Pick> {
    let mut request = None;
    let busy = session.picker.is_some();

    ui::section(ui, t(Text::SectionGeneral), None, |ui| {
        let name_problem = problem_for(errors, session, Field::Name);
        ui::field(
            ui,
            t(Text::Name),
            |ui| {
                let response =
                    ui.add(egui::TextEdit::singleline(&mut session.form.name).desired_width(ui.available_width()));
                touch_if(&mut session.touched, Field::Name, response.changed());
            },
            |ui| note(ui, Field::Name, name_problem, Text::HelpName),
        );
        ui::field(
            ui,
            t(Text::Description),
            |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut session.form.description)
                        .desired_rows(2)
                        .desired_width(ui.available_width()),
                );
            },
            |ui| help(ui, Text::HelpDescription),
        );
    });

    ui::section(ui, t(Text::SectionScript), Some(t(Text::SectionScriptCaption)), |ui| {
        let kind = session.form.script_kind();
        let problem = problem_for(errors, session, Field::ScriptPath);
        ui::field(
            ui,
            t(Text::Script),
            |ui| {
                ui.allocate_ui_with_layout(row_size(ui), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!busy, egui::Button::new(t(Text::ChooseFile))).clicked() {
                        request = Some(Pick::Script);
                    }
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut session.form.script_path).desired_width(ui.available_width()),
                    );
                    touch_if(&mut session.touched, Field::ScriptPath, response.changed());
                });
            },
            |ui| {
                match kind {
                    Some(kind) => {
                        let text = format!("{} {}", t(Text::DetectedType), t(script_kind_text(kind)));
                        ui.label(RichText::new(text).weak());
                    }
                    None => show_problem(ui, Field::ScriptPath, problem),
                }
                help(ui, Text::HelpScript);
            },
        );
        ui::field(
            ui,
            t(Text::Arguments),
            |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut session.form.args)
                        .desired_rows(3)
                        .desired_width(ui.available_width()),
                );
            },
            |ui| help(ui, Text::HelpArguments),
        );
        ui::field(
            ui,
            t(Text::Interpreter),
            |ui| {
                ui.allocate_ui_with_layout(row_size(ui), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!busy, egui::Button::new(t(Text::ChooseFile))).clicked() {
                        request = Some(Pick::Interpreter);
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut session.form.interpreter).desired_width(ui.available_width()),
                    );
                });
            },
            |ui| help(ui, Text::HelpInterpreter),
        );
        ui::field(
            ui,
            t(Text::WorkingDir),
            |ui| {
                ui.allocate_ui_with_layout(row_size(ui), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(!busy, egui::Button::new(t(Text::ChooseFolder)))
                        .clicked()
                    {
                        request = Some(Pick::WorkingDir);
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut session.form.working_dir).desired_width(ui.available_width()),
                    );
                });
            },
            |ui| help(ui, Text::HelpWorkingDir),
        );
    });

    ui::section(
        ui,
        t(Text::SectionExecution),
        Some(t(Text::SectionExecutionCaption)),
        |ui| {
            let problem = problem_for(errors, session, Field::Timeout);
            ui::field(
                ui,
                t(Text::Timeout),
                |ui| {
                    ui.horizontal(|ui| {
                        let response =
                            ui.add(egui::TextEdit::singleline(&mut session.form.timeout_amount).desired_width(90.0));
                        touch_if(&mut session.touched, Field::Timeout, response.changed());
                        let changed = unit_combo(ui, "timeout_unit", &mut session.form.timeout_unit);
                        touch_if(&mut session.touched, Field::Timeout, changed);
                    });
                },
                |ui| note(ui, Field::Timeout, problem, Text::HelpTimeout),
            );
            ui.checkbox(&mut session.form.allow_concurrent, t(Text::AllowConcurrent));
            help(ui, Text::HelpAllowConcurrent);
            ui.checkbox(&mut session.form.capture_logs, t(Text::CaptureLogs));
            help(ui, Text::HelpCaptureLogs);
            let mut in_user_session = session.form.run_as == RunAs::UserSession;
            if ui.checkbox(&mut in_user_session, t(Text::RunInUserSession)).changed() {
                session.form.run_as = if in_user_session {
                    RunAs::UserSession
                } else {
                    RunAs::Service
                };
            }
            help(ui, Text::HelpRunInUserSession);
        },
    );

    ui::section(
        ui,
        t(Text::SectionSchedule),
        Some(t(Text::SectionScheduleCaption)),
        |ui| {
            if session.form.schedule.kind != ScheduleKind::None {
                ui.checkbox(&mut session.form.schedule_enabled, t(Text::ScheduleEnabled));
                help(ui, Text::HelpScheduleEnabled);
                ui.add_space(4.0);
            }
            ui::field(
                ui,
                t(Text::ScheduleType),
                |ui| kind_combo(ui, &mut session.form.schedule.kind),
                |_| {},
            );
            match session.form.schedule.kind {
                ScheduleKind::None => {}
                ScheduleKind::Interval => interval_row(ui, session, errors),
                ScheduleKind::Daily => time_row(ui, session, errors),
                ScheduleKind::Weekly => {
                    time_row(ui, session, errors);
                    days_row(ui, session, errors);
                }
                ScheduleKind::Cron => cron_row(ui, session, errors),
            }
            ui.add_space(8.0);
            schedule_preview(ui, session);
        },
    );
    request
}

fn interval_row(ui: &mut Ui, session: &mut Session, errors: &[FormError]) {
    let problem = problem_for(errors, session, Field::ScheduleInterval);
    ui::field(
        ui,
        i18n::t(Common::ScheduleEvery),
        |ui| {
            ui.horizontal(|ui| {
                let response =
                    ui.add(egui::TextEdit::singleline(&mut session.form.schedule.interval_amount).desired_width(90.0));
                touch_if(&mut session.touched, Field::ScheduleInterval, response.changed());
                let changed = unit_combo(ui, "interval_unit", &mut session.form.schedule.interval_unit);
                touch_if(&mut session.touched, Field::ScheduleInterval, changed);
            });
        },
        |ui| note(ui, Field::ScheduleInterval, problem, Text::HelpInterval),
    );
}

fn time_row(ui: &mut Ui, session: &mut Session, errors: &[FormError]) {
    let problem = problem_for(errors, session, Field::ScheduleTime);
    ui::field(
        ui,
        t(Text::Time),
        |ui| {
            let response = ui.add(egui::TextEdit::singleline(&mut session.form.schedule.time).desired_width(90.0));
            touch_if(&mut session.touched, Field::ScheduleTime, response.changed());
        },
        |ui| note(ui, Field::ScheduleTime, problem, Text::HelpTime),
    );
}

fn days_row(ui: &mut Ui, session: &mut Session, errors: &[FormError]) {
    let problem = problem_for(errors, session, Field::ScheduleDays);
    ui::field(
        ui,
        t(Text::Days),
        |ui| {
            ui.horizontal(|ui| {
                for (index, day) in WEEK.iter().enumerate() {
                    let on = session.form.schedule.days[index];
                    let name = i18n::weekday_short(i18n::Lang::default(), *day);
                    // Each day is a widget of its own: the id must differ, even though the names are short.
                    ui.push_id(index, |ui| {
                        if ui.selectable_label(on, name).clicked() {
                            session.form.schedule.days[index] = !on;
                            touch_if(&mut session.touched, Field::ScheduleDays, true);
                        }
                    });
                }
            });
        },
        |ui| note(ui, Field::ScheduleDays, problem, Text::HelpDays),
    );
}

fn cron_row(ui: &mut Ui, session: &mut Session, errors: &[FormError]) {
    let problem = problem_for(errors, session, Field::ScheduleCron);
    ui::field(
        ui,
        t(Text::CronExpr),
        |ui| {
            let response =
                ui.add(egui::TextEdit::singleline(&mut session.form.schedule.cron).desired_width(ui.available_width()));
            touch_if(&mut session.touched, Field::ScheduleCron, response.changed());
        },
        |ui| note(ui, Field::ScheduleCron, problem, Text::HelpCron),
    );
}

/// The next runs, as a muted list at the end of the schedule.
fn schedule_preview(ui: &mut Ui, session: &mut Session) {
    ui.label(RichText::new(t(Text::SectionPreview)).strong());
    if session.form.schedule.kind == ScheduleKind::None {
        ui.label(RichText::new(t(Text::PreviewNone)).weak());
        return;
    }
    let runs = preview_runs(&mut session.preview, &session.form, Utc::now(), &Local);
    if runs.is_empty() {
        ui.label(RichText::new(t(Text::PreviewInvalid)).weak());
    } else {
        for run in runs {
            ui.label(RichText::new(fmt::date_time_local(run)).weak());
        }
    }
}

fn kind_combo(ui: &mut Ui, kind: &mut ScheduleKind) {
    egui::ComboBox::from_id_salt("schedule_kind")
        .selected_text(kind_label(*kind))
        .show_ui(ui, |ui| {
            for option in [
                ScheduleKind::None,
                ScheduleKind::Interval,
                ScheduleKind::Daily,
                ScheduleKind::Weekly,
                ScheduleKind::Cron,
            ] {
                ui.selectable_value(kind, option, kind_label(option));
            }
        });
}

fn kind_label(kind: ScheduleKind) -> &'static str {
    match kind {
        ScheduleKind::None => i18n::t(Common::NoSchedule),
        ScheduleKind::Interval => t(Text::KindInterval),
        ScheduleKind::Daily => t(Text::KindDaily),
        ScheduleKind::Weekly => t(Text::KindWeekly),
        ScheduleKind::Cron => t(Text::KindCron),
    }
}

/// A unit choice. Returns whether it changed.
fn unit_combo(ui: &mut Ui, id: &str, unit: &mut TimeUnit) -> bool {
    let before = *unit;
    egui::ComboBox::from_id_salt(id)
        .selected_text(t(unit_text(*unit)))
        .show_ui(ui, |ui| {
            for option in [TimeUnit::Seconds, TimeUnit::Minutes, TimeUnit::Hours] {
                ui.selectable_value(unit, option, t(unit_text(option)));
            }
        });
    *unit != before
}

/// The problem to show under `field`, as the form stands now.
fn problem_for(errors: &[FormError], session: &Session, field: Field) -> Option<Problem> {
    visible_problem(errors, field, session.attempted, &session.touched)
}

/// The problem under a field, in red, if there is one.
fn show_problem(ui: &mut Ui, field: Field, problem: Option<Problem>) {
    if let Some(problem) = problem {
        let color = theme::tone_color(Tone::Bad, ui.visuals().dark_mode);
        ui.label(RichText::new(t(problem_text(field, problem))).color(color));
    }
}

/// The problem under a field (if there is one), then the field's help line.
fn note(ui: &mut Ui, field: Field, problem: Option<Problem>, text: Text) {
    show_problem(ui, field, problem);
    help(ui, text);
}

fn help(ui: &mut Ui, text: Text) {
    ui.label(RichText::new(t(text)).weak().small());
}

fn touch_if(touched: &mut Vec<Field>, field: Field, changed: bool) {
    if changed && !touched.contains(&field) {
        touched.push(field);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use chrono::{FixedOffset, NaiveTime, Weekday};
    use scriptrunner_core::{Schedule, Task, TaskOverview};

    use super::*;
    use crate::backend::{ConnectionState, Reason, Update};
    use crate::i18n::editor::t as text_of;
    use crate::views::testing;

    const FIELDS: [Field; 7] = [
        Field::Name,
        Field::ScriptPath,
        Field::Timeout,
        Field::ScheduleInterval,
        Field::ScheduleTime,
        Field::ScheduleDays,
        Field::ScheduleCron,
    ];

    const PROBLEMS: [Problem; 8] = [
        Problem::Required,
        Problem::UnknownScriptType,
        Problem::NotANumber,
        Problem::MustBePositive,
        Problem::OutOfRange,
        Problem::BadTime,
        Problem::NoDaysChosen,
        Problem::BadCron,
    ];

    /// Not used at run time: a field added to `form.rs` must be added to `FIELDS` too. This match does not
    /// compile until it names every variant, so the list cannot silently fall behind.
    #[test]
    fn the_lists_of_fields_and_problems_are_complete() {
        for field in FIELDS {
            match field {
                Field::Name
                | Field::ScriptPath
                | Field::Timeout
                | Field::ScheduleInterval
                | Field::ScheduleTime
                | Field::ScheduleDays
                | Field::ScheduleCron => {}
            }
        }
        for problem in PROBLEMS {
            match problem {
                Problem::Required
                | Problem::UnknownScriptType
                | Problem::NotANumber
                | Problem::MustBePositive
                | Problem::OutOfRange
                | Problem::BadTime
                | Problem::NoDaysChosen
                | Problem::BadCron => {}
            }
        }
        assert_eq!(FIELDS.len(), 7);
        assert_eq!(PROBLEMS.len(), 8);
    }

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    fn overview(task: Task) -> TaskOverview {
        TaskOverview {
            task,
            running: false,
            last_execution: None,
            next_run: None,
        }
    }

    /// A state with the task list loaded (`list`) and the screen on `screen`. The list comes in through
    /// `apply`, the way the backend delivers it, because `AppState`'s private fields cannot be set here.
    fn with_list(list: Vec<Task>, screen: Screen) -> AppState {
        let mut state = AppState::default();
        state.apply(
            Update::Overview(list.into_iter().map(overview).collect()),
            Instant::now(),
        );
        state.screen = screen;
        state
    }

    /// A new task: the screen's target is "new" and the list has loaded.
    fn creating() -> AppState {
        with_list(Vec::new(), Screen::TaskEditor { task: None })
    }

    /// An existing task, in the list, with the screen on its editor.
    fn editing(task: Task) -> AppState {
        let screen = Screen::TaskEditor {
            task: Some(task.id.clone()),
        };
        with_list(vec![task], screen)
    }

    fn connected(state: &mut AppState) {
        state.connection = ConnectionState::Connected {
            version: "0.1.0".into(),
            protocol: 2,
        };
    }

    fn valid_task() -> Task {
        Task::new("Backup", r"C:\scripts\backup.py")
    }

    /// Draws one frame in a context the caller keeps, so the next frame follows it.
    fn run(ctx: &egui::Context, view: &mut TaskEditorView, state: &mut AppState) -> Outcome {
        let mut outcome = Outcome::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            outcome = view.show(ui, state);
        });
        output.textures_delta.clear();
        outcome
    }

    // ---- layout ----

    /// Draws the editor `frames` times in a window of a fixed size, the way the shell does (inside a
    /// central panel), and returns the width the screen used after each frame.
    fn widths_over_time(
        view: &mut TaskEditorView,
        state: &mut AppState,
        window: egui::Vec2,
        frames: usize,
    ) -> Vec<f32> {
        let ctx = egui::Context::default();
        theme::apply_style(&ctx);
        let mut widths = Vec::new();
        for _ in 0..frames {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, window)),
                ..egui::RawInput::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| view.show(ui, state));
            });
            output.textures_delta.clear();
            widths.push(ctx.globally_used_rect().width());
        }
        widths
    }

    /// A field that asks for "all the width there is" inside a grid inside a scroll area makes the grid
    /// remember a wider column each frame, so the form grows until a horizontal scrollbar appears and keeps
    /// growing. Seen on Windows: the script box kept widening and could not be used.
    #[test]
    fn the_form_never_grows_wider_than_the_window_however_many_frames_are_drawn() {
        for (name, mut state) in [("new", creating()), ("existing", editing(valid_task()))] {
            let mut view = TaskEditorView::default();
            let window = egui::vec2(900.0, 700.0);
            let widths = widths_over_time(&mut view, &mut state, window, 15);
            let last = widths[widths.len() - 1];
            assert!(
                widths.iter().all(|width| *width <= window.x + 0.5),
                "the {name} form outgrew a {} px window: {widths:?}",
                window.x
            );
            assert!(
                (widths[widths.len() - 2] - last).abs() < 0.5,
                "the {name} form was still changing width after 15 frames: {widths:?}"
            );
        }
    }

    /// The height of the form after each of `frames` frames drawn in one context.
    fn heights_over_time(
        view: &mut TaskEditorView,
        state: &mut AppState,
        window: egui::Vec2,
        frames: usize,
    ) -> Vec<f32> {
        let ctx = egui::Context::default();
        theme::apply_style(&ctx);
        let mut heights = Vec::new();
        for _ in 0..frames {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, window)),
                ..egui::RawInput::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| view.show(ui, state));
            });
            output.textures_delta.clear();
            heights.push(view.session.as_ref().map_or(0.0, |session| session.content_height));
        }
        heights
    }

    /// A `right_to_left` layout asked for directly in a cell of a scroll area takes all the height that is
    /// left, and in a scroll area that has no end: the form got taller every frame and its middle went blank
    /// (seen on Windows, in the script section). Every row must have the height of its widgets.
    #[test]
    fn the_form_has_a_finite_height_that_stops_changing() {
        for (name, mut state) in [("new", creating()), ("existing", editing(valid_task()))] {
            let mut view = TaskEditorView::default();
            let heights = heights_over_time(&mut view, &mut state, egui::vec2(900.0, 700.0), 15);
            let last = heights[heights.len() - 1];
            assert!(
                (heights[heights.len() - 2] - last).abs() < 0.5,
                "the {name} form was still changing height after 15 frames: {heights:?}"
            );
            // The whole form is a few dozen rows. Anything near a screenful of screens is runaway growth,
            // and anything very short means the rows collapsed.
            assert!(
                last < 2_500.0,
                "the {name} form is absurdly tall ({last} px): {heights:?}"
            );
            assert!(
                last > 400.0,
                "the {name} form is too short to hold its fields ({last} px)"
            );
        }
    }

    #[test]
    fn the_form_also_fits_a_narrow_window() {
        let mut view = TaskEditorView::default();
        let mut state = creating();
        let window = egui::vec2(480.0, 600.0);
        let widths = widths_over_time(&mut view, &mut state, window, 15);
        assert!(
            widths.iter().all(|width| *width <= window.x + 0.5),
            "the form outgrew a {} px window: {widths:?}",
            window.x
        );
    }

    // ---- (a) which task the editor shows ----

    #[test]
    fn a_new_task_never_waits_for_the_list() {
        assert_eq!(decide_target(None, false, false), Target::New);
        assert_eq!(decide_target(None, false, true), Target::New);
    }

    #[test]
    fn an_existing_task_is_shown_once_it_is_in_the_list() {
        let id = TaskId::new();
        assert_eq!(decide_target(Some(&id), true, false), Target::Existing);
        assert_eq!(decide_target(Some(&id), true, true), Target::Existing);
    }

    #[test]
    fn a_task_not_yet_in_the_list_is_loading_until_the_list_arrives() {
        let id = TaskId::new();
        assert_eq!(decide_target(Some(&id), false, false), Target::Loading);
    }

    #[test]
    fn a_task_missing_from_a_loaded_list_is_not_found() {
        let id = TaskId::new();
        assert_eq!(decide_target(Some(&id), false, true), Target::NotFound);
    }

    // ---- (b) every problem has a text ----

    #[test]
    fn every_field_and_problem_the_form_can_report_has_a_text() {
        for field in FIELDS {
            for problem in PROBLEMS {
                assert!(
                    !text_of(problem_text(field, problem)).is_empty(),
                    "{field:?} {problem:?}"
                );
            }
        }
    }

    #[test]
    fn the_problems_the_form_reports_have_their_own_text() {
        for (field, problem, expected) in [
            (Field::Name, Problem::Required, "Informe o nome da tarefa."),
            (Field::ScriptPath, Problem::Required, "Informe o caminho do script."),
            (
                Field::ScriptPath,
                Problem::UnknownScriptType,
                "O tipo do script não é reconhecido (use .py, .bat, .cmd, .ps1 ou .exe).",
            ),
            (Field::Timeout, Problem::NotANumber, "Digite um número inteiro."),
            (
                Field::ScheduleInterval,
                Problem::MustBePositive,
                "Deve ser maior que zero.",
            ),
            (Field::ScheduleInterval, Problem::OutOfRange, "Valor fora do permitido."),
            (Field::ScheduleTime, Problem::BadTime, "Horário inválido (use HH:MM)."),
            (Field::ScheduleDays, Problem::NoDaysChosen, "Escolha pelo menos um dia."),
            (Field::ScheduleCron, Problem::BadCron, "Expressão cron inválida."),
        ] {
            assert_eq!(text_of(problem_text(field, problem)), expected, "{field:?} {problem:?}");
        }
    }

    #[test]
    fn every_script_extension_in_the_dialog_is_one_the_form_knows() {
        for extension in SCRIPT_EXTENSIONS {
            let path = format!("a.{extension}");
            assert!(
                ScriptKind::from_path(std::path::Path::new(&path)).is_some(),
                "{extension} is offered but not recognised"
            );
        }
    }

    // ---- (c) when a required problem shows ----

    #[test]
    fn a_required_problem_waits_until_the_user_tries_or_edits_that_field() {
        let errors = [FormError {
            field: Field::Name,
            problem: Problem::Required,
        }];
        assert_eq!(visible_problem(&errors, Field::Name, false, &[]), None);
        assert_eq!(
            visible_problem(&errors, Field::Name, true, &[]),
            Some(Problem::Required)
        );
        assert_eq!(
            visible_problem(&errors, Field::Name, false, &[Field::Name]),
            Some(Problem::Required)
        );
        // Another field's edit does not reveal this one.
        assert_eq!(visible_problem(&errors, Field::Name, false, &[Field::Timeout]), None);
    }

    #[test]
    fn the_other_problems_show_at_once() {
        let errors = [FormError {
            field: Field::Timeout,
            problem: Problem::NotANumber,
        }];
        assert_eq!(
            visible_problem(&errors, Field::Timeout, false, &[]),
            Some(Problem::NotANumber)
        );
        assert_eq!(visible_problem(&errors, Field::Name, false, &[]), None);
    }

    // ---- (d) a file dialog's answer goes to its field ----

    fn path_field(form: &TaskForm, pick: Pick) -> &str {
        match pick {
            Pick::Script => &form.script_path,
            Pick::Interpreter => &form.interpreter,
            Pick::WorkingDir => &form.working_dir,
        }
    }

    #[test]
    fn a_chosen_path_goes_to_the_field_it_was_opened_for_and_no_other() {
        let path = r"C:\Meus Scripts\rel.ps1";
        for pick in [Pick::Script, Pick::Interpreter, Pick::WorkingDir] {
            let mut form = TaskForm::new_task();
            apply_pick(&mut form, pick, Some(PathBuf::from(path)));
            assert_eq!(path_field(&form, pick), path, "{pick:?}");
            for other in [Pick::Script, Pick::Interpreter, Pick::WorkingDir] {
                if other != pick {
                    assert_eq!(path_field(&form, other), "", "{pick:?} leaked into {other:?}");
                }
            }
        }
    }

    #[test]
    fn a_cancelled_dialog_changes_nothing() {
        let before = TaskForm::new_task();
        let mut form = before.clone();
        apply_pick(&mut form, Pick::Script, None);
        apply_pick(&mut form, Pick::WorkingDir, None);
        assert_eq!(form, before);
    }

    // ---- (e) when the form is made again ----

    #[test]
    fn the_form_is_made_when_there_is_none() {
        assert!(must_restart(None, &None, 0));
    }

    #[test]
    fn the_form_is_kept_while_the_editor_is_drawn_every_frame_on_the_same_task() {
        let id = TaskId::new();
        assert!(!must_restart(Some((&Some(id.clone()), 4)), &Some(id), 5));
        assert!(!must_restart(Some((&None, 0)), &None, 1));
    }

    #[test]
    fn the_form_is_made_again_for_another_task() {
        let id = TaskId::new();
        assert!(must_restart(Some((&None, 4)), &Some(id), 5));
        assert!(must_restart(Some((&Some(TaskId::new()), 4)), &Some(TaskId::new()), 5));
    }

    #[test]
    fn the_form_is_made_again_after_the_editor_was_left_for_a_frame() {
        assert!(must_restart(Some((&None, 4)), &None, 6));
        // The same frame twice is not a continuation either.
        assert!(must_restart(Some((&None, 4)), &None, 4));
    }

    #[test]
    fn leaving_and_coming_back_starts_from_the_saved_task_not_the_draft() {
        let ctx = egui::Context::default();
        let task = valid_task();
        let mut state = editing(task.clone());
        connected(&mut state);
        let mut view = TaskEditorView::default();

        run(&ctx, &mut view, &mut state);
        view.session.as_mut().unwrap().form.name = "rascunho".into();
        run(&ctx, &mut view, &mut state);
        assert_eq!(view.session.as_ref().unwrap().form.name, "rascunho", "kept while drawn");

        // A frame without the editor: the user went to the dashboard.
        let mut output = ctx.run_ui(egui::RawInput::default(), |_ui| {});
        output.textures_delta.clear();
        run(&ctx, &mut view, &mut state);
        assert_eq!(view.session.as_ref().unwrap().form.name, "Backup");
    }

    // ---- (f) what the notices mean for a save ----

    #[test]
    fn the_save_of_the_task_being_saved_closes_the_editor() {
        let task = valid_task();
        assert_eq!(
            save_event(&NoticeKind::TaskSaved(Box::new(task.clone())), &task.id),
            Some(SaveEvent::Saved)
        );
    }

    #[test]
    fn the_save_of_another_task_is_none_of_our_business() {
        let ours = valid_task();
        let other = Task::new("Outra", "b.py");
        assert_eq!(save_event(&NoticeKind::TaskSaved(Box::new(other)), &ours.id), None);
    }

    #[test]
    fn a_failed_save_is_reported_and_other_failures_are_not() {
        let id = valid_task().id;
        let failure = Failure::Other("x".into());
        for action in [Action::CreateTask, Action::UpdateTask] {
            assert_eq!(
                save_event(
                    &NoticeKind::Error {
                        action,
                        failure: failure.clone()
                    },
                    &id
                ),
                Some(SaveEvent::Failed(failure.clone()))
            );
        }
        for action in [Action::DeleteTask, Action::RunNow] {
            assert_eq!(
                save_event(
                    &NoticeKind::Error {
                        action,
                        failure: failure.clone()
                    },
                    &id
                ),
                None
            );
        }
        assert_eq!(save_event(&NoticeKind::Done(Action::CreateTask), &id), None);
    }

    #[test]
    fn a_failure_says_what_went_wrong() {
        assert_eq!(failure_text(&Failure::NotConnected), i18n::t(Common::NotConnected));
        assert_eq!(
            failure_text(&Failure::Remote {
                code: scriptrunner_ipc::ErrorCode::ProtocolMismatch,
                message: "nome repetido".into(),
            }),
            "nome repetido"
        );
        assert_eq!(failure_text(&Failure::Other("disco cheio".into())), "disco cheio");
    }

    #[test]
    fn save_needs_a_valid_form_a_connection_and_nothing_pending() {
        assert!(can_save(true, true, false));
        assert!(!can_save(false, true, false));
        assert!(!can_save(true, false, false));
        assert!(!can_save(true, true, true));
    }

    #[test]
    fn leaving_asks_only_when_something_was_typed() {
        let initial = TaskForm::new_task();
        assert!(!has_changes(&initial.clone(), &initial));
        let mut changed = initial.clone();
        changed.name.push('x');
        assert!(has_changes(&changed, &initial));
    }

    // ---- the preview ----

    #[test]
    fn the_preview_instant_is_the_minute_and_is_kept_while_the_schedule_is_unchanged() {
        let mut form = TaskForm::new_task();
        form.schedule.kind = ScheduleKind::Interval;
        form.schedule.interval_amount = "10".into();
        form.schedule.interval_unit = TimeUnit::Minutes;
        let mut cache = None;

        // Computed from 12:00 (the minute of 12:00:37): the first run is 12:10.
        let first = preview_runs(&mut cache, &form, at("2026-11-12T12:00:37Z"), &Utc);
        assert_eq!(first[0], at("2026-11-12T12:10:00Z"));

        // Later, with the same schedule: nothing is recomputed, so the runs stay the same.
        let later = preview_runs(&mut cache, &form, at("2026-11-12T12:05:10Z"), &Utc);
        assert_eq!(later, first);

        // Another field changing does not recompute either.
        form.name = "outro".into();
        let renamed = preview_runs(&mut cache, &form, at("2026-11-12T12:07:00Z"), &Utc);
        assert_eq!(renamed, first);

        // A change of schedule does: the new runs start from the minute of now (12:05).
        form.schedule.interval_amount = "15".into();
        let changed = preview_runs(&mut cache, &form, at("2026-11-12T12:05:10Z"), &Utc);
        assert_eq!(changed[0], at("2026-11-12T12:20:00Z"));
    }

    #[test]
    fn an_invalid_schedule_has_no_runs_to_show() {
        let mut form = TaskForm::new_task();
        form.schedule.kind = ScheduleKind::Cron;
        form.schedule.cron = "isto não é cron".into();
        let mut cache = None;
        assert!(preview_runs(&mut cache, &form, Utc::now(), &FixedOffset::west_opt(0).unwrap()).is_empty());
    }

    #[test]
    fn the_preview_instant_rounds_down_to_the_minute() {
        assert_eq!(
            round_to_minute(at("2026-11-12T12:34:56.789Z")),
            at("2026-11-12T12:34:00Z")
        );
    }

    // ---- drawing without a window ----

    #[test]
    fn a_new_blank_task_draws() {
        let mut view = TaskEditorView::default();
        let mut state = creating();
        testing::frame(|ui| {
            view.show(ui, &mut state);
        });
    }

    #[test]
    fn an_existing_task_draws_with_each_kind_of_schedule() {
        let schedules = [
            None,
            Some(Schedule::Interval {
                every_secs: 900,
                anchor: at("2026-01-01T00:00:00Z"),
            }),
            Some(Schedule::Daily {
                time: NaiveTime::from_hms_opt(9, 30, 0).unwrap(),
            }),
            Some(Schedule::Weekly {
                days: vec![Weekday::Mon, Weekday::Fri],
                time: NaiveTime::from_hms_opt(8, 5, 30).unwrap(),
            }),
            Some(Schedule::Cron {
                expr: "0 9 * * 1-5".into(),
            }),
        ];
        for schedule in schedules {
            let mut task = valid_task();
            task.schedule = schedule;
            let mut view = TaskEditorView::default();
            let mut state = editing(task);
            connected(&mut state);
            testing::frame(|ui| {
                view.show(ui, &mut state);
            });
        }
    }

    #[test]
    fn a_form_with_problems_draws() {
        let mut view = TaskEditorView::default();
        let mut state = creating();
        let ctx = egui::Context::default();
        run(&ctx, &mut view, &mut state);
        let session = view.session.as_mut().unwrap();
        session.attempted = true;
        session.form.timeout_amount = "x".into();
        session.form.schedule.kind = ScheduleKind::Weekly;
        session.form.schedule.days = [false; 7];
        run(&ctx, &mut view, &mut state);
    }

    #[test]
    fn a_save_in_progress_draws() {
        let mut view = TaskEditorView::default();
        let mut state = editing(valid_task());
        connected(&mut state);
        let ctx = egui::Context::default();
        run(&ctx, &mut view, &mut state);
        let id = view.session.as_ref().unwrap().form.to_task(Utc::now()).unwrap().id;
        view.session.as_mut().unwrap().saving = Some(id);
        run(&ctx, &mut view, &mut state);
    }

    #[test]
    fn a_daemon_refusal_draws() {
        let mut view = TaskEditorView::default();
        let mut state = editing(valid_task());
        connected(&mut state);
        let ctx = egui::Context::default();
        run(&ctx, &mut view, &mut state);
        view.session.as_mut().unwrap().save_error = Some(failure_text(&Failure::Remote {
            code: scriptrunner_ipc::ErrorCode::ProtocolMismatch,
            message: "a tarefa já existe".into(),
        }));
        run(&ctx, &mut view, &mut state);
    }

    #[test]
    fn a_task_still_loading_draws() {
        let id = TaskId::new();
        let mut view = TaskEditorView::default();
        // No list yet: the task may still come.
        let mut state = with_list(Vec::new(), Screen::TaskEditor { task: Some(id) });
        state.tasks_loaded = false;
        testing::frame(|ui| {
            view.show(ui, &mut state);
        });
    }

    #[test]
    fn a_task_not_found_draws() {
        let id = TaskId::new();
        let mut view = TaskEditorView::default();
        let mut state = with_list(Vec::new(), Screen::TaskEditor { task: Some(id) });
        testing::frame(|ui| {
            view.show(ui, &mut state);
        });
    }

    #[test]
    fn a_disconnected_editor_draws() {
        let mut view = TaskEditorView::default();
        let mut state = editing(valid_task());
        state.connection = ConnectionState::Down {
            reason: Reason::Lost,
            retry_in: std::time::Duration::from_secs(1),
        };
        testing::frame(|ui| {
            view.show(ui, &mut state);
        });
    }

    #[test]
    fn the_discard_confirmation_draws() {
        let mut view = TaskEditorView::default();
        let mut state = creating();
        let ctx = egui::Context::default();
        run(&ctx, &mut view, &mut state);
        let session = view.session.as_mut().unwrap();
        session.form.name = "mudança".into();
        session.confirm_discard = true;
        run(&ctx, &mut view, &mut state);
    }

    // ---- the notices, through the screen ----

    #[test]
    fn the_saved_task_closes_the_editor_and_goes_to_the_dashboard() {
        let task = valid_task();
        let ctx = egui::Context::default();
        let mut view = TaskEditorView::default();
        let mut state = editing(task.clone());
        connected(&mut state);
        run(&ctx, &mut view, &mut state);
        view.session.as_mut().unwrap().saving = Some(task.id.clone());

        state.apply(Update::TaskSaved(task), Instant::now());
        let outcome = run(&ctx, &mut view, &mut state);

        assert_eq!(outcome.navigate, Some(Screen::Dashboard));
        assert!(view.session.is_none(), "the draft is dropped with the visit");
    }

    #[test]
    fn a_save_of_another_task_does_not_close_the_editor() {
        let ctx = egui::Context::default();
        let mut view = TaskEditorView::default();
        let mut state = editing(valid_task());
        connected(&mut state);
        run(&ctx, &mut view, &mut state);
        let ours = valid_task();
        view.session.as_mut().unwrap().saving = Some(ours.id);

        state.apply(Update::TaskSaved(Task::new("Outra", "b.py")), Instant::now());
        let outcome = run(&ctx, &mut view, &mut state);

        assert_eq!(outcome.navigate, None);
        assert!(view.session.as_ref().unwrap().saving.is_some());
    }

    #[test]
    fn a_refused_save_stops_waiting_and_says_why() {
        let ctx = egui::Context::default();
        let mut view = TaskEditorView::default();
        let mut state = editing(valid_task());
        connected(&mut state);
        run(&ctx, &mut view, &mut state);
        let task = valid_task();
        view.session.as_mut().unwrap().saving = Some(task.id);

        state.apply(
            Update::Failed {
                action: Action::UpdateTask,
                failure: Failure::Other("disco cheio".into()),
            },
            Instant::now(),
        );
        let outcome = run(&ctx, &mut view, &mut state);

        assert_eq!(outcome.navigate, None);
        let session = view.session.as_ref().unwrap();
        assert!(session.saving.is_none());
        assert_eq!(session.save_error.as_deref(), Some("disco cheio"));
    }

    #[test]
    fn a_notice_from_before_the_visit_is_not_acted_on() {
        let task = valid_task();
        let ctx = egui::Context::default();
        let mut view = TaskEditorView::default();
        let mut state = editing(task.clone());
        connected(&mut state);
        // Saved in an earlier visit, before this one opened.
        state.apply(Update::TaskSaved(task.clone()), Instant::now());

        run(&ctx, &mut view, &mut state);
        view.session.as_mut().unwrap().saving = Some(task.id);
        let outcome = run(&ctx, &mut view, &mut state);

        assert_eq!(outcome.navigate, None);
    }
}
