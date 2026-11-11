//! Wire types. Everything here is `serde`-serialisable JSON; the framing lives
//! in `codec`.
//!
//! Conversation shape:
//!
//! 1. client → `ClientFrame::Hello` carrying the newest version it speaks;
//!    server → `ServerFrame::Hello` carrying the version the connection will
//!    use (the lower of the two), or a `ServerFrame::Fatal` (and the connection
//!    closes) if the client is older than [`MIN_PROTOCOL_VERSION`].
//! 2. client → `ClientFrame::Request { id, request }`; server →
//!    `ServerFrame::Response { id, outcome }`. The daemon answers the requests
//!    of one connection **in order**, so a slow call (a big log read) delays the
//!    ones behind it; a client that must stay responsive opens a second
//!    connection for slow calls. Clients still correlate by `id`.
//! 3. After `Request::Subscribe`, the server also pushes `ServerFrame::Event`.
//!
//! Evolution: a request, response, event or error code this build does not know
//! decodes to its `Unknown` variant instead of breaking the connection. A daemon
//! answers an unknown request with `ErrorCode::Unsupported`, and a client ignores
//! an unknown event.

use scriptrunner_core::logs::{LogChunk, LogEntry};
use scriptrunner_core::{Execution, ExecutionId, ExecutionQuery, Settings, Task, TaskId, TaskOverview};
use serde::{Deserialize, Serialize};

/// The newest protocol this build speaks. Bumped on any change to the types in
/// this module; additions that older peers can ignore keep the old versions
/// working (see [`MIN_PROTOCOL_VERSION`]).
///
/// * 2: `ReadLogFrom` / `LogChunk`, fixed-width integers on the wire,
///   `Unsupported`, and version negotiation instead of an exact match.
pub const PROTOCOL_VERSION: u32 = 2;

/// The oldest protocol this build still talks. Version 1 is a subset of 2.
pub const MIN_PROTOCOL_VERSION: u32 = 1;

/// A type with a catch-all variant for values this build cannot decode.
trait Unknown {
    fn unknown() -> Self;
}

/// Decodes `T`, or its `Unknown` variant when the value is not one `T` knows
/// (a newer peer's request, response or event). `#[serde(other)]` cannot do this
/// for the adjacently tagged enums here, because it rejects a variant that
/// still carries `params` or `data`.
fn or_unknown<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Unknown,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either<T> {
        Known(T),
        Other(serde::de::IgnoredAny),
    }
    Ok(match Either::<T>::deserialize(deserializer)? {
        Either::Known(value) => value,
        Either::Other(_) => T::unknown(),
    })
}

// Wire values live for one send/receive; boxing would only push `Box` into every caller.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    Hello {
        protocol_version: u32,
    },
    Request {
        id: u64,
        #[serde(deserialize_with = "or_unknown")]
        request: Request,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    Hello {
        protocol_version: u32,
    },
    Response {
        id: u64,
        outcome: Outcome,
    },
    /// Unsolicited; only sent after `Request::Subscribe`.
    Event {
        #[serde(deserialize_with = "or_unknown")]
        event: Event,
    },
    /// The server is closing the connection (handshake failure, too many
    /// connections, malformed frame...). Not tied to any request id.
    Fatal {
        error: ErrorBody,
    },
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Ok {
        #[serde(deserialize_with = "or_unknown")]
        response: Response,
    },
    Err {
        error: ErrorBody,
    },
}

impl From<Result<Response, ErrorBody>> for Outcome {
    fn from(result: Result<Response, ErrorBody>) -> Self {
        match result {
            Ok(response) => Self::Ok { response },
            Err(error) => Self::Err { error },
        }
    }
}

impl From<Outcome> for Result<Response, ErrorBody> {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Ok { response } => Ok(response),
            Outcome::Err { error } => Err(error),
        }
    }
}

