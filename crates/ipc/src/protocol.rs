//! Wire types. Everything here is `serde`-serialisable JSON; the framing lives
//! in `codec`.
//!
//! Conversation shape:
//!
//! 1. client → `ClientFrame::Hello`; server → `ServerFrame::Hello`, or a
//!    `ServerFrame::Fatal` (and the connection closes) on a version mismatch.
//! 2. client → `ClientFrame::Request { id, request }`; server →
//!    `ServerFrame::Response { id, outcome }`. Responses may arrive out of order,
//!    so the client correlates them by `id`.
//! 3. After `Request::Subscribe`, the server also pushes `ServerFrame::Event`.

use scriptrunner_core::logs::LogEntry;
use scriptrunner_core::{Execution, ExecutionId, ExecutionQuery, Settings, Task, TaskId, TaskOverview};
use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to the types in this module.
pub const PROTOCOL_VERSION: u32 = 1;

// Wire values live for one send/receive; boxing would only push `Box` into every caller.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    Hello { protocol_version: u32 },
    Request { id: u64, request: Request },
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
    Ok { response: Response },
    Err { error: ErrorBody },
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
    ReadLog {
        id: ExecutionId,
        skip: usize,
        limit: usize,
    },
    ExportLog {
        id: ExecutionId,
    },
    GetSettings,
    UpdateSettings {
        settings: Settings,
    },
    /// Start receiving `ServerFrame::Event`s on this connection.
    Subscribe,
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
    Stopped(usize),
    Executions(Vec<Execution>),
    Execution(Option<Execution>),
    LogEntries(Vec<LogEntry>),
    LogText(String),
    Settings(Settings),
    /// Success with nothing to report (update settings, clear history...).
    Done,
    Subscribed,
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
