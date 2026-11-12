//! The bridge between the synchronous UI and the daemon (plan D16, D17, P3).
//!
//! egui draws on the main thread and must never wait for the network. So the daemon is spoken to from
//! a thread of its own that runs a tokio runtime:
//!
//! ```text
//!   UI thread                         backend thread (tokio)
//!   ---------                         ----------------------
//!   Backend::send(Command)  ───────►  commands: run, stop, save, query, follow a log...
//!   Backend::try_recv()     ◄───────  Update: connection state, overview, events, results
//!                           repaint()  called after every Update so the UI wakes up
//! ```
//!
//! * **State is the daemon's.** After `Subscribe`, every `TasksChanged` / `ExecutionChanged` makes the
//!   backend refetch the overview (coalesced, so a burst of events costs one call). `Resync`, or this
//!   client falling behind, refetches everything the same way and tells the UI with
//!   [`Update::Resynced`] so it can refetch what it keeps beyond the overview (a history page, the
//!   open execution).
//! * **Reconnection.** If the daemon is not there, or goes away, the backend reports
//!   [`ConnectionState::Down`] with the reason and tries again with growing waits (1 s up to 10 s),
//!   forever. Commands issued meanwhile fail at once with [`Failure::NotConnected`] instead of
//!   queueing for a daemon that may be gone for hours.
//! * **Two connections** (P3). The daemon answers the requests of one connection in order, so a big
//!   log export would delay a "Stop" queued behind it. Commands and events use one connection; log
//!   reading (following a run, exporting) uses a second, opened on first use.

use std::future::{Future, pending};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use scriptrunner_core::logs::LogChunk;
use scriptrunner_core::{Execution, ExecutionId, ExecutionQuery, Settings, Task, TaskId, TaskOverview};
use scriptrunner_ipc::client::{Client, ClientError, LOG_CHUNK_BYTES};
use scriptrunner_ipc::transport::{Endpoint, TransportError};
use scriptrunner_ipc::{ErrorCode, Event, StatusInfo};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep, sleep_until, timeout};

/// Timings. The defaults are for real use; tests shrink them.
#[derive(Debug, Clone)]
pub struct BackendConfig {
    /// Wait before the first reconnection attempt; doubles after each failure.
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// A call the daemon has not answered by then counts as failed (and, for the calls that keep the
    /// session going, the connection as lost).
    pub call_timeout: Duration,
    /// Events arriving within this window cost one overview refetch.
    pub refresh_delay: Duration,
    /// How often the daemon's uptime is refreshed.
    pub status_interval: Duration,
    /// Pause between reads of a log being followed, once it has caught up.
    pub log_poll: Duration,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(10),
            call_timeout: Duration::from_secs(20),
            refresh_delay: Duration::from_millis(150),
            status_interval: Duration::from_secs(30),
            log_poll: Duration::from_millis(500),
        }
    }
}

/// Why there is no connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// Nothing is listening: the service is stopped.
    NotRunning,
    /// The pipe is there but this process may not use it (not elevated).
    AccessDenied,
    /// The handshake failed: the daemon speaks another protocol, or something else answered.
    Protocol(String),
    /// The connection was up and dropped.
    Lost,
    Other(String),
}

impl Reason {
    fn of(error: &ClientError) -> Self {
        match error {
            ClientError::NotRunning(_) => Self::NotRunning,
            ClientError::Transport(TransportError::AccessDenied(_)) => Self::AccessDenied,
            ClientError::Disconnected | ClientError::Codec(_) => Self::Lost,
            other => Self::Other(other.to_string()),
        }
    }

