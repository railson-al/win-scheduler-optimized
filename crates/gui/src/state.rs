//! What the window knows, and how each [`Update`] from the backend changes it (plan D17, D19).
//!
//! No egui here: the screens read this state and draw it, and send the [`Command`]s that user actions
//! and [`AppState::apply`] produce. Because nothing in this module needs a window, every rule below is
//! tested directly.
//!
//! The principle is D17: the daemon is the source of truth. The task list is replaced wholesale by each
//! `Overview`; the history page, the open execution and the log are refetched when events may have been
//! lost (`Resynced`) or the connection came back. The state never guesses a task's status from a click.
//!
//! Side effects are explicit: [`AppState::apply`] and [`AppState::go`] return the commands the caller
//! must send, instead of sending them, so a test can see exactly what a transition asks for.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

use chrono::{DateTime, Utc};
use scriptrunner_core::logs::{LogChunk, LogEntry, LogKind, format_entry};
use scriptrunner_core::{
    Execution, ExecutionId, ExecutionQuery, ExecutionStatus, Settings, Task, TaskId, TaskOverview,
};
use scriptrunner_ipc::StatusInfo;

use crate::backend::{Action, Command, ConnectionState, Failure, LogUpdate, Update};

/// Rows per page of the history.
pub const HISTORY_PAGE_SIZE: u32 = 50;
/// Lines of one log kept in memory. A 5 MiB log (the default limit) is well below this; past it the oldest
/// lines are dropped from the screen (the file, and its export, still have them).
pub const MAX_LOG_ENTRIES: usize = 200_000;
/// Notices kept for the screens to show.
pub const MAX_NOTICES: usize = 50;

/// Which screen is showing. Leaving or entering one has side effects; see [`AppState::go`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Dashboard,
    /// `None` is a new task.
    TaskEditor {
        task: Option<TaskId>,
    },
    History,
    /// The detail and live log of one execution.
    Execution(ExecutionId),
    Settings,
}

/// What the daemon last said about itself, and when we heard it.
#[derive(Debug, Clone)]
pub struct DaemonStatus {
    pub info: StatusInfo,
    heard_at: Instant,
}

impl DaemonStatus {
    /// Uptime now: what the daemon reported plus the time since it did.
    pub fn uptime_secs(&self, now: Instant) -> u64 {
        self.info.uptime_secs + now.saturating_duration_since(self.heard_at).as_secs()
    }
}

#[derive(Debug)]
pub struct AppState {
    pub screen: Screen,
    pub connection: ConnectionState,
    pub status: Option<DaemonStatus>,
    pub tasks: Vec<TaskOverview>,
    /// `true` once the daemon has sent a task list, so an empty list can be told from "not loaded yet".
    pub tasks_loaded: bool,
    /// What the dashboard's search box holds.
    pub search: String,
    pub settings: Option<Settings>,
    pub history: HistoryState,
    /// The execution open on [`Screen::Execution`].
    pub log: Option<LogView>,
    notices: VecDeque<Notice>,
    next_notice: u64,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            screen: Screen::Dashboard,
            connection: ConnectionState::Connecting,
            status: None,
            tasks: Vec::new(),
            tasks_loaded: false,
            search: String::new(),
            settings: None,
            history: HistoryState::default(),
            log: None,
            notices: VecDeque::new(),
            next_notice: 1,
        }
    }
}

impl AppState {
    pub fn is_connected(&self) -> bool {
        matches!(self.connection, ConnectionState::Connected { .. })
    }

    /// The tasks whose name contains the search text, ignoring case, in the daemon's order.
    pub fn visible_tasks(&self) -> Vec<&TaskOverview> {
        let needle = self.search.trim().to_lowercase();
        self.tasks
            .iter()
            .filter(|row| needle.is_empty() || row.task.name.to_lowercase().contains(&needle))
            .collect()
    }

    pub fn task(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.iter().map(|row| &row.task).find(|task| &task.id == id)
    }

    pub fn running_count(&self) -> usize {
        self.tasks.iter().filter(|row| row.running).count()
    }

    /// Notices newer than `last_seen` (0 for all of them). A screen remembers the highest `id` it has
    /// handled; this way several screens can each react to the same notice.
    pub fn notices_since(&self, last_seen: u64) -> impl Iterator<Item = &Notice> {
        self.notices.iter().filter(move |notice| notice.id > last_seen)
    }

