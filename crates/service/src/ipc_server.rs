//! IPC server: accepts connections from `scriptrunner-ipc` clients and maps
//! each request onto the `Engine`.
//!
//! Per connection:
//! * one task reads requests and answers them one at a time (so a client sees
//!   its responses in order, though the protocol does not require it);
//! * one task owns the write half and drains an outbox, so responses and pushed
//!   events never interleave inside a frame;
//! * after `Subscribe`, a third task forwards engine events into the outbox.
//!
//! Every engine call goes through `Engine::blocking`, so SQLite and file I/O
//! never stall the async runtime that also drives the scheduler.

use std::sync::Arc;
use std::time::{Duration, Instant};

use scriptrunner_ipc::codec::{
    CodecError, FrameReader, FrameWriter, frame_reader, frame_writer, read_frame, write_frame,
};
use scriptrunner_ipc::transport::{Listener, Stream};
use scriptrunner_ipc::{
    ClientFrame, ErrorBody, ErrorCode, Event as WireEvent, MIN_PROTOCOL_VERSION, Outcome, PROTOCOL_VERSION, Request,
    Response, ServerFrame, StatusInfo,
};
use scriptrunner_storage::StorageError;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Semaphore, broadcast, mpsc};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::engine::{Engine, EngineError, Event};

/// Simultaneous clients allowed; the next one is told `Busy` and disconnected.
pub const MAX_CONNECTIONS: usize = 32;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// A client that stops reading must not hold a connection slot forever: one
/// frame that cannot be written within this long closes the connection.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Frames waiting to be written to one client before the producers back off.
const OUTBOX_CAPACITY: usize = 256;
/// A response must fit in one frame (8 MiB); these caps keep history and log
/// pages comfortably below that. Clients page with `offset` / `skip`.
const MAX_QUERY_ROWS: u32 = 5_000;
const MAX_LOG_LINES: usize = 20_000;
/// Bytes of a log file per `ReadLogFrom`. A short log line grows about 4x as JSON
/// (timestamp and field names), so this stays well below one frame.
const MAX_LOG_CHUNK_BYTES: u64 = 1024 * 1024;