    /// For a failure during the handshake, where an unexpected answer means "not our protocol".
    fn of_handshake(error: &ClientError) -> Self {
        match error {
            ClientError::Remote(body) if body.code == ErrorCode::ProtocolMismatch => {
                Self::Protocol(body.message.clone())
            }
            ClientError::Unexpected(message) => Self::Protocol(message.clone()),
            other => Self::of(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected {
        /// The daemon's crate version.
        version: String,
        /// What the two sides agreed to speak.
        protocol: u32,
    },
    /// No connection; another attempt comes after `retry_in`.
    Down {
        reason: Reason,
        retry_in: Duration,
    },
}

/// Why a command did not work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// There is no connection to the daemon right now (or it dropped mid-call).
    NotConnected,
    /// The daemon refused or could not do it. The message is the daemon's own, in English.
    Remote {
        code: ErrorCode,
        message: String,
    },
    Other(String),
}

impl From<ClientError> for Failure {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Remote(body) => Self::Remote {
                code: body.code,
                message: body.message,
            },
            ClientError::NotRunning(_) | ClientError::Disconnected | ClientError::Transport(_) => Self::NotConnected,
            other => Self::Other(other.to_string()),
        }
    }
}

/// What a command was for, so a failure can say what did not work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Refresh,
    RunNow,
    Stop,
    SetScheduleEnabled,
    CreateTask,
    UpdateTask,
    DeleteTask,
    DuplicateTask,
    QueryHistory,
    ClearHistory,
    GetExecution,
    GetSettings,
    UpdateSettings,
    FollowLog,
    ExportLog,
}

/// What the UI asks the backend to do. Nothing here waits: the answer arrives as an [`Update`].
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Refetch the overview now (the user pressed "refresh").
    Refresh,
    RunNow(TaskId),
    /// Cancels the running and queued executions of the task.
    Stop(TaskId),
    SetScheduleEnabled {
        id: TaskId,
        enabled: bool,
    },
    CreateTask(Task),
    UpdateTask(Task),
    DeleteTask(TaskId),
    DuplicateTask(TaskId),
    /// `tag` comes back in [`Update::History`], so the UI can tell which of its queries was answered.
    QueryHistory {
        tag: u64,
        query: ExecutionQuery,
    },
    ClearHistory(TaskId),
    GetExecution(ExecutionId),
    GetSettings,
    UpdateSettings(Settings),
    /// Streams the log of an execution from byte `offset` (0 for the start, or the `next_offset` of the
    /// last chunk seen) until the execution is over. Replaces any log already being followed.
    FollowLog {
        id: ExecutionId,
        offset: u64,
    },
    StopFollowing,
    /// Writes the whole log, as text, to `path`.
    ExportLog {
        id: ExecutionId,
        path: PathBuf,
    },
}

impl Command {
    pub fn action(&self) -> Action {
        match self {
            Self::Refresh => Action::Refresh,
            Self::RunNow(_) => Action::RunNow,
            Self::Stop(_) => Action::Stop,
            Self::SetScheduleEnabled { .. } => Action::SetScheduleEnabled,
            Self::CreateTask(_) => Action::CreateTask,
            Self::UpdateTask(_) => Action::UpdateTask,
            Self::DeleteTask(_) => Action::DeleteTask,
            Self::DuplicateTask(_) => Action::DuplicateTask,
            Self::QueryHistory { .. } => Action::QueryHistory,
            Self::ClearHistory(_) => Action::ClearHistory,
            Self::GetExecution(_) => Action::GetExecution,
            Self::GetSettings => Action::GetSettings,
            Self::UpdateSettings(_) => Action::UpdateSettings,
            Self::FollowLog { .. } | Self::StopFollowing => Action::FollowLog,
            Self::ExportLog { .. } => Action::ExportLog,
        }
    }
}

/// Progress of a followed log.
#[derive(Debug, Clone, PartialEq)]
pub enum LogUpdate {
    /// New lines. `chunk.next_offset` is where to resume from after a reconnection.
    Chunk { id: ExecutionId, chunk: LogChunk },
    /// The execution is over and everything it wrote has been delivered.
    Finished { id: ExecutionId },
    /// Following stopped early. Send `FollowLog` again (from the last `next_offset`) to resume.
    Failed { id: ExecutionId, failure: Failure },
}