    /// Switches screen. Returns what to ask the backend for: opening the history queries it, opening an
    /// execution loads it and follows its log, and leaving an execution stops following.
    pub fn go(&mut self, screen: Screen) -> Vec<Command> {
        if screen == self.screen {
            return Vec::new();
        }
        let mut commands = Vec::new();
        if matches!(self.screen, Screen::Execution(_)) {
            self.log = None;
            commands.push(Command::StopFollowing);
        }
        if self.screen == Screen::History {
            self.history.active = false;
        }
        match &screen {
            Screen::Execution(id) => {
                self.log = Some(LogView::new(id.clone()));
                commands.push(Command::GetExecution(id.clone()));
                commands.push(Command::FollowLog {
                    id: id.clone(),
                    offset: 0,
                });
            }
            Screen::History => {
                self.history.active = true;
                commands.push(self.history.request());
            }
            Screen::Dashboard | Screen::TaskEditor { .. } | Screen::Settings => {}
        }
        self.screen = screen;
        commands
    }

    /// Follows the open log again from where it stopped (after a failure the user chose to retry).
    pub fn retry_log(&mut self) -> Vec<Command> {
        let Some(log) = &mut self.log else {
            return Vec::new();
        };
        log.failure = None;
        vec![
            Command::GetExecution(log.id.clone()),
            Command::FollowLog {
                id: log.id.clone(),
                offset: log.next_offset,
            },
        ]
    }

    /// Applies one update from the backend. `now` is passed in so the state needs no clock of its own.
    /// Returns the commands the caller must send in answer.
    pub fn apply(&mut self, update: Update, now: Instant) -> Vec<Command> {
        match update {
            Update::Connection(state) => {
                let connected = matches!(state, ConnectionState::Connected { .. });
                let reconnected = connected && !self.is_connected();
                if !connected {
                    self.status = None;
                }
                self.connection = state;
                if reconnected {
                    // The backend's log follower ended with the old connection: start it again.
                    return self.refetch(true);
                }
            }
            Update::Overview(rows) => {
                self.tasks = rows;
                self.tasks_loaded = true;
            }
            Update::Status(info) => self.status = Some(DaemonStatus { info, heard_at: now }),
            Update::Settings(settings) => self.settings = Some(settings),
            Update::ExecutionChanged(execution) => {
                self.history.patch(&execution);
                if let Some(log) = &mut self.log
                    && log.id == execution.id
                {
                    log.execution = Some(execution);
                }
            }
            // Events were lost. The log follower reads the file, not events, so it is unaffected.
            Update::Resynced => return self.refetch(false),
            Update::History { tag, rows } => return self.history.receive(tag, rows).into_iter().collect(),
            Update::ExecutionLoaded { id, execution } => {
                if let Some(log) = &mut self.log
                    && log.id == id
                {
                    log.missing = execution.is_none();
                    log.execution = execution;
                }
            }
            Update::Log(update) => self.apply_log(update),
            Update::Started { task, execution } => self.notify(now, NoticeKind::Started { task, execution }),
            Update::Stopped { task, count } => self.notify(now, NoticeKind::Stopped { task, count }),
            Update::TaskSaved(task) => self.notify(now, NoticeKind::TaskSaved(Box::new(task))),
            Update::Exported { id, path, bytes } => self.notify(now, NoticeKind::Exported { id, path, bytes }),
            Update::Succeeded(action) => {
                self.notify(now, NoticeKind::Done(action));
                if action == Action::ClearHistory && self.history.active {
                    return vec![self.history.request()];
                }
            }
            Update::Failed { action, failure } => self.apply_failure(action, failure, now),
        }
        Vec::new()
    }

    /// What to ask for again after events may have been lost or the connection came back.
    fn refetch(&mut self, restart_log: bool) -> Vec<Command> {
        let mut commands = Vec::new();
        if self.history.active {
            commands.push(self.history.request());
        }
        if let Some(log) = &mut self.log {
            commands.push(Command::GetExecution(log.id.clone()));
            if restart_log && !log.finished {
                log.failure = None;
                commands.push(Command::FollowLog {
                    id: log.id.clone(),
                    offset: log.next_offset,
                });
            }
        }
        commands
    }

    fn apply_log(&mut self, update: LogUpdate) {
        // An update for a log that is no longer open (the user moved on) is simply dropped.
        let (id, change) = match update {
            LogUpdate::Chunk { id, chunk } => (id, LogChange::Chunk(chunk)),
            LogUpdate::Finished { id } => (id, LogChange::Finished),
            LogUpdate::Failed { id, failure } => (id, LogChange::Failed(failure)),
        };
        let Some(log) = self.log.as_mut().filter(|log| log.id == id) else {
            return;
        };
        match change {
            LogChange::Chunk(chunk) => log.receive(chunk),
            LogChange::Finished => log.finished = true,
            LogChange::Failed(failure) => log.failure = Some(failure),
        }
    }