/// Mirrors the public API of the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Status,
    Overview,
    ListTasks,
    GetTask {
        id: TaskId,
    },
    CreateTask {
        task: Task,
    },
    UpdateTask {
        task: Task,
    },
    DeleteTask {
        id: TaskId,
    },
    DuplicateTask {
        id: TaskId,
    },
    SetScheduleEnabled {
        id: TaskId,
        enabled: bool,
    },
    RunNow {
        id: TaskId,
    },
    Stop {
        id: TaskId,
    },
    Executions {
        query: ExecutionQuery,
    },
    GetExecution {
        id: ExecutionId,
    },
    ClearHistory {
        id: TaskId,
    },
    /// Entries `skip..skip + limit`. The daemon rereads the file from the start, so
    /// prefer [`Request::ReadLogFrom`] for anything beyond a small log.
    ReadLog {
        id: ExecutionId,
        skip: u32,
        limit: u32,
    },
    /// Whole log as text, which fails with `TooLarge` above one frame. Kept for
    /// version 1 peers; `Client::export_log` pages with `ReadLogFrom` instead.
    ExportLog {
        id: ExecutionId,
    },
    /// Complete log lines starting at byte `offset` (0, or the `next_offset` of the
    /// previous chunk), about `max_bytes` of them. Cheap to repeat, so it is how
    /// to follow a running execution. (Protocol 2.)
    ReadLogFrom {
        id: ExecutionId,
        offset: u64,
        max_bytes: u32,
    },
    GetSettings,
    UpdateSettings {
        settings: Settings,
    },
    /// Start receiving `ServerFrame::Event`s on this connection.
    Subscribe,
    /// Never sent. What a request this build does not know decodes to.
    Unknown,
}