/// What the backend tells the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    Connection(ConnectionState),
    /// The daemon's own account of every task. Replaces whatever the UI had.
    Overview(Vec<TaskOverview>),
    /// Version and uptime of the daemon, refreshed now and then.
    Status(StatusInfo),
    /// The settings as the daemon has them (after a load or a successful save).
    Settings(Settings),
    /// An execution changed state (queued, started, finished...).
    ExecutionChanged(Execution),
    /// Events were lost: refetch everything shown beyond the overview.
    Resynced,
    History {
        tag: u64,
        rows: Vec<Execution>,
    },
    ExecutionLoaded {
        id: ExecutionId,
        execution: Option<Execution>,
    },
    Started {
        task: TaskId,
        execution: ExecutionId,
    },
    Stopped {
        task: TaskId,
        /// How many running or queued executions were cancelled.
        count: u32,
    },
    /// A task was created, edited or duplicated; this is how the daemon now has it.
    TaskSaved(Task),
    Log(LogUpdate),
    Exported {
        id: ExecutionId,
        path: PathBuf,
        bytes: u64,
    },
    /// A command that has nothing more to report worked.
    Succeeded(Action),
    Failed {
        action: Action,
        failure: Failure,
    },
}

/// The UI's end of the bridge. Dropping it stops the backend thread.
pub struct Backend {
    commands: mpsc::UnboundedSender<Command>,
    updates: std_mpsc::Receiver<Update>,
}

impl Backend {
    /// Starts the backend thread and begins connecting to the daemon at `endpoint`.
    ///
    /// `repaint` is called (from the backend thread) after every [`Update`], to wake the UI; pass
    /// `move || ctx.request_repaint()`.
    pub fn spawn(
        endpoint: Endpoint,
        config: BackendConfig,
        repaint: impl Fn() + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("scriptrunner-io")
            .enable_all()
            .build()?;
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (update_tx, updates) = std_mpsc::channel();
        let sink = Updates {
            tx: update_tx,
            repaint: Arc::new(repaint),
        };
        std::thread::Builder::new()
            .name("scriptrunner-backend".into())
            .spawn(move || {
                runtime.block_on(supervise(endpoint, config, command_rx, sink));
                // The UI is gone; do not wait for calls still in flight.
                runtime.shutdown_background();
            })?;
        Ok(Self { commands, updates })
    }

    /// Queues a command. Never blocks.
    pub fn send(&self, command: Command) {
        // Fails only when the backend thread is gone, which means the process is shutting down.
        let _ = self.commands.send(command);
    }

    /// The next update, if one is waiting. Never blocks.
    pub fn try_recv(&self) -> Option<Update> {
        self.updates.try_recv().ok()
    }

    /// Everything waiting right now.
    pub fn drain(&self) -> Vec<Update> {
        std::iter::from_fn(|| self.try_recv()).collect()
    }

    /// Waits up to `wait` for the next update. For tests and tools without a UI loop: the UI itself
    /// must not block.
    pub fn recv_timeout(&self, wait: Duration) -> Option<Update> {
        self.updates.recv_timeout(wait).ok()
    }
}

#[derive(Clone)]
struct Updates {
    tx: std_mpsc::Sender<Update>,
    repaint: Arc<dyn Fn() + Send + Sync>,
}

impl Updates {
    fn send(&self, update: Update) {
        if self.tx.send(update).is_ok() {
            (self.repaint)();
        }
    }
}

/// What every part of the backend needs.
struct Context {
    endpoint: Endpoint,
    config: BackendConfig,
    updates: Updates,
}

enum SessionEnd {
    /// The UI is gone.
    Shutdown,
    Failed {
        reason: Reason,
        /// The connection was up before it failed (so the next wait starts from the shortest).
        was_connected: bool,
    },
}

/// Connects, serves until the connection drops, waits, and does it again, until the UI goes away.
async fn supervise(
    endpoint: Endpoint,
    config: BackendConfig,
    mut commands: mpsc::UnboundedReceiver<Command>,
    updates: Updates,
) {
    let ctx = Context {
        endpoint,
        config,
        updates,
    };
    let mut backoff = ctx.config.initial_backoff;
    loop {
        ctx.updates.send(Update::Connection(ConnectionState::Connecting));
        match session(&ctx, &mut commands).await {
            SessionEnd::Shutdown => return,
            SessionEnd::Failed { reason, was_connected } => {
                if was_connected {
                    backoff = ctx.config.initial_backoff;
                }
                ctx.updates.send(Update::Connection(ConnectionState::Down {
                    reason,
                    retry_in: backoff,
                }));
                if !wait_rejecting_commands(&ctx, &mut commands, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(ctx.config.max_backoff);
            }
        }
    }
}

/// Sleeps for `delay`, answering every command with "not connected". `false` if the UI went away.
async fn wait_rejecting_commands(
    ctx: &Context,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    delay: Duration,
) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        tokio::select! {
            _ = sleep_until(deadline) => return true,
            command = commands.recv() => match command {
                None => return false,
                Some(command) => ctx.updates.send(Update::Failed {
                    action: command.action(),
                    failure: Failure::NotConnected,
                }),
            },
        }
    }
}