    fn apply_failure(&mut self, action: Action, failure: Failure, now: Instant) {
        match action {
            // These have a place on their own screen, which shows the failure next to what failed.
            Action::QueryHistory => {
                self.history.loading = false;
                self.history.failure = Some(failure);
            }
            Action::GetExecution | Action::FollowLog => {
                if let Some(log) = &mut self.log {
                    log.failure = Some(failure);
                }
            }
            _ => self.notify(now, NoticeKind::Error { action, failure }),
        }
    }

    fn notify(&mut self, at: Instant, kind: NoticeKind) {
        let id = self.next_notice;
        self.next_notice += 1;
        self.notices.push_back(Notice { id, at, kind });
        while self.notices.len() > MAX_NOTICES {
            self.notices.pop_front();
        }
    }
}

enum LogChange {
    Chunk(LogChunk),
    Finished,
    Failed(Failure),
}

/// Something the user should be told, once: the result of what they just did.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    /// Increases with every notice, for [`AppState::notices_since`].
    pub id: u64,
    pub at: Instant,
    pub kind: NoticeKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NoticeKind {
    Started {
        task: TaskId,
        execution: ExecutionId,
    },
    Stopped {
        task: TaskId,
        count: u32,
    },
    /// A task was created, edited or duplicated. (The editor closes on this.)
    TaskSaved(Box<Task>),
    Exported {
        id: ExecutionId,
        path: PathBuf,
        bytes: u64,
    },
    /// Something worked and has nothing more to say.
    Done(Action),
    Error {
        action: Action,
        failure: Failure,
    },
}

/// The history screen: a filter, a page, and the rows the daemon last returned for them.
#[derive(Debug, Default)]
pub struct HistoryState {
    pub filter: HistoryFilter,
    /// Zero-based.
    pub page: u32,
    pub rows: Vec<Execution>,
    pub has_next: bool,
    pub loading: bool,
    pub failure: Option<Failure>,
    /// Whether the history screen is showing. When it is not, nothing is refetched for it.
    pub active: bool,
    /// Identifies the latest query, so an answer to an older one (a filter changed meanwhile) is ignored.
    tag: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryFilter {
    pub task: Option<TaskId>,
    pub status: Option<ExecutionStatus>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}

impl HistoryFilter {
    fn matches(&self, execution: &Execution) -> bool {
        self.task.as_ref().is_none_or(|task| *task == execution.task_id)
            && self.status.is_none_or(|status| status == execution.status)
            && self.since.is_none_or(|since| execution.created_at >= since)
            && self.until.is_none_or(|until| execution.created_at <= until)
    }
}

impl HistoryState {
    /// Queries the current page. One more row than a page is asked for, to know whether there is a next one.
    pub fn request(&mut self) -> Command {
        self.tag += 1;
        self.loading = true;
        self.failure = None;
        Command::QueryHistory {
            tag: self.tag,
            query: ExecutionQuery {
                task_id: self.filter.task.clone(),
                status: self.filter.status,
                since: self.filter.since,
                until: self.filter.until,
                limit: Some(HISTORY_PAGE_SIZE + 1),
                offset: self.page * HISTORY_PAGE_SIZE,
            },
        }
    }

    /// A new filter starts again from the first page.
    pub fn set_filter(&mut self, filter: HistoryFilter) -> Command {
        self.filter = filter;
        self.page = 0;
        self.request()
    }

    pub fn next_page(&mut self) -> Option<Command> {
        if !self.has_next {
            return None;
        }
        self.page += 1;
        Some(self.request())
    }

    pub fn previous_page(&mut self) -> Option<Command> {
        if self.page == 0 {
            return None;
        }
        self.page -= 1;
        Some(self.request())
    }

    /// Takes the answer to a query. `None` for "nothing more to ask".
    fn receive(&mut self, tag: u64, mut rows: Vec<Execution>) -> Option<Command> {
        if tag != self.tag {
            return None;
        }
        self.loading = false;
        // The page we were on has emptied (its rows were cleared or aged out): show the one before it.
        if rows.is_empty() && self.page > 0 {
            self.page -= 1;
            return Some(self.request());
        }
        self.has_next = rows.len() > HISTORY_PAGE_SIZE as usize;
        rows.truncate(HISTORY_PAGE_SIZE as usize);
        self.rows = rows;
        None
    }

