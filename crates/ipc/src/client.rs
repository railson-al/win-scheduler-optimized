//! Client side of the protocol: connect, handshake, request/response calls
//! correlated by id, and an event stream. Used by the CLI today and by the GUI
//! later.
//!
//! A background task reads every frame from the daemon: responses complete the
//! matching pending call, events fan out to subscribers, and a `Fatal` (or the
//! connection dropping) fails all pending calls. Calls are safe to issue from
//! several tasks at once on a cloned `Client`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use scriptrunner_core::logs::{LogChunk, LogEntry, format_entry};
use scriptrunner_core::{Execution, ExecutionId, ExecutionQuery, Settings, Task, TaskId, TaskOverview};
use thiserror::Error;
use tokio::io::{ReadHalf, WriteHalf};
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::codec::{CodecError, FrameReader, FrameWriter, frame_reader, frame_writer, read_frame, write_frame};
use crate::protocol::{
    ClientFrame, ErrorBody, ErrorCode, Event, MIN_PROTOCOL_VERSION, Outcome, PROTOCOL_VERSION, Request, Response,
    ServerFrame, StatusInfo,
};
use crate::transport::{Endpoint, Stream, TransportError, connect};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const EVENT_BUFFER: usize = 1024;
/// Bytes of log asked for per `ReadLogFrom` call. The daemon caps it too.
pub const LOG_CHUNK_BYTES: u32 = 1024 * 1024;
/// First protocol with `ReadLogFrom`.
const LOG_FROM_SINCE: u32 = 2;

#[derive(Debug, Error)]
pub enum ClientError {
    /// Nothing is listening: the daemon is not running.
    #[error("the ScriptRunner daemon is not running (nothing is listening on {0})")]
    NotRunning(String),
    #[error("could not reach the daemon: {0}")]
    Transport(TransportError),
    #[error("ipc protocol error: {0}")]
    Codec(#[from] CodecError),
    /// The daemon answered with an error (or closed the connection with one).
    #[error("{}", .0.message)]
    Remote(ErrorBody),
    /// The connection closed while a call was in flight.
    #[error("the connection to the daemon was lost")]
    Disconnected,
    /// The daemon sent something this client does not expect.
    #[error("unexpected message from the daemon: {0}")]
    Unexpected(String),
}

impl From<TransportError> for ClientError {
    fn from(err: TransportError) -> Self {
        match err {
            TransportError::NotRunning(address) => Self::NotRunning(address),
            other => Self::Transport(other),
        }
    }
}

pub type Result<T> = std::result::Result<T, ClientError>;

type Pending = HashMap<u64, oneshot::Sender<Outcome>>;

/// State the reader task shares with the callers.
struct Shared {
    /// `None` once the connection is closed, so a late call fails immediately
    /// instead of waiting for a response that can never come.
    pending: Mutex<Option<Pending>>,
    /// Dropped when the connection closes, so every receiver sees `Closed` instead of waiting forever.
    events: Mutex<Option<broadcast::Sender<Event>>>,
    /// Why the daemon closed the connection, when it said.
    fatal: Mutex<Option<ErrorBody>>,
}

impl Shared {
    fn lock_pending(&self) -> std::sync::MutexGuard<'_, Option<Pending>> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn lock_fatal(&self) -> std::sync::MutexGuard<'_, Option<ErrorBody>> {
        self.fatal.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn lock_events(&self) -> std::sync::MutexGuard<'_, Option<broadcast::Sender<Event>>> {
        self.events.lock().unwrap_or_else(|p| p.into_inner())
    }
}

struct Inner {
    /// What the handshake settled on; older than ours when the daemon is older.
    protocol_version: u32,
    shared: Arc<Shared>,
    writer: tokio::sync::Mutex<FrameWriter<WriteHalf<Stream>>>,
    next_id: AtomicU64,
    reader_task: JoinHandle<()>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.reader_task.abort();
    }
}

