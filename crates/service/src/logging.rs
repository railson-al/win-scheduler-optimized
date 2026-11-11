//! Logging shared by the console daemon and the future Windows service.
//!
//! Console output is for a person at a terminal. File output is for a process
//! that runs unattended and must leave a record behind; `emergency` covers the
//! failures that happen before either one is up.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use chrono::{SecondsFormat, Utc};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;

const LOG_PREFIX: &str = "scriptrunner-service";
const LOG_SUFFIX: &str = "log";
const KEPT_LOG_FILES: usize = 14;
const EMERGENCY_FILE: &str = "startup-error.log";

/// Keeps file logging alive. Dropping it flushes the pending lines and stops
/// logging, so the caller must hold it until the process exits.
pub struct FileLogGuard {
    _guard: WorkerGuard,
}

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Installs a console subscriber as the global default.
pub fn init_console() {
    tracing_subscriber::fmt().with_env_filter(env_filter()).init();
}

/// Builds a non-blocking writer that appends to a daily-rotated log file in
/// `dir`, keeping the last [`KEPT_LOG_FILES`] files. The returned guard must
/// outlive the writer, or buffered lines are lost.
pub fn file_writer(dir: &Path) -> Result<(NonBlocking, WorkerGuard)> {
    fs::create_dir_all(dir).with_context(|| format!("cannot create log directory {}", dir.display()))?;
    // The builder defaults to never rotating, so daily rotation has to be asked for.
    let appender: RollingFileAppender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_PREFIX)
        .filename_suffix(LOG_SUFFIX)
        .max_log_files(KEPT_LOG_FILES)
        .build(dir)
        .with_context(|| format!("cannot open a log file in {}", dir.display()))?;
    Ok(tracing_appender::non_blocking(appender))
}

/// Installs a global subscriber that writes to daily log files in `dir`.
/// Fails if a global subscriber is already installed.
pub fn init_file(dir: &Path) -> Result<FileLogGuard> {
    let (writer, guard) = file_writer(dir)?;
    tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .with_ansi(false)
        .with_writer(writer)
        .try_init()
        .map_err(|err| anyhow!("cannot install the file log subscriber: {err}"))?;
    Ok(FileLogGuard { _guard: guard })
}

/// Appends one timestamped line to `startup-error.log` in `dir`, for failures
/// that happen before logging is up. Best effort: I/O errors are ignored, so
/// it never panics and never fails the caller.
pub fn emergency(dir: &Path, message: &str) {
    // Never creates `dir`: a directory made here would carry inherited permissions instead of
    // the restricted ones, so the caller must have created (and secured) it first.
    let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(EMERGENCY_FILE))
    else {
        return;
    };
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let _ = writeln!(file, "{timestamp} {message}");
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use chrono::DateTime;

    use super::*;

    #[test]
    fn file_writer_creates_directory_and_writes_rolling_file() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("logs").join("nested");

        let (mut writer, guard) = file_writer(&dir).unwrap();
        writer.write_all(b"hello from the test\n").unwrap();
        // Dropping the guard flushes the background thread.
        drop(writer);
        drop(guard);

        let logs: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                let name = path.file_name().unwrap().to_string_lossy();
                name.starts_with(LOG_PREFIX) && name.ends_with(".log")
            })
            .collect();
        assert_eq!(logs.len(), 1, "expected one log file, found {logs:?}");
        let text = fs::read_to_string(&logs[0]).unwrap();
        assert!(text.contains("hello from the test"), "log file was {text:?}");
    }

    #[test]
    fn emergency_creates_file_and_appends_on_each_call() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("data");
        fs::create_dir(&dir).unwrap();

        emergency(&dir, "first failure");
        emergency(&dir, "second failure");

        let text = fs::read_to_string(dir.join(EMERGENCY_FILE)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "file was {text:?}");
        let (timestamp, message) = lines[0].split_once(' ').unwrap();
        assert!(
            DateTime::parse_from_rfc3339(timestamp).is_ok(),
            "bad timestamp {timestamp:?}"
        );
        assert_eq!(message, "first failure");
        assert!(lines[1].ends_with(" second failure"));
    }

    #[test]
    fn emergency_never_creates_the_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("data");

        emergency(&dir, "nowhere to write");

        assert!(!dir.exists(), "a directory made here would carry inherited permissions");
    }

    #[test]
    fn emergency_does_not_panic_when_directory_is_impossible() {
        let root = tempfile::tempdir().unwrap();
        let blocker = root.path().join("blocker");
        fs::write(&blocker, "a regular file").unwrap();

        emergency(&blocker.join("nested"), "unreachable");

        assert!(!blocker.join("nested").exists());
    }
}