    /// Keeps the rows on screen current as executions change, without a refetch: an execution already
    /// shown is updated (or removed if it stopped matching the filter), and a new one that matches the
    /// filter appears at the top of the first page.
    fn patch(&mut self, execution: &Execution) {
        if !self.active {
            return;
        }
        let matches = self.filter.matches(execution);
        if let Some(position) = self.rows.iter().position(|row| row.id == execution.id) {
            if matches {
                self.rows[position] = execution.clone();
            } else {
                self.rows.remove(position);
            }
        } else if matches && self.page == 0 {
            self.rows.insert(0, execution.clone());
            if self.rows.len() > HISTORY_PAGE_SIZE as usize {
                self.rows.truncate(HISTORY_PAGE_SIZE as usize);
                self.has_next = true;
            }
        }
    }
}

/// Which kinds of line the log view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFilter {
    pub stdout: bool,
    pub stderr: bool,
    /// ScriptRunner's own lines (the "started" and "finished" banners).
    pub meta: bool,
}

impl Default for StreamFilter {
    fn default() -> Self {
        Self {
            stdout: true,
            stderr: true,
            meta: true,
        }
    }
}

impl StreamFilter {
    pub fn shows(self, kind: LogKind) -> bool {
        match kind {
            LogKind::Stdout => self.stdout,
            LogKind::Stderr => self.stderr,
            LogKind::Meta => self.meta,
        }
    }
}

/// One execution open on screen, with its log as far as it has been read.
#[derive(Debug)]
pub struct LogView {
    pub id: ExecutionId,
    /// The execution's header (status, exit code, duration). `None` until loaded.
    pub execution: Option<Execution>,
    /// The daemon has no such execution.
    pub missing: bool,
    pub entries: Vec<LogEntry>,
    /// Lines dropped from the front of `entries` because of [`MAX_LOG_ENTRIES`].
    pub dropped: usize,
    /// Where the next read of the file starts; also where to resume following.
    pub next_offset: u64,
    /// The execution is over and its whole log has been read.
    pub finished: bool,
    /// Following (or loading) failed. Shown with a retry.
    pub failure: Option<Failure>,
    pub streams: StreamFilter,
    /// Keep the view scrolled to the newest line.
    pub stick_to_bottom: bool,
}

impl LogView {
    fn new(id: ExecutionId) -> Self {
        Self {
            id,
            execution: None,
            missing: false,
            entries: Vec::new(),
            dropped: 0,
            next_offset: 0,
            finished: false,
            failure: None,
            streams: StreamFilter::default(),
            stick_to_bottom: true,
        }
    }

    /// Adds a chunk. One that does not reach past what has been read is a repeat (two followers
    /// overlapped around a reconnection) and is ignored, so no line is ever shown twice.
    fn receive(&mut self, chunk: LogChunk) {
        if chunk.next_offset <= self.next_offset {
            return;
        }
        self.next_offset = chunk.next_offset;
        self.entries.extend(chunk.entries);
        if self.entries.len() > MAX_LOG_ENTRIES {
            let excess = self.entries.len() - MAX_LOG_ENTRIES;
            self.entries.drain(..excess);
            self.dropped += excess;
        }
    }

    /// Still being followed: neither over nor failed.
    pub fn is_live(&self) -> bool {
        !self.finished && self.failure.is_none() && !self.missing
    }

    /// The lines the stream filter lets through.
    pub fn visible(&self) -> impl Iterator<Item = &LogEntry> {
        self.entries.iter().filter(|entry| self.streams.shows(entry.kind))
    }

    /// The visible lines as text, for "copy".
    pub fn visible_text(&self) -> String {
        self.visible().map(format_entry).collect()
    }
}

#[cfg(test)]
mod tests {
    use scriptrunner_ipc::PROTOCOL_VERSION;

    use super::*;
    use crate::backend::Reason;

    fn now() -> Instant {
        Instant::now()
    }

    fn connected() -> Update {
        Update::Connection(ConnectionState::Connected {
            version: "0.1.0".into(),
            protocol: PROTOCOL_VERSION,
        })
    }

    fn down() -> Update {
        Update::Connection(ConnectionState::Down {
            reason: Reason::Lost,
            retry_in: std::time::Duration::from_secs(1),
        })
    }

    fn overview(name: &str, running: bool) -> TaskOverview {
        TaskOverview {
            task: Task::new(name, "a.sh"),
            running,
            last_execution: None,
            next_run: None,
        }
    }

    fn execution(task: &Task, status: ExecutionStatus) -> Execution {
        let mut execution = Execution::new(task, scriptrunner_core::Trigger::Manual, None);
        execution.status = status;
        execution
    }

    fn entry(kind: LogKind, text: &str) -> LogEntry {
        LogEntry {
            at: Utc::now(),
            kind,
            text: text.into(),
        }
    }