/// Aborts the followed log when it goes out of scope, so no task outlives its session.
#[derive(Default)]
struct Follower(Option<JoinHandle<()>>);

impl Follower {
    fn replace(&mut self, task: Option<JoinHandle<()>>) {
        if let Some(old) = std::mem::replace(&mut self.0, task) {
            old.abort();
        }
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.replace(None);
    }
}

async fn session(ctx: &Context, commands: &mut mpsc::UnboundedReceiver<Command>) -> SessionEnd {
    let mut was_connected = false;
    match serve_session(ctx, commands, &mut was_connected).await {
        Ok(()) => SessionEnd::Shutdown,
        Err(reason) => SessionEnd::Failed { reason, was_connected },
    }
}

/// One connection's lifetime. `Ok` only when the UI goes away; every other way out is an `Err`.
async fn serve_session(
    ctx: &Context,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    was_connected: &mut bool,
) -> Result<(), Reason> {
    let limit = ctx.config.call_timeout;
    let client = bounded(limit, Client::connect(&ctx.endpoint))
        .await
        .map_err(|e| Reason::of_handshake(&e))?;
    // Subscribe before the first fetch, so nothing that happens in between is missed.
    let mut events = bounded(limit, client.subscribe()).await.map_err(|e| Reason::of(&e))?;
    let status = bounded(limit, client.status()).await.map_err(|e| Reason::of(&e))?;
    *was_connected = true;
    ctx.updates.send(Update::Connection(ConnectionState::Connected {
        version: status.version.clone(),
        protocol: client.protocol_version(),
    }));
    ctx.updates.send(Update::Status(status));
    refresh_overview(ctx, &client).await?;
    // Settings are small and every screen's validation may want them; not worth a separate command.
    let settings = bounded(limit, client.settings()).await.map_err(|e| Reason::of(&e))?;
    ctx.updates.send(Update::Settings(settings));

    let logs = Arc::new(LogConnection {
        endpoint: ctx.endpoint.clone(),
        limit,
        slot: Mutex::new(None),
    });
    let mut follower = Follower::default();
    let mut refresh_at: Option<Instant> = None;
    let mut status_tick = interval_at(Instant::now() + ctx.config.status_interval, ctx.config.status_interval);
    status_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        let deadline = refresh_at;
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { return Ok(()) };
                match command {
                    Command::Refresh => {
                        refresh_at = None;
                        refresh_overview(ctx, &client).await?;
                        let status = bounded(limit, client.status()).await.map_err(|e| Reason::of(&e))?;
                        ctx.updates.send(Update::Status(status));
                    }
                    Command::FollowLog { id, offset } => {
                        let task = tokio::spawn(follow_log(
                            Arc::clone(&logs),
                            ctx.updates.clone(),
                            id,
                            offset,
                            limit,
                            ctx.config.log_poll,
                        ));
                        follower.replace(Some(task));
                    }
                    Command::StopFollowing => follower.replace(None),
                    other => {
                        let client = client.clone();
                        let logs = Arc::clone(&logs);
                        let updates = ctx.updates.clone();
                        tokio::spawn(async move {
                            let action = other.action();
                            match run_command(other, &client, &logs, limit).await {
                                Ok(done) => done.into_iter().for_each(|update| updates.send(update)),
                                Err(failure) => updates.send(Update::Failed { action, failure }),
                            }
                        });
                    }
                }
            }
            event = events.recv() => match event {
                Ok(Event::TasksChanged) => schedule_refresh(&mut refresh_at, ctx.config.refresh_delay),
                Ok(Event::ExecutionChanged(execution)) => {
                    ctx.updates.send(Update::ExecutionChanged(execution));
                    schedule_refresh(&mut refresh_at, ctx.config.refresh_delay);
                }
                // Events were lost, on the daemon's side or on ours: nothing can be trusted but a refetch.
                Ok(Event::Resync) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    ctx.updates.send(Update::Resynced);
                    refresh_at = Some(Instant::now());
                }
                Ok(Event::Unknown) => {}
                Err(broadcast::error::RecvError::Closed) => return Err(Reason::Lost),
            },
            _ = async move {
                match deadline {
                    Some(at) => sleep_until(at).await,
                    None => pending::<()>().await,
                }
            } => {
                refresh_at = None;
                refresh_overview(ctx, &client).await?;
            }
            _ = status_tick.tick() => {
                let status = bounded(limit, client.status()).await.map_err(|e| Reason::of(&e))?;
                ctx.updates.send(Update::Status(status));
            }
        }
    }
}

