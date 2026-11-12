//! Platform-independent heart of ScriptRunner: data model, schedule maths and
//! the process executor. Windows-only bits (Job Objects, OEM code pages) are
//! isolated behind `cfg(windows)` so everything else builds and tests on Linux.

/// Name of the Windows service. Shared so the pieces that install, run and
/// connect to it (including the client's check that the pipe's server really is
/// the service) agree on it.
pub const SERVICE_NAME: &str = "ScriptRunner";

pub mod errors;
pub mod executor;
pub mod logs;
pub mod models;
pub mod paths;
pub mod schedule;

pub use errors::{CoreError, Result};
pub use models::{
    Execution, ExecutionId, ExecutionQuery, ExecutionStatus, RunAs, ScriptKind, Settings, Task, TaskId, TaskOverview,
    Trigger,
};
pub use schedule::Schedule;