    fn chunk(next_offset: u64, entries: Vec<LogEntry>) -> LogChunk {
        LogChunk {
            entries,
            next_offset,
            eof: true,
        }
    }

    fn open(state: &mut AppState, id: &ExecutionId) {
        state.go(Screen::Execution(id.clone()));
    }

    // ---- tasks ----

    #[test]
    fn the_overview_replaces_the_list_and_the_search_filters_it() {
        let mut state = AppState::default();
        assert!(!state.tasks_loaded);
        state.apply(
            Update::Overview(vec![overview("Backup noturno", false), overview("Limpeza", true)]),
            now(),
        );
        assert!(state.tasks_loaded);
        assert_eq!(state.visible_tasks().len(), 2);
        assert_eq!(state.running_count(), 1);

        state.search = "  BACKUP ".into();
        let names: Vec<_> = state.visible_tasks().iter().map(|row| row.task.name.as_str()).collect();
        assert_eq!(names, ["Backup noturno"]);

        // A second overview replaces the first; it is never merged.
        state.apply(Update::Overview(vec![overview("Outra", false)]), now());
        assert_eq!(state.tasks.len(), 1);
        assert!(state.visible_tasks().is_empty());
    }

    #[test]
    fn the_uptime_keeps_counting_between_reports() {
        let mut state = AppState::default();
        let heard = now();
        state.apply(
            Update::Status(StatusInfo {
                version: "0.1.0".into(),
                protocol_version: 2,
                uptime_secs: 100,
            }),
            heard,
        );
        let status = state.status.as_ref().unwrap();
        assert_eq!(status.uptime_secs(heard), 100);
        assert_eq!(status.uptime_secs(heard + std::time::Duration::from_secs(7)), 107);
        // Not connected: the uptime of a daemon we cannot reach is not shown.
        state.apply(down(), now());
        assert!(state.status.is_none());
    }

    // ---- navigation ----

    #[test]
    fn opening_the_history_queries_it_and_leaving_stops_refetching_it() {
        let mut state = AppState::default();
        let commands = state.go(Screen::History);
        assert!(state.history.active);
        assert!(matches!(commands.as_slice(), [Command::QueryHistory { .. }]));

        state.go(Screen::Dashboard);
        assert!(!state.history.active);
        // Nothing to refetch for a screen that is not open.
        assert!(state.apply(Update::Resynced, now()).is_empty());
    }

    #[test]
    fn opening_an_execution_loads_it_and_follows_its_log_and_leaving_stops() {
        let mut state = AppState::default();
        let id = ExecutionId::new();
        let commands = state.go(Screen::Execution(id.clone()));
        assert_eq!(
            commands,
            [
                Command::GetExecution(id.clone()),
                Command::FollowLog {
                    id: id.clone(),
                    offset: 0
                }
            ]
        );
        assert!(state.log.is_some());

        let commands = state.go(Screen::Dashboard);
        assert_eq!(commands, [Command::StopFollowing]);
        assert!(state.log.is_none());
        // Going where we already are asks for nothing.
        assert!(state.go(Screen::Dashboard).is_empty());
    }

    // ---- reconnection and resync ----

    #[test]
    fn reconnecting_refetches_what_is_open_and_resumes_the_log_where_it_stopped() {
        let mut state = AppState::default();
        state.apply(connected(), now());
        let id = ExecutionId::new();
        open(&mut state, &id);
        state.apply(
            Update::Log(LogUpdate::Chunk {
                id: id.clone(),
                chunk: chunk(120, vec![entry(LogKind::Stdout, "a")]),
            }),
            now(),
        );

        state.apply(down(), now());
        let commands = state.apply(connected(), now());
        assert_eq!(
            commands,
            [
                Command::GetExecution(id.clone()),
                Command::FollowLog { id, offset: 120 }
            ]
        );
    }

    #[test]
    fn a_finished_log_is_not_followed_again_after_reconnecting() {
        let mut state = AppState::default();
        let id = ExecutionId::new();
        open(&mut state, &id);
        state.apply(Update::Log(LogUpdate::Finished { id: id.clone() }), now());
        state.apply(connected(), now());
        state.apply(down(), now());
        let commands = state.apply(connected(), now());
        assert_eq!(commands, [Command::GetExecution(id)]);
    }

    #[test]
    fn a_resync_refetches_the_history_and_the_execution_but_leaves_the_log_follower_alone() {
        let mut state = AppState::default();
        state.go(Screen::History);
        let commands = state.apply(Update::Resynced, now());
        assert!(matches!(commands.as_slice(), [Command::QueryHistory { .. }]));

        let id = ExecutionId::new();
        state.go(Screen::Dashboard);
        open(&mut state, &id);
        let commands = state.apply(Update::Resynced, now());
        assert_eq!(commands, [Command::GetExecution(id)]);
    }

