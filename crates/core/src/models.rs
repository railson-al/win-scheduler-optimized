use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::errors::{CoreError, Result};
use crate::schedule::Schedule;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4().to_string())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl FromStr for $name {
            type Err = CoreError;

            fn from_str(s: &str) -> Result<Self> {
                match s {
                    $($text => Ok(Self::$variant),)+
                    other => Err(CoreError::UnknownValue(format!(
                        "{} {:?}",
                        stringify!($name),
                        other
                    ))),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

id_type!(TaskId);
id_type!(ExecutionId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptKind {
    Python,
    Bat,
    Cmd,
    PowerShell,
    Exe,
}

string_enum!(ScriptKind {
    Python => "python",
    Bat => "bat",
    Cmd => "cmd",
    PowerShell => "power_shell",
    Exe => "exe",
});

impl ScriptKind {
    /// Infers the kind from the file extension (case-insensitive).
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "py" | "pyw" => Some(Self::Python),
            "bat" => Some(Self::Bat),
            "cmd" => Some(Self::Cmd),
            "ps1" => Some(Self::PowerShell),
            "exe" => Some(Self::Exe),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Manual,
    Scheduled,
}

string_enum!(Trigger {
    Manual => "manual",
    Scheduled => "scheduled",
});

/// State of one execution attempt. The schedule on/off state lives on the
/// task (`schedule_enabled`) and is deliberately independent of this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    /// Registered, waiting for a free global execution slot.
    Queued,
    Running,
    Success,
    Failed,
    /// Cancelled by the user or cut short by a service restart.
    Interrupted,
    /// Never started: overlap rule, missed run while the machine was off, etc.
    Skipped,
}

string_enum!(ExecutionStatus {
    Queued => "queued",
    Running => "running",
    Success => "success",
    Failed => "failed",
    Interrupted => "interrupted",
    Skipped => "skipped",
});

impl ExecutionStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub name: String,
    pub description: Option<String>,
    pub script_path: PathBuf,
    pub script_kind: ScriptKind,
    /// Defaults to the script's own directory when unset.
    pub working_dir: Option<PathBuf>,
    pub args: Vec<String>,
    /// Python / PowerShell interpreter override (e.g. a venv's `python.exe`).
    pub interpreter: Option<PathBuf>,
    /// `None` means the task only ever runs manually.
    pub schedule: Option<Schedule>,
    pub schedule_enabled: bool,
    pub timeout_secs: Option<u64>,
    pub allow_concurrent: bool,
    pub capture_logs: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Task {
    pub fn new(name: impl Into<String>, script_path: impl Into<PathBuf>) -> Self {
        let script_path = script_path.into();
        let now = Utc::now();
        Self {
            id: TaskId::new(),
            name: name.into(),
            description: None,
            script_kind: ScriptKind::from_path(&script_path).unwrap_or(ScriptKind::Exe),
            script_path,
            working_dir: None,
            args: Vec::new(),
            interpreter: None,
            schedule: None,
            schedule_enabled: true,
            timeout_secs: None,
            allow_concurrent: false,
            capture_logs: true,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(CoreError::InvalidTask("name must not be empty".into()));
        }
        if self.script_path.as_os_str().is_empty() {
            return Err(CoreError::InvalidTask("script path must not be empty".into()));
        }
        if self.timeout_secs == Some(0) {
            return Err(CoreError::InvalidTask("timeout must be greater than zero".into()));
        }
        if let Some(schedule) = &self.schedule {
            schedule.validate()?;
        }
        Ok(())
    }

    /// Copy with a fresh id and timestamps, for the "duplicate" action.
    /// The copy starts disabled so it never fires before being reviewed.
    pub fn duplicate(&self) -> Self {
        let now = Utc::now();
        let mut copy = self.clone();
        copy.id = TaskId::new();
        copy.name = format!("{} (cópia)", self.name);
        copy.schedule_enabled = false;
        copy.created_at = now;
        copy.updated_at = now;
        copy
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Execution {
    pub id: ExecutionId,
    pub task_id: TaskId,
    /// Task name at the time of the attempt (the task may be renamed or deleted later).
    pub task_name: String,
    pub trigger: Trigger,
    /// The slot this run was scheduled for; `None` for manual runs.
    pub scheduled_for: Option<DateTime<Utc>>,
    /// When the attempt was registered.
    pub created_at: DateTime<Utc>,
    /// When the process was actually spawned.
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: ExecutionStatus,
    pub exit_code: Option<i32>,
    pub failure_reason: Option<String>,
    /// Log file name, relative to the logs directory.
    pub log_file: Option<String>,
    pub log_bytes: u64,
    pub log_truncated: bool,
}

impl Execution {
    pub fn new(task: &Task, trigger: Trigger, scheduled_for: Option<DateTime<Utc>>) -> Self {
        Self {
            id: ExecutionId::new(),
            task_id: task.id.clone(),
            task_name: task.name.clone(),
            trigger,
            scheduled_for,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            status: ExecutionStatus::Queued,
            exit_code: None,
            failure_reason: None,
            log_file: None,
            log_bytes: 0,
            log_truncated: false,
        }
    }

    pub fn duration(&self) -> Option<chrono::Duration> {
        Some(self.finished_at? - self.started_at?)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Days of history to keep; `0` keeps everything.
    pub history_retention_days: u32,
    pub max_log_bytes_per_execution: u64,
    pub max_total_log_bytes: u64,
    pub default_timeout_secs: Option<u64>,
    pub default_scripts_dir: Option<PathBuf>,
    pub default_python: Option<PathBuf>,
    pub max_concurrent_executions: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            history_retention_days: 90,
            max_log_bytes_per_execution: 5 * 1024 * 1024,
            max_total_log_bytes: 512 * 1024 * 1024,
            default_timeout_secs: None,
            default_scripts_dir: None,
            default_python: None,
            max_concurrent_executions: 4,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        if self.max_concurrent_executions == 0 {
            return Err(CoreError::InvalidTask(
                "max concurrent executions must be at least 1".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_is_inferred_from_extension() {
        assert_eq!(
            ScriptKind::from_path(Path::new("C:\\x\\Backup.PY")),
            Some(ScriptKind::Python)
        );
        assert_eq!(ScriptKind::from_path(Path::new("a.bat")), Some(ScriptKind::Bat));
        assert_eq!(ScriptKind::from_path(Path::new("a.cmd")), Some(ScriptKind::Cmd));
        assert_eq!(ScriptKind::from_path(Path::new("a.ps1")), Some(ScriptKind::PowerShell));
        assert_eq!(ScriptKind::from_path(Path::new("a.exe")), Some(ScriptKind::Exe));
        assert_eq!(ScriptKind::from_path(Path::new("a.txt")), None);
    }

    #[test]
    fn enums_round_trip_through_strings() {
        for kind in [
            ScriptKind::Python,
            ScriptKind::Bat,
            ScriptKind::Cmd,
            ScriptKind::PowerShell,
            ScriptKind::Exe,
        ] {
            assert_eq!(kind.as_str().parse::<ScriptKind>().unwrap(), kind);
        }
        assert!("nope".parse::<ExecutionStatus>().is_err());
    }

    #[test]
    fn validation_rejects_bad_tasks() {
        assert!(Task::new("  ", "a.py").validate().is_err());
        assert!(Task::new("x", "").validate().is_err());
        let mut t = Task::new("x", "a.py");
        t.timeout_secs = Some(0);
        assert!(t.validate().is_err());
        t.timeout_secs = Some(5);
        assert!(t.validate().is_ok());
    }

    #[test]
    fn duplicate_gets_new_identity_and_is_disabled() {
        let t = Task::new("Backup", "a.py");
        let d = t.duplicate();
        assert_ne!(t.id, d.id);
        assert!(!d.schedule_enabled);
        assert_eq!(d.script_path, t.script_path);
    }

    #[test]
    fn partial_settings_json_uses_defaults() {
        let s: Settings = serde_json::from_str(r#"{"max_concurrent_executions": 8}"#).unwrap();
        assert_eq!(s.max_concurrent_executions, 8);
        assert_eq!(s.history_retention_days, 90);
    }
}