/// A connection to the daemon. Cheap to clone; all clones share the connection.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl Client {
    /// Connects and performs the handshake.
    pub async fn connect(endpoint: &Endpoint) -> Result<Self> {
        let stream = connect(endpoint).await?;
        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = frame_reader(read_half);
        let mut writer = frame_writer(write_half);

        write_frame(
            &mut writer,
            &ClientFrame::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .await?;
        let negotiated = match timeout(HANDSHAKE_TIMEOUT, read_frame::<_, ServerFrame>(&mut reader)).await {
            Err(_) => {
                return Err(ClientError::Unexpected(
                    "the daemon did not answer the handshake".into(),
                ));
            }
            Ok(Ok(Some(ServerFrame::Hello { protocol_version })))
                if (MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&protocol_version) =>
            {
                protocol_version
            }
            Ok(Ok(Some(ServerFrame::Hello { protocol_version }))) => {
                return Err(ClientError::Unexpected(format!(
                    "the daemon speaks protocol {protocol_version}, this client speaks {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION}"
                )));
            }
            Ok(Ok(Some(ServerFrame::Fatal { error }))) => return Err(ClientError::Remote(error)),
            Ok(Ok(Some(_))) => {
                return Err(ClientError::Unexpected(
                    "the handshake was not answered with a hello".into(),
                ));
            }
            Ok(Ok(None)) => return Err(ClientError::Disconnected),
            Ok(Err(err)) => return Err(err.into()),
        };

        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let shared = Arc::new(Shared {
            pending: Mutex::new(Some(HashMap::new())),
            events: Mutex::new(Some(events)),
            fatal: Mutex::new(None),
        });
        let reader_task = tokio::spawn(read_loop(reader, Arc::clone(&shared)));
        Ok(Self {
            inner: Arc::new(Inner {
                protocol_version: negotiated,
                shared,
                writer: tokio::sync::Mutex::new(writer),
                next_id: AtomicU64::new(1),
                reader_task,
            }),
        })
    }

    /// The protocol this connection speaks: ours, or an older one if the daemon is older.
    pub fn protocol_version(&self) -> u32 {
        self.inner.protocol_version
    }

    /// Sends one request and waits for its response. A daemon-side error comes
    /// back as `ClientError::Remote`.
    pub async fn call(&self, request: Request) -> Result<Response> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        match self.inner.shared.lock_pending().as_mut() {
            Some(pending) => {
                pending.insert(id, reply);
            }
            None => return Err(self.closed_error()),
        }

        let sent = {
            let mut writer = self.inner.writer.lock().await;
            write_frame(&mut *writer, &ClientFrame::Request { id, request }).await
        };
        if let Err(err) = sent {
            if let Some(pending) = self.inner.shared.lock_pending().as_mut() {
                pending.remove(&id);
            }
            return Err(err.into());
        }

        match answer.await {
            Ok(outcome) => std::result::Result::<Response, ErrorBody>::from(outcome).map_err(ClientError::Remote),
            // The reader task dropped our sender: the connection is gone.
            Err(_) => Err(self.closed_error()),
        }
    }

    fn closed_error(&self) -> ClientError {
        match self.inner.shared.lock_fatal().clone() {
            Some(error) => ClientError::Remote(error),
            None => ClientError::Disconnected,
        }
    }

    /// Starts receiving daemon events. Each call returns an independent receiver.
    ///
    /// * `recv()` returning `Closed` means the connection to the daemon is gone.
    /// * `Lagged` (this client was slow) and `Event::Resync` (the daemon's own
    ///   broadcast overflowed) both mean events were lost: refetch whatever you
    ///   display instead of trusting the stream.
    pub async fn subscribe(&self) -> Result<broadcast::Receiver<Event>> {
        // Create the receiver first so nothing pushed right after the reply is lost.
        let receiver = match self.inner.shared.lock_events().as_ref() {
            Some(events) => events.subscribe(),
            None => return Err(self.closed_error()),
        };
        match self.call(Request::Subscribe).await? {
            Response::Subscribed => Ok(receiver),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn status(&self) -> Result<StatusInfo> {
        match self.call(Request::Status).await? {
            Response::Status(status) => Ok(status),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn overview(&self) -> Result<Vec<TaskOverview>> {
        match self.call(Request::Overview).await? {
            Response::Overview(rows) => Ok(rows),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn list_tasks(&self) -> Result<Vec<Task>> {
        match self.call(Request::ListTasks).await? {
            Response::Tasks(tasks) => Ok(tasks),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn get_task(&self, id: &TaskId) -> Result<Task> {
        self.task_call(Request::GetTask { id: id.clone() }).await
    }

    pub async fn create_task(&self, task: Task) -> Result<Task> {
        self.task_call(Request::CreateTask { task }).await
    }

    pub async fn update_task(&self, task: Task) -> Result<Task> {
        self.task_call(Request::UpdateTask { task }).await
    }

    /// `true` when something was actually deleted.
    pub async fn delete_task(&self, id: &TaskId) -> Result<bool> {
        match self.call(Request::DeleteTask { id: id.clone() }).await? {
            Response::Deleted(deleted) => Ok(deleted),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn duplicate_task(&self, id: &TaskId) -> Result<Task> {
        self.task_call(Request::DuplicateTask { id: id.clone() }).await
    }

    pub async fn set_schedule_enabled(&self, id: &TaskId, enabled: bool) -> Result<Task> {
        self.task_call(Request::SetScheduleEnabled {
            id: id.clone(),
            enabled,
        })
        .await
    }

    pub async fn run_now(&self, id: &TaskId) -> Result<ExecutionId> {
        match self.call(Request::RunNow { id: id.clone() }).await? {
            Response::ExecutionStarted(execution) => Ok(execution),
            other => Err(unexpected(&other)),
        }
    }

    /// How many running or queued executions were cancelled.
    pub async fn stop(&self, id: &TaskId) -> Result<u32> {
        match self.call(Request::Stop { id: id.clone() }).await? {
            Response::Stopped(count) => Ok(count),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn executions(&self, query: ExecutionQuery) -> Result<Vec<Execution>> {
        match self.call(Request::Executions { query }).await? {
            Response::Executions(rows) => Ok(rows),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn get_execution(&self, id: &ExecutionId) -> Result<Option<Execution>> {
        match self.call(Request::GetExecution { id: id.clone() }).await? {
            Response::Execution(execution) => Ok(execution),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn clear_history(&self, id: &TaskId) -> Result<()> {
        self.done_call(Request::ClearHistory { id: id.clone() }).await
    }

    pub async fn read_log(&self, id: &ExecutionId, skip: u32, limit: u32) -> Result<Vec<LogEntry>> {
        match self
            .call(Request::ReadLog {
                id: id.clone(),
                skip,
                limit,
            })
            .await?
        {
            Response::LogEntries(entries) => Ok(entries),
            other => Err(unexpected(&other)),
        }
    }

    /// One page of the log from byte `offset` (0, or the `next_offset` of the previous
    /// page). Poll it to follow a running execution: it costs the same on every call.
    /// Needs a daemon that speaks protocol 2.
    pub async fn read_log_from(&self, id: &ExecutionId, offset: u64, max_bytes: u32) -> Result<LogChunk> {
        if self.inner.protocol_version < LOG_FROM_SINCE {
            return Err(ClientError::Remote(ErrorBody::new(
                ErrorCode::Unsupported,
                "the daemon is older than this client and cannot read logs by position; update it",
            )));
        }
        match self
            .call(Request::ReadLogFrom {
                id: id.clone(),
                offset,
                max_bytes,
            })
            .await?
        {
            Response::LogChunk(chunk) => Ok(chunk),
            other => Err(unexpected(&other)),
        }
    }

    /// The whole log as text, however large: it is fetched page by page, so it is no longer
    /// limited by the size of one frame. (It does build the whole text in memory.)
    pub async fn export_log(&self, id: &ExecutionId) -> Result<String> {
        if self.inner.protocol_version < LOG_FROM_SINCE {
            return match self.call(Request::ExportLog { id: id.clone() }).await? {
                Response::LogText(text) => Ok(text),
                other => Err(unexpected(&other)),
            };
        }
        let mut text = String::new();
        let mut offset = 0;
        loop {
            let chunk = self.read_log_from(id, offset, LOG_CHUNK_BYTES).await?;
            text.extend(chunk.entries.iter().map(format_entry));
            if chunk.eof {
                return Ok(text);
            }
            if chunk.next_offset <= offset {
                return Err(ClientError::Unexpected("the daemon's log read made no progress".into()));
            }
            offset = chunk.next_offset;
        }
    }

    pub async fn settings(&self) -> Result<Settings> {
        match self.call(Request::GetSettings).await? {
            Response::Settings(settings) => Ok(settings),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn update_settings(&self, settings: Settings) -> Result<()> {
        self.done_call(Request::UpdateSettings { settings }).await
    }

    async fn task_call(&self, request: Request) -> Result<Task> {
        match self.call(request).await? {
            Response::Task(task) => Ok(task),
            other => Err(unexpected(&other)),
        }
    }

    async fn done_call(&self, request: Request) -> Result<()> {
        match self.call(request).await? {
            Response::Done => Ok(()),
            other => Err(unexpected(&other)),
        }
    }
}

fn unexpected(response: &Response) -> ClientError {
    ClientError::Unexpected(format!("unexpected response {response:?}"))
}

async fn read_loop(mut reader: FrameReader<ReadHalf<Stream>>, shared: Arc<Shared>) {
    loop {
        match read_frame::<_, ServerFrame>(&mut reader).await {
            Ok(Some(ServerFrame::Response { id, outcome })) => {
                let reply = shared.lock_pending().as_mut().and_then(|pending| pending.remove(&id));
                if let Some(reply) = reply {
                    // The caller may have given up; nothing to do then.
                    let _ = reply.send(outcome);
                }
            }
            Ok(Some(ServerFrame::Event { event })) => {
                // No receivers is fine: nobody subscribed yet or anymore.
                if let Some(events) = shared.lock_events().as_ref() {
                    let _ = events.send(event);
                }
            }
            Ok(Some(ServerFrame::Fatal { error })) => {
                *shared.lock_fatal() = Some(error);
                break;
            }
            // A second hello is not part of the protocol; treat it as a broken peer.
            Ok(Some(ServerFrame::Hello { .. })) | Ok(None) | Err(_) => break,
        }
    }
    // Dropping the senders wakes every pending call with an error, and every
    // event receiver with `Closed`.
    *shared.lock_pending() = None;
    *shared.lock_events() = None;
}