    // ---- history ----

    fn query_of(command: &Command) -> (u64, &ExecutionQuery) {
        match command {
            Command::QueryHistory { tag, query } => (*tag, query),
            other => panic!("expected a history query, got {other:?}"),
        }
    }

    fn rows(task: &Task, count: usize) -> Vec<Execution> {
        (0..count).map(|_| execution(task, ExecutionStatus::Success)).collect()
    }

    #[test]
    fn history_pages_ask_for_one_extra_row_to_know_whether_there_is_a_next_page() {
        let task = Task::new("t", "a.sh");
        let mut history = HistoryState::default();
        let first = history.request();
        let (tag, query) = query_of(&first);
        assert_eq!(query.limit, Some(HISTORY_PAGE_SIZE + 1));
        assert_eq!(query.offset, 0);

        // 51 rows came back: show 50 and offer the next page.
        history.receive(tag, rows(&task, 51));
        assert_eq!(history.rows.len(), 50);
        assert!(history.has_next && !history.loading);

        let second = history.next_page().unwrap();
        assert_eq!(query_of(&second).1.offset, HISTORY_PAGE_SIZE);
        history.receive(query_of(&second).0, rows(&task, 3));
        assert!(!history.has_next);
        assert!(history.next_page().is_none());

        let back = history.previous_page().unwrap();
        assert_eq!(query_of(&back).1.offset, 0);
        history.receive(query_of(&back).0, rows(&task, 1));
        assert!(history.previous_page().is_none());
    }

    #[test]
    fn an_answer_to_an_older_query_is_ignored() {
        let task = Task::new("t", "a.sh");
        let mut history = HistoryState::default();
        let old = history.request();
        let new = history.set_filter(HistoryFilter {
            status: Some(ExecutionStatus::Failed),
            ..HistoryFilter::default()
        });
        assert_eq!(query_of(&new).1.status, Some(ExecutionStatus::Failed));

        history.receive(query_of(&old).0, rows(&task, 2));
        assert!(
            history.rows.is_empty() && history.loading,
            "the stale answer changed nothing"
        );
        history.receive(query_of(&new).0, rows(&task, 1));
        assert_eq!(history.rows.len(), 1);
        assert!(!history.loading);
    }

    #[test]
    fn a_new_filter_goes_back_to_the_first_page() {
        let mut history = HistoryState {
            page: 3,
            ..HistoryState::default()
        };
        let command = history.set_filter(HistoryFilter::default());
        assert_eq!(history.page, 0);
        assert_eq!(query_of(&command).1.offset, 0);
    }

    #[test]
    fn a_page_that_emptied_falls_back_to_the_previous_one() {
        let mut history = HistoryState {
            page: 2,
            ..HistoryState::default()
        };
        let command = history.request();
        let follow_up = history.receive(query_of(&command).0, Vec::new()).unwrap();
        assert_eq!(history.page, 1);
        assert_eq!(query_of(&follow_up).1.offset, HISTORY_PAGE_SIZE);
    }

    #[test]
    fn executions_that_change_keep_the_visible_page_current() {
        let task = Task::new("t", "a.sh");
        let mut state = AppState::default();
        let commands = state.go(Screen::History);
        let (tag, _) = query_of(&commands[0]);
        let running = execution(&task, ExecutionStatus::Running);
        state.apply(
            Update::History {
                tag,
                rows: vec![running.clone()],
            },
            now(),
        );

        // The same execution finishing updates its row in place.
        let mut done = running.clone();
        done.status = ExecutionStatus::Success;
        state.apply(Update::ExecutionChanged(done), now());
        assert_eq!(state.history.rows.len(), 1);
        assert_eq!(state.history.rows[0].status, ExecutionStatus::Success);

        // A new execution shows up on top.
        let newer = execution(&task, ExecutionStatus::Queued);
        state.apply(Update::ExecutionChanged(newer.clone()), now());
        assert_eq!(state.history.rows[0].id, newer.id);

        // With a status filter, a row that stops matching leaves, and one that does not match never enters.
        state.history.filter.status = Some(ExecutionStatus::Success);
        let mut failed = newer.clone();
        failed.status = ExecutionStatus::Failed;
        state.apply(Update::ExecutionChanged(failed), now());
        assert!(state.history.rows.iter().all(|row| row.id != newer.id));
        let rows_before = state.history.rows.len();
        state.apply(
            Update::ExecutionChanged(execution(&task, ExecutionStatus::Failed)),
            now(),
        );
        assert_eq!(state.history.rows.len(), rows_before);
    }