/// Serves clients until `shutdown` is cancelled, then waits (bounded) for the
/// open connections to close.
pub async fn serve(engine: Engine, mut listener: Listener, shutdown: CancellationToken) {
    let started = Instant::now();
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut connections = JoinSet::new();

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            Some(finished) = connections.join_next(), if !connections.is_empty() => {
                if let Err(err) = finished {
                    warn!(%err, "an ipc connection task ended abnormally");
                }
            }
            accepted = listener.accept() => match accepted {
                Ok(stream) => match Arc::clone(&slots).try_acquire_owned() {
                    Ok(permit) => {
                        let engine = engine.clone();
                        let shutdown = shutdown.child_token();
                        connections.spawn(async move {
                            let _permit = permit;
                            handle_connection(engine, stream, started, shutdown).await;
                        });
                    }
                    Err(_) => {
                        connections.spawn(reject_busy(stream));
                    }
                },
                Err(err) => {
                    error!(%err, "accepting an ipc connection failed");
                    // Do not spin if the listener is persistently failing.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    }

    // The connections hold child tokens of `shutdown`, so they are already winding down.
    let drain = async { while connections.join_next().await.is_some() {} };
    if timeout(SHUTDOWN_GRACE, drain).await.is_err() {
        warn!("ipc connections did not close in time; aborting them");
        connections.abort_all();
    }
}

async fn reject_busy(stream: Stream) {
    let (mut read_half, write_half) = tokio::io::split(stream);
    let mut writer = frame_writer(write_half);
    let fatal = ServerFrame::Fatal {
        error: ErrorBody::new(ErrorCode::Busy, "too many simultaneous connections"),
    };
    let work = async {
        if write_frame(&mut writer, &fatal).await.is_ok() {
            // Closing with unread input (the client's hello) makes the OS reset the
            // connection, which can discard the `Fatal` before the client reads it.
            // So signal EOF first, then drain until the client hangs up.
            let _ = writer.get_mut().shutdown().await;
            let _ = tokio::io::copy(&mut read_half, &mut tokio::io::sink()).await;
        }
    };
    let _ = timeout(Duration::from_secs(2), work).await;
}

async fn handle_connection(engine: Engine, stream: Stream, started: Instant, shutdown: CancellationToken) {
    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = frame_reader(read_half);
    let (outbox, inbox) = mpsc::channel(OUTBOX_CAPACITY);
    // Cancelled on server shutdown, or when the writer finds the client gone.
    let broken = shutdown.child_token();
    let mut writer = tokio::spawn(write_loop(frame_writer(write_half), inbox, broken.clone()));

    if let Some(fatal) = run_session(&engine, &mut reader, &outbox, started, &broken).await {
        // Bounded: with a stuck client the outbox may be full.
        let _ = timeout(Duration::from_secs(2), outbox.send(ServerFrame::Fatal { error: fatal })).await;
    }

    // Closing the outbox lets the writer flush what is queued (including a
    // `Fatal`) and stop; abort it if the client is not reading.
    drop(outbox);
    if timeout(Duration::from_secs(2), &mut writer).await.is_err() {
        writer.abort();
    }
}

/// Runs the conversation. Returns the `Fatal` error to send before closing, or
/// `None` for a clean close.
async fn run_session<R: AsyncRead + Unpin>(
    engine: &Engine,
    reader: &mut FrameReader<R>,
    outbox: &mpsc::Sender<ServerFrame>,
    started: Instant,
    broken: &CancellationToken,
) -> Option<ErrorBody> {
    let first = tokio::select! {
        _ = broken.cancelled() => return None,
        first = timeout(HANDSHAKE_TIMEOUT, read_frame::<_, ClientFrame>(reader)) => first,
    };
    // The client names the newest protocol it speaks; the connection uses the lower of the two,
    // as long as the client is not older than the oldest one this daemon still serves.
    let negotiated = match first {
        Err(_) => return Some(ErrorBody::new(ErrorCode::Invalid, "handshake timed out")),
        Ok(Ok(Some(ClientFrame::Hello { protocol_version }))) if protocol_version >= MIN_PROTOCOL_VERSION => {
            protocol_version.min(PROTOCOL_VERSION)
        }
        Ok(Ok(Some(ClientFrame::Hello { protocol_version }))) => {
            return Some(ErrorBody::new(
                ErrorCode::ProtocolMismatch,
                format!(
                    "the daemon speaks protocols {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION}, the client sent {protocol_version}"
                ),
            ));
        }
        Ok(Ok(Some(_))) => return Some(ErrorBody::new(ErrorCode::Invalid, "the first frame must be a hello")),
        // The peer left without a word (e.g. the stale-socket probe in `Listener::bind`).
        Ok(Ok(None)) => return None,
        Ok(Err(err)) => return frame_error(err),
    };
    let hello = ServerFrame::Hello {
        protocol_version: negotiated,
    };
    if !enqueue(outbox, hello, broken).await {
        return None;
    }

    let mut subscription: Option<JoinHandle<()>> = None;
    let fatal = loop {
        let frame = tokio::select! {
            _ = broken.cancelled() => break None,
            frame = read_frame::<_, ClientFrame>(reader) => frame,
        };
        match frame {
            Ok(Some(ClientFrame::Request { id, request })) => {
                let result = match request {
                    Request::Subscribe => {
                        // Subscribe *before* replying, so no event after the reply is missed.
                        if subscription.is_none() {
                            subscription = Some(spawn_forwarder(engine.subscribe(), outbox.clone(), broken.clone()));
                        }
                        Ok(Response::Subscribed)
                    }
                    Request::Unknown => Err(ErrorBody::new(
                        ErrorCode::Unsupported,
                        "this daemon does not understand that request; it may be older than the client",
                    )),
                    other => dispatch(engine, started, other).await.map_err(|err| error_body(&err)),
                };
                let response = ServerFrame::Response {
                    id,
                    outcome: Outcome::from(result),
                };
                if !enqueue(outbox, response, broken).await {
                    break None;
                }
            }
            Ok(Some(ClientFrame::Hello { .. })) => {
                break Some(ErrorBody::new(
                    ErrorCode::Invalid,
                    "unexpected hello after the handshake",
                ));
            }
            Ok(None) => break None,
            Err(err) => break frame_error(err),
        }
    };

    if let Some(forwarder) = subscription {
        // Awaited so its clone of the outbox is gone before the writer waits for the channel to close.
        forwarder.abort();
        let _ = forwarder.await;
    }
    fatal
}

/// Queues a frame for the writer. Gives up (returning `false`) as soon as the
/// connection is broken, so a client that stopped reading cannot pin this task
/// on a full outbox.
async fn enqueue(outbox: &mpsc::Sender<ServerFrame>, frame: ServerFrame, broken: &CancellationToken) -> bool {
    tokio::select! {
        _ = broken.cancelled() => false,
        sent = outbox.send(frame) => sent.is_ok(),
    }
}

/// Frame-level failures: a malformed or oversized frame gets a `Fatal`; a broken
/// stream just closes.
fn frame_error(err: CodecError) -> Option<ErrorBody> {
    match err {
        CodecError::Json(err) => {
            debug!(%err, "malformed ipc frame");
            Some(ErrorBody::new(ErrorCode::Invalid, "malformed frame"))
        }
        CodecError::FrameTooLarge => Some(ErrorBody::new(ErrorCode::Invalid, "frame too large")),
        CodecError::Io(err) => {
            debug!(%err, "ipc connection closed");
            None
        }
    }
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: FrameWriter<W>,
    mut inbox: mpsc::Receiver<ServerFrame>,
    broken: CancellationToken,
) {
    while let Some(frame) = inbox.recv().await {
        match timeout(WRITE_TIMEOUT, write_frame(&mut writer, &frame)).await {
            Ok(Ok(())) => {}
            Ok(Err(CodecError::FrameTooLarge)) => {
                // A response that cannot fit is replaced by an error so the client's call still completes.
                if let ServerFrame::Response { id, .. } = frame {
                    let error = ErrorBody::new(
                        ErrorCode::TooLarge,
                        "the response is too large to send; ask for a smaller page",
                    );
                    let reply = ServerFrame::Response {
                        id,
                        outcome: Outcome::Err { error },
                    };
                    if !matches!(
                        timeout(WRITE_TIMEOUT, write_frame(&mut writer, &reply)).await,
                        Ok(Ok(()))
                    ) {
                        break;
                    }
                }
            }
            Ok(Err(err)) => {
                debug!(%err, "ipc write failed");
                break;
            }
            Err(_) => {
                warn!("an ipc client stopped reading; closing its connection");
                break;
            }
        }
    }
    broken.cancel();
}

fn spawn_forwarder(
    mut events: broadcast::Receiver<Event>,
    outbox: mpsc::Sender<ServerFrame>,
    broken: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let received = tokio::select! {
                _ = broken.cancelled() => return,
                received = events.recv() => received,
            };
            let event = match received {
                Ok(Event::TasksChanged) => WireEvent::TasksChanged,
                Ok(Event::ExecutionChanged(execution)) => WireEvent::ExecutionChanged(execution),
                Err(RecvError::Lagged(skipped)) => {
                    // Events were lost; `Resync` tells the client to refetch tasks and executions.
                    warn!(skipped, "ipc subscriber fell behind; asking it to resync");
                    WireEvent::Resync
                }
                Err(RecvError::Closed) => return,
            };
            if !enqueue(&outbox, ServerFrame::Event { event }, &broken).await {
                return;
            }
        }
    })
}