impl Unknown for Request {
    fn unknown() -> Self {
        Self::Unknown
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Response {
    Status(StatusInfo),
    Overview(Vec<TaskOverview>),
    Tasks(Vec<Task>),
    Task(Task),
    /// `true` when something was actually deleted.
    Deleted(bool),
    ExecutionStarted(ExecutionId),
    /// How many running or queued executions were cancelled.
    Stopped(u32),
    Executions(Vec<Execution>),
    Execution(Option<Execution>),
    LogEntries(Vec<LogEntry>),
    LogText(String),
    LogChunk(LogChunk),
    Settings(Settings),
    /// Success with nothing to report (update settings, clear history...).
    Done,
    Subscribed,
    /// Never sent. What a response this build does not know decodes to.
    Unknown,
}

impl Unknown for Response {
    fn unknown() -> Self {
        Self::Unknown
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusInfo {
    /// The daemon's crate version.
    pub version: String,
    pub protocol_version: u32,
    pub uptime_secs: u64,
}

/// Mirrors `Engine` state changes; see `service::engine::Event`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    TasksChanged,
    ExecutionChanged(Execution),
    /// The daemon dropped events for this client (it fell behind). Tasks *and*
    /// executions may be stale: refetch everything you display.
    Resync,
    /// Never sent. An event this build does not know: nothing to do with it.
    Unknown,
}

impl Unknown for Event {
    fn unknown() -> Self {
        Self::Unknown
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    AlreadyRunning,
    /// The request was well-formed but its content is not acceptable.
    Invalid,
    /// Anything else. The message is generic: paths and stack traces stay in the daemon's log.
    Internal,
    /// Handshake: the peer speaks a different `PROTOCOL_VERSION`.
    ProtocolMismatch,
    /// Too many simultaneous connections.
    Busy,
    /// The answer would not fit in one frame (8 MiB). Ask for less: a smaller page or fewer rows.
    TooLarge,
    /// The daemon does not know this request (it is older than the client).
    Unsupported,
    /// Never sent. An error code from a newer daemon; read the message.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(value: T) {
        let json = serde_json::to_string(&value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(back, value, "through {json}");
    }

    #[test]
    fn requests_round_trip() {
        round_trip(ClientFrame::Hello {
            protocol_version: PROTOCOL_VERSION,
        });
        for request in [
            Request::Status,
            Request::ListTasks,
            Request::GetTask { id: "t1".into() },
            Request::CreateTask {
                task: Task::new("Backup", "backup.py"),
            },
            Request::SetScheduleEnabled {
                id: "t1".into(),
                enabled: false,
            },
            Request::Executions {
                query: ExecutionQuery {
                    limit: Some(10),
                    ..ExecutionQuery::default()
                },
            },
            Request::ReadLog {
                id: "e1".into(),
                skip: 0,
                limit: 100,
            },
            Request::ReadLogFrom {
                id: "e1".into(),
                offset: 5_000_000_000,
                max_bytes: 1 << 20,
            },
            Request::UpdateSettings {
                settings: Settings::default(),
            },
            Request::Subscribe,
        ] {
            round_trip(ClientFrame::Request { id: 7, request });
        }
    }

    #[test]
    fn responses_and_events_round_trip() {
        let task = Task::new("Backup", "backup.py");
        let execution = Execution::new(&task, scriptrunner_core::Trigger::Manual, None);
        for outcome in [
            Outcome::Ok {
                response: Response::Done,
            },
            Outcome::Ok {
                response: Response::Task(task.clone()),
            },
            Outcome::Ok {
                response: Response::Execution(Some(execution.clone())),
            },
            Outcome::Ok {
                response: Response::Stopped(2),
            },
            Outcome::Ok {
                response: Response::LogChunk(LogChunk {
                    entries: vec![],
                    next_offset: 42,
                    eof: true,
                }),
            },
            Outcome::Err {
                error: ErrorBody::new(ErrorCode::NotFound, "task not found"),
            },
        ] {
            round_trip(ServerFrame::Response { id: 1, outcome });
        }
        round_trip(ServerFrame::Event {
            event: Event::ExecutionChanged(execution),
        });
        round_trip(ServerFrame::Event {
            event: Event::TasksChanged,
        });
        round_trip(ServerFrame::Event { event: Event::Resync });
        round_trip(ServerFrame::Fatal {
            error: ErrorBody::new(ErrorCode::TooLarge, "too big"),
        });
        round_trip(ServerFrame::Fatal {
            error: ErrorBody::new(ErrorCode::ProtocolMismatch, "unsupported protocol version"),
        });
    }

    #[test]
    fn things_from_a_newer_peer_decode_to_unknown_instead_of_failing() {
        let request: ClientFrame = serde_json::from_str(
            r#"{"type":"request","id":9,"request":{"method":"from_the_future","params":{"x":[1,2]}}}"#,
        )
        .unwrap();
        assert_eq!(
            request,
            ClientFrame::Request {
                id: 9,
                request: Request::Unknown
            }
        );
        // A newer request with no params at all.
        let bare: ClientFrame =
            serde_json::from_str(r#"{"type":"request","id":1,"request":{"method":"from_the_future"}}"#).unwrap();
        assert_eq!(
            bare,
            ClientFrame::Request {
                id: 1,
                request: Request::Unknown
            }
        );

        let response: ServerFrame =
            serde_json::from_str(r#"{"type":"response","id":2,"outcome":{"status":"ok","response":{"type":"from_the_future","data":{"y":1}}}}"#)
                .unwrap();
        assert_eq!(
            response,
            ServerFrame::Response {
                id: 2,
                outcome: Outcome::Ok {
                    response: Response::Unknown
                }
            }
        );
        let event: ServerFrame =
            serde_json::from_str(r#"{"type":"event","event":{"type":"from_the_future","data":[1]}}"#).unwrap();
        assert_eq!(event, ServerFrame::Event { event: Event::Unknown });
        let body: ErrorBody = serde_json::from_str(r#"{"code":"from_the_future","message":"m"}"#).unwrap();
        assert_eq!(body.code, ErrorCode::Unknown);
    }

    #[test]
    fn wire_format_is_stable() {
        let json = serde_json::to_string(&ClientFrame::Request {
            id: 3,
            request: Request::GetTask { id: "abc".into() },
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"request","id":3,"request":{"method":"get_task","params":{"id":"abc"}}}"#
        );
        let json = serde_json::to_string(&Outcome::Err {
            error: ErrorBody::new(ErrorCode::AlreadyRunning, "x"),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"status":"err","error":{"code":"already_running","message":"x"}}"#
        );
    }
}