fn schedule_refresh(refresh_at: &mut Option<Instant>, delay: Duration) {
    refresh_at.get_or_insert_with(|| Instant::now() + delay);
}

/// Done in the session loop rather than in a spawned task, so two refreshes can never overtake each
/// other and leave the UI with the older picture.
async fn refresh_overview(ctx: &Context, client: &Client) -> Result<(), Reason> {
    let rows = bounded(ctx.config.call_timeout, client.overview())
        .await
        .map_err(|e| Reason::of(&e))?;
    ctx.updates.send(Update::Overview(rows));
    Ok(())
}

/// Runs a command that does not touch the session's own state, and says what to tell the UI.
async fn run_command(
    command: Command,
    client: &Client,
    logs: &LogConnection,
    limit: Duration,
) -> Result<Vec<Update>, Failure> {
    let updates = match command {
        Command::RunNow(id) => {
            let execution = bounded(limit, client.run_now(&id)).await?;
            vec![Update::Started { task: id, execution }]
        }
        Command::Stop(id) => {
            let count = bounded(limit, client.stop(&id)).await?;
            vec![Update::Stopped { task: id, count }]
        }
        Command::SetScheduleEnabled { id, enabled } => {
            bounded(limit, client.set_schedule_enabled(&id, enabled)).await?;
            vec![Update::Succeeded(Action::SetScheduleEnabled)]
        }
        Command::CreateTask(task) => vec![Update::TaskSaved(bounded(limit, client.create_task(task)).await?)],
        Command::UpdateTask(task) => vec![Update::TaskSaved(bounded(limit, client.update_task(task)).await?)],
        Command::DuplicateTask(id) => vec![Update::TaskSaved(bounded(limit, client.duplicate_task(&id)).await?)],
        Command::DeleteTask(id) => {
            bounded(limit, client.delete_task(&id)).await?;
            vec![Update::Succeeded(Action::DeleteTask)]
        }
        Command::QueryHistory { tag, query } => {
            let rows = bounded(limit, client.executions(query)).await?;
            vec![Update::History { tag, rows }]
        }
        Command::ClearHistory(id) => {
            bounded(limit, client.clear_history(&id)).await?;
            vec![Update::Succeeded(Action::ClearHistory)]
        }
        Command::GetExecution(id) => {
            let execution = bounded(limit, client.get_execution(&id)).await?;
            vec![Update::ExecutionLoaded { id, execution }]
        }
        Command::GetSettings => vec![Update::Settings(bounded(limit, client.settings()).await?)],
        Command::UpdateSettings(settings) => {
            bounded(limit, client.update_settings(settings)).await?;
            // Show what the daemon now has, not what was sent.
            let saved = bounded(limit, client.settings()).await?;
            vec![Update::Succeeded(Action::UpdateSettings), Update::Settings(saved)]
        }
        Command::ExportLog { id, path } => {
            // No time limit: a big log takes as long as it takes. It has a connection of its own.
            let text = logs
                .with(|client| {
                    let id = id.clone();
                    async move { client.export_log(&id).await }
                })
                .await?;
            tokio::fs::write(&path, text.as_bytes())
                .await
                .map_err(|e| Failure::Other(format!("could not write {}: {e}", path.display())))?;
            vec![Update::Exported {
                id,
                path,
                bytes: text.len() as u64,
            }]
        }
        // Handled by the session loop, which owns the state they touch.
        Command::Refresh | Command::FollowLog { .. } | Command::StopFollowing => Vec::new(),
    };
    Ok(updates)
}