async fn dispatch(engine: &Engine, started: Instant, request: Request) -> Result<Response, EngineError> {
    Ok(match request {
        Request::Status => Response::Status(StatusInfo {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_version: PROTOCOL_VERSION,
            uptime_secs: started.elapsed().as_secs(),
        }),
        Request::Overview => Response::Overview(engine.blocking(|e| e.overview()).await?),
        Request::ListTasks => Response::Tasks(engine.blocking(|e| e.list_tasks()).await?),
        Request::GetTask { id } => Response::Task(engine.blocking(move |e| e.get_task(&id)).await?),
        Request::CreateTask { task } => Response::Task(engine.blocking(move |e| e.create_task(task)).await?),
        Request::UpdateTask { task } => Response::Task(engine.blocking(move |e| e.update_task(task)).await?),
        Request::DeleteTask { id } => Response::Deleted(engine.blocking(move |e| e.delete_task(&id)).await?),
        Request::DuplicateTask { id } => Response::Task(engine.blocking(move |e| e.duplicate_task(&id)).await?),
        Request::SetScheduleEnabled { id, enabled } => {
            Response::Task(engine.blocking(move |e| e.set_schedule_enabled(&id, enabled)).await?)
        }
        Request::RunNow { id } => Response::ExecutionStarted(engine.blocking(move |e| e.run_now(&id)).await?),
        Request::Stop { id } => Response::Stopped(
            engine
                .blocking(move |e| Ok(u32::try_from(e.stop(&id)).unwrap_or(u32::MAX)))
                .await?,
        ),
        Request::Executions { mut query } => {
            query.limit = Some(query.limit.map_or(MAX_QUERY_ROWS, |limit| limit.min(MAX_QUERY_ROWS)));
            Response::Executions(engine.blocking(move |e| e.executions(&query)).await?)
        }
        Request::GetExecution { id } => Response::Execution(engine.blocking(move |e| e.get_execution(&id)).await?),
        Request::ClearHistory { id } => {
            engine.blocking(move |e| e.clear_history(&id)).await?;
            Response::Done
        }
        Request::ReadLog { id, skip, limit } => {
            let limit = (limit as usize).min(MAX_LOG_LINES);
            Response::LogEntries(engine.blocking(move |e| e.read_log(&id, skip as usize, limit)).await?)
        }
        Request::ReadLogFrom { id, offset, max_bytes } => {
            let max_bytes = u64::from(max_bytes).min(MAX_LOG_CHUNK_BYTES);
            Response::LogChunk(
                engine
                    .blocking(move |e| e.read_log_from(&id, offset, max_bytes))
                    .await?,
            )
        }
        Request::ExportLog { id } => Response::LogText(engine.blocking(move |e| e.export_log(&id)).await?),
        Request::GetSettings => Response::Settings(engine.blocking(|e| e.settings()).await?),
        Request::UpdateSettings { settings } => {
            engine.blocking(move |e| e.update_settings(settings)).await?;
            Response::Done
        }
        // Handled by the session, which owns the subscription and answers unknown requests.
        Request::Subscribe | Request::Unknown => Response::Subscribed,
    })
}

/// Maps engine errors to wire errors. Only validation and lookup messages,
/// which are about the caller's own input, are passed through; anything else is
/// logged here and reported generically so paths and SQL never reach a client.
fn error_body(err: &EngineError) -> ErrorBody {
    match err {
        EngineError::TaskNotFound(_) | EngineError::Storage(StorageError::NotFound(_)) => {
            ErrorBody::new(ErrorCode::NotFound, err.to_string())
        }
        EngineError::AlreadyRunning(_) => ErrorBody::new(ErrorCode::AlreadyRunning, err.to_string()),
        EngineError::Core(_) | EngineError::Storage(StorageError::Core(_)) => {
            ErrorBody::new(ErrorCode::Invalid, err.to_string())
        }
        _ => {
            error!(%err, "ipc request failed");
            ErrorBody::new(ErrorCode::Internal, "internal error; see the daemon log")
        }
    }
}