    #[test]
    fn the_filter_matches_by_task_status_and_period() {
        let task = Task::new("t", "a.sh");
        let other = Task::new("o", "b.sh");
        let run = execution(&task, ExecutionStatus::Failed);
        let by = |filter: HistoryFilter| filter.matches(&run);
        assert!(by(HistoryFilter::default()));
        assert!(by(HistoryFilter {
            task: Some(task.id.clone()),
            ..HistoryFilter::default()
        }));
        assert!(!by(HistoryFilter {
            task: Some(other.id),
            ..HistoryFilter::default()
        }));
        assert!(!by(HistoryFilter {
            status: Some(ExecutionStatus::Success),
            ..HistoryFilter::default()
        }));
        assert!(!by(HistoryFilter {
            since: Some(run.created_at + chrono::Duration::seconds(1)),
            ..HistoryFilter::default()
        }));
        assert!(!by(HistoryFilter {
            until: Some(run.created_at - chrono::Duration::seconds(1)),
            ..HistoryFilter::default()
        }));
    }

    #[test]
    fn clearing_the_history_requeries_it_only_while_it_is_open() {
        let mut state = AppState::default();
        assert!(state.apply(Update::Succeeded(Action::ClearHistory), now()).is_empty());
        state.go(Screen::History);
        let commands = state.apply(Update::Succeeded(Action::ClearHistory), now());
        assert!(matches!(commands.as_slice(), [Command::QueryHistory { .. }]));
    }

    // ---- log ----