/// The second connection, used for reading logs. Opened on first use and replaced if it breaks.
struct LogConnection {
    endpoint: Endpoint,
    limit: Duration,
    slot: Mutex<Option<Client>>,
}

impl LogConnection {
    async fn client(&self) -> Result<Client, ClientError> {
        let mut slot = self.slot.lock().await;
        if let Some(client) = slot.as_ref() {
            return Ok(client.clone());
        }
        let client = bounded(self.limit, Client::connect(&self.endpoint)).await?;
        *slot = Some(client.clone());
        Ok(client)
    }

    /// Runs `operation` on the log connection. If the connection turns out to be broken it is replaced
    /// and the operation tried once more; every use is a read, so repeating one is harmless.
    async fn with<T, F, Fut>(&self, operation: F) -> Result<T, ClientError>
    where
        F: Fn(Client) -> Fut,
        Fut: Future<Output = Result<T, ClientError>>,
    {
        let client = self.client().await?;
        match operation(client).await {
            Err(error) if is_connection_error(&error) => {
                *self.slot.lock().await = None;
                operation(self.client().await?).await
            }
            other => other,
        }
    }
}

fn is_connection_error(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::Disconnected | ClientError::Codec(_) | ClientError::Transport(_) | ClientError::NotRunning(_)
    )
}

/// Streams a log until its execution is over (see [`Command::FollowLog`]).
async fn follow_log(
    logs: Arc<LogConnection>,
    updates: Updates,
    id: ExecutionId,
    offset: u64,
    limit: Duration,
    poll: Duration,
) {
    let outcome = follow_log_inner(&logs, &updates, &id, offset, limit, poll).await;
    updates.send(Update::Log(match outcome {
        Ok(()) => LogUpdate::Finished { id },
        Err(failure) => LogUpdate::Failed { id, failure },
    }));
}

async fn follow_log_inner(
    logs: &LogConnection,
    updates: &Updates,
    id: &ExecutionId,
    mut offset: u64,
    limit: Duration,
    poll: Duration,
) -> Result<(), Failure> {
    loop {
        // Whether the execution is over is read *before* the log: if it was already over, nothing more
        // can be written, so reading to the end then is enough. Read the other way round, a line written
        // between the two reads would be lost.
        let execution = logs
            .with(|client| {
                let id = id.clone();
                async move { bounded(limit, client.get_execution(&id)).await }
            })
            .await?;
        let Some(execution) = execution else {
            return Err(Failure::Other("the execution no longer exists".into()));
        };
        let over = execution.status.is_terminal();

        loop {
            let from = offset;
            let chunk = logs
                .with(|client| {
                    let id = id.clone();
                    async move { bounded(limit, client.read_log_from(&id, from, LOG_CHUNK_BYTES)).await }
                })
                .await?;
            let (next, caught_up) = (chunk.next_offset, chunk.eof);
            if !chunk.entries.is_empty() {
                updates.send(Update::Log(LogUpdate::Chunk { id: id.clone(), chunk }));
            }
            let progressed = next > offset;
            offset = offset.max(next);
            // `!progressed` guards against a daemon that says "more" without moving.
            if caught_up || !progressed {
                break;
            }
        }

        if over {
            return Ok(());
        }
        sleep(poll).await;
    }
}

/// A call with a time limit. The daemon's own errors pass through; running out of time is one more.
async fn bounded<T>(limit: Duration, call: impl Future<Output = Result<T, ClientError>>) -> Result<T, ClientError> {
    match timeout(limit, call).await {
        Ok(result) => result,
        Err(_) => Err(ClientError::Unexpected("the daemon did not answer in time".into())),
    }
}
