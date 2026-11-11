//! Platform-independent heart of ScriptRunner: data model, schedule maths and
//! the process executor. Windows-only bits (Job Objects, OEM code pages) are
//! isolated behind `cfg(windows)` so everything else builds and tests on Linux.

pub mod errors;
pub mod executor;
pub mod logs;
pub mod models;
pub mod schedule;

pub use errors::{CoreError, Result};
pub use models::{Execution, ExecutionId, ExecutionStatus, ScriptKind, Settings, Task, TaskId, Trigger};
pub use schedule::Schedule;