    #[test]
    fn chunks_for_the_open_log_are_appended_and_others_are_dropped() {
        let mut state = AppState::default();
        let id = ExecutionId::new();
        open(&mut state, &id);
        state.apply(
            Update::Log(LogUpdate::Chunk {
                id: id.clone(),
                chunk: chunk(10, vec![entry(LogKind::Stdout, "one")]),
            }),
            now(),
        );
        state.apply(
            Update::Log(LogUpdate::Chunk {
                id: ExecutionId::new(),
                chunk: chunk(99, vec![entry(LogKind::Stdout, "intruder")]),
            }),
            now(),
        );
        state.apply(
            Update::Log(LogUpdate::Chunk {
                id,
                chunk: chunk(25, vec![entry(LogKind::Stderr, "two")]),
            }),
            now(),
        );
        let log = state.log.as_ref().unwrap();
        let texts: Vec<_> = log.entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["one", "two"]);
        assert_eq!(log.next_offset, 25);
    }

    #[test]
    fn a_repeated_chunk_never_duplicates_lines() {
        let mut state = AppState::default();
        let id = ExecutionId::new();
        open(&mut state, &id);
        let update = |next, text| {
            Update::Log(LogUpdate::Chunk {
                id: id.clone(),
                chunk: chunk(next, vec![entry(LogKind::Stdout, text)]),
            })
        };
        state.apply(update(10, "a"), now());
        // Two followers overlapped around a reconnection and both delivered the same chunk.
        state.apply(update(10, "a"), now());
        state.apply(update(20, "b"), now());
        assert_eq!(state.log.as_ref().unwrap().entries.len(), 2);
    }

    #[test]
    fn a_very_long_log_keeps_only_the_newest_lines_and_counts_what_it_dropped() {
        let mut log = LogView::new(ExecutionId::new());
        let many = (0..MAX_LOG_ENTRIES + 5)
            .map(|n| entry(LogKind::Stdout, &n.to_string()))
            .collect();
        log.receive(chunk(1, many));
        assert_eq!(log.entries.len(), MAX_LOG_ENTRIES);
        assert_eq!(log.dropped, 5);
        assert_eq!(log.entries[0].text, "5");
    }

    #[test]
    fn the_stream_filter_hides_lines_and_the_copy_follows_it() {
        let mut log = LogView::new(ExecutionId::new());
        log.receive(chunk(
            1,
            vec![
                entry(LogKind::Meta, "started"),
                entry(LogKind::Stdout, "out"),
                entry(LogKind::Stderr, "err"),
            ],
        ));
        assert_eq!(log.visible().count(), 3);
        log.streams.stderr = false;
        log.streams.meta = false;
        let texts: Vec<_> = log.visible().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["out"]);
        assert!(log.visible_text().contains("[OUT] out") && !log.visible_text().contains("err"));
    }

    #[test]
    fn a_log_that_failed_can_be_retried_from_where_it_stopped() {
        let mut state = AppState::default();
        let id = ExecutionId::new();
        open(&mut state, &id);
        state.apply(
            Update::Log(LogUpdate::Chunk {
                id: id.clone(),
                chunk: chunk(77, vec![entry(LogKind::Stdout, "x")]),
            }),
            now(),
        );
        state.apply(
            Update::Log(LogUpdate::Failed {
                id: id.clone(),
                failure: Failure::NotConnected,
            }),
            now(),
        );
        assert!(!state.log.as_ref().unwrap().is_live());

        let commands = state.retry_log();
        assert_eq!(
            commands,
            [Command::GetExecution(id.clone()), Command::FollowLog { id, offset: 77 }]
        );
        assert!(state.log.as_ref().unwrap().is_live());
    }

    #[test]
    fn the_open_execution_header_follows_its_events() {
        let task = Task::new("t", "a.sh");
        let mut state = AppState::default();
        let running = execution(&task, ExecutionStatus::Running);
        open(&mut state, &running.id);
        state.apply(
            Update::ExecutionLoaded {
                id: running.id.clone(),
                execution: Some(running.clone()),
            },
            now(),
        );
        let mut done = running.clone();
        done.status = ExecutionStatus::Failed;
        state.apply(Update::ExecutionChanged(done), now());
        assert_eq!(
            state.log.as_ref().unwrap().execution.as_ref().unwrap().status,
            ExecutionStatus::Failed
        );

        // An execution the daemon does not know is flagged, not left loading forever.
        let ghost = ExecutionId::new();
        state.go(Screen::Dashboard);
        open(&mut state, &ghost);
        state.apply(
            Update::ExecutionLoaded {
                id: ghost,
                execution: None,
            },
            now(),
        );
        assert!(state.log.as_ref().unwrap().missing);
        assert!(!state.log.as_ref().unwrap().is_live());
    }

    // ---- notices and failures ----

    #[test]
    fn results_become_numbered_notices_and_old_ones_fall_off() {
        let mut state = AppState::default();
        state.apply(Update::Succeeded(Action::DeleteTask), now());
        state.apply(
            Update::Failed {
                action: Action::RunNow,
                failure: Failure::NotConnected,
            },
            now(),
        );
        let all: Vec<_> = state.notices_since(0).collect();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id + 1, all[1].id);
        assert!(matches!(
            all[1].kind,
            NoticeKind::Error {
                action: Action::RunNow,
                ..
            }
        ));
        let first = all[0].id;
        assert_eq!(state.notices_since(first).count(), 1);

        for _ in 0..MAX_NOTICES + 10 {
            state.apply(Update::Succeeded(Action::Stop), now());
        }
        assert_eq!(state.notices_since(0).count(), MAX_NOTICES);
    }

    #[test]
    fn failures_that_belong_to_a_screen_show_there_instead_of_as_notices() {
        let mut state = AppState::default();
        state.go(Screen::History);
        state.apply(
            Update::Failed {
                action: Action::QueryHistory,
                failure: Failure::NotConnected,
            },
            now(),
        );
        assert_eq!(state.history.failure, Some(Failure::NotConnected));
        assert!(!state.history.loading);

        let id = ExecutionId::new();
        state.go(Screen::Dashboard);
        open(&mut state, &id);
        state.apply(
            Update::Failed {
                action: Action::FollowLog,
                failure: Failure::Other("x".into()),
            },
            now(),
        );
        assert_eq!(state.log.as_ref().unwrap().failure, Some(Failure::Other("x".into())));
        assert_eq!(state.notices_since(0).count(), 0);
    }

    #[test]
    fn a_saved_task_and_a_started_run_are_noticed_with_what_they_are_about() {
        let mut state = AppState::default();
        let task = Task::new("Backup", "a.sh");
        state.apply(Update::TaskSaved(task.clone()), now());
        let execution = ExecutionId::new();
        state.apply(
            Update::Started {
                task: task.id.clone(),
                execution: execution.clone(),
            },
            now(),
        );
        let kinds: Vec<_> = state.notices_since(0).map(|n| n.kind.clone()).collect();
        assert_eq!(kinds[0], NoticeKind::TaskSaved(Box::new(task.clone())));
        assert_eq!(
            kinds[1],
            NoticeKind::Started {
                task: task.id,
                execution
            }
        );
    }

    #[test]
    fn settings_come_from_the_daemon_as_it_has_them() {
        let mut state = AppState::default();
        assert!(state.settings.is_none());
        let settings = Settings {
            max_concurrent_executions: 9,
            ..Settings::default()
        };
        state.apply(Update::Settings(settings.clone()), now());
        assert_eq!(state.settings, Some(settings));
    }
}
