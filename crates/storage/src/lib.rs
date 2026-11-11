//! SQLite persistence: tasks, schedules, execution history and settings.
//!
//! All timestamps are stored as RFC 3339 UTC text with millisecond precision
//! (`2025-01-01T10:00:00.000Z`), which sorts correctly as plain text (RN14).
//! Script output is *not* stored here; executions only reference a log file.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Row, ToSql, params};
use scriptrunner_core::{
    CoreError, Execution, ExecutionId, ExecutionStatus, Schedule, ScriptKind, Settings, Task, TaskId, Trigger,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("corrupt data: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// Each entry migrates the schema from version `index` to `index + 1`.
const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE tasks (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL,
    description      TEXT,
    script_path      TEXT NOT NULL,
    script_kind      TEXT NOT NULL,
    working_dir      TEXT,
    args_json        TEXT NOT NULL,
    interpreter      TEXT,
    timeout_secs     INTEGER,
    allow_concurrent INTEGER NOT NULL,
    capture_logs     INTEGER NOT NULL,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL
);
CREATE TABLE schedules (
    task_id     TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    spec_json   TEXT NOT NULL,
    enabled     INTEGER NOT NULL,
    next_run_at TEXT
);
CREATE INDEX idx_schedules_next ON schedules(next_run_at) WHERE enabled = 1;
CREATE TABLE executions (
    id             TEXT PRIMARY KEY,
    task_id        TEXT NOT NULL,
    task_name      TEXT NOT NULL,
    trigger        TEXT NOT NULL,
    scheduled_for  TEXT,
    created_at     TEXT NOT NULL,
    started_at     TEXT,
    finished_at    TEXT,
    status         TEXT NOT NULL,
    exit_code      INTEGER,
    failure_reason TEXT,
    log_file       TEXT,
    log_bytes      INTEGER NOT NULL DEFAULT 0,
    log_truncated  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_exec_task ON executions(task_id, created_at DESC);
CREATE INDEX idx_exec_created ON executions(created_at);
CREATE INDEX idx_exec_status ON executions(status);
CREATE TABLE settings (
    key        TEXT PRIMARY KEY,
    value_json TEXT NOT NULL
);
"#];

#[derive(Debug, Clone, Default)]
pub struct ExecutionQuery {
    pub task_id: Option<TaskId>,
    pub status: Option<ExecutionStatus>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: Option<u32>,
    pub offset: u32,
}

/// A schedule's stored next slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledSlot {
    pub task_id: TaskId,
    pub next_run_at: DateTime<Utc>,
}

pub struct Storage {
    // rusqlite connections are not `Sync`. Queries here are short, so one
    // mutex-guarded connection is simpler and plenty fast for this workload.
    conn: Mutex<Connection>,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StorageError::Corrupt(e.to_string()))?;
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // WAL + NORMAL survives process crashes without corruption and keeps
        // writes cheap; foreign keys are off by default in SQLite.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        let storage = Self { conn: Mutex::new(conn) };
        storage.migrate()?;
        Ok(storage)
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock cannot leave the database itself inconsistent.
        self.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn migrate(&self) -> Result<()> {
        let mut conn = self.db();
        let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (index, sql) in MIGRATIONS.iter().enumerate().skip(current.max(0) as usize) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.execute_batch(&format!("PRAGMA user_version = {}", index + 1))?;
            tx.commit()?;
        }
        Ok(())
    }

    // ---- tasks ---------------------------------------------------------

    /// Inserts the task and its schedule. `next_run` is the first slot
    /// computed by the caller (`None` for manual-only tasks).
    pub fn insert_task(&self, task: &Task, next_run: Option<DateTime<Utc>>) -> Result<()> {
        task.validate()?;
        let mut conn = self.db();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO tasks (id, name, description, script_path, script_kind, working_dir,
                                args_json, interpreter, timeout_secs, allow_concurrent,
                                capture_logs, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                task.id.as_str(),
                task.name,
                task.description,
                path_text(&task.script_path),
                task.script_kind.as_str(),
                task.working_dir.as_deref().map(path_text),
                serde_json::to_string(&task.args)?,
                task.interpreter.as_deref().map(path_text),
                task.timeout_secs.map(|s| s as i64),
                task.allow_concurrent,
                task.capture_logs,
                ts(task.created_at),
                ts(task.updated_at),
            ],
        )?;
        write_schedule(&tx, task, next_run)?;
        tx.commit()?;
        Ok(())
    }

    pub fn update_task(&self, task: &Task, next_run: Option<DateTime<Utc>>) -> Result<()> {
        task.validate()?;
        let mut conn = self.db();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE tasks SET name=?2, description=?3, script_path=?4, script_kind=?5,
                    working_dir=?6, args_json=?7, interpreter=?8, timeout_secs=?9,
                    allow_concurrent=?10, capture_logs=?11, updated_at=?12
             WHERE id=?1",
            params![
                task.id.as_str(),
                task.name,
                task.description,
                path_text(&task.script_path),
                task.script_kind.as_str(),
                task.working_dir.as_deref().map(path_text),
                serde_json::to_string(&task.args)?,
                task.interpreter.as_deref().map(path_text),
                task.timeout_secs.map(|s| s as i64),
                task.allow_concurrent,
                task.capture_logs,
                ts(task.updated_at),
            ],
        )?;
        if changed == 0 {
            return Err(StorageError::NotFound(format!("task {}", task.id)));
        }
        write_schedule(&tx, task, next_run)?;
        tx.commit()?;
        Ok(())
    }

    /// Deletes the task and its schedule. Its execution history is kept,
    /// since each record carries the task name as it was at the time.
    pub fn delete_task(&self, id: &TaskId) -> Result<bool> {
        let changed = self.db().execute("DELETE FROM tasks WHERE id = ?1", [id.as_str()])?;
        Ok(changed > 0)
    }

    pub fn get_task(&self, id: &TaskId) -> Result<Option<Task>> {
        let conn = self.db();
        conn.query_row(&format!("{TASK_SELECT} WHERE t.id = ?1"), [id.as_str()], task_from_row)
            .optional()?
            .transpose()
    }

    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        let conn = self.db();
        let mut stmt = conn.prepare(&format!("{TASK_SELECT} ORDER BY t.name COLLATE NOCASE, t.id"))?;
        let rows = stmt.query_map([], task_from_row)?;
        rows.map(|row| row?).collect()
    }

    // ---- scheduling ----------------------------------------------------

    pub fn next_run_of(&self, id: &TaskId) -> Result<Option<DateTime<Utc>>> {
        let conn = self.db();
        let text: Option<Option<String>> = conn
            .query_row(
                "SELECT next_run_at FROM schedules WHERE task_id = ?1",
                [id.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        text.flatten().map(|t| parse_ts(&t)).transpose()
    }

    /// Enabled schedules that have a next slot, earliest first.
    pub fn list_scheduled(&self) -> Result<Vec<ScheduledSlot>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT task_id, next_run_at FROM schedules
             WHERE enabled = 1 AND next_run_at IS NOT NULL ORDER BY next_run_at",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (id, at) = row?;
            Ok(ScheduledSlot {
                task_id: TaskId::from(id),
                next_run_at: parse_ts(&at)?,
            })
        })
        .collect()
    }

    /// Overwrites a schedule's next slot (used when skipping missed runs).
    pub fn set_next_run(&self, id: &TaskId, next_run: Option<DateTime<Utc>>) -> Result<()> {
        self.db().execute(
            "UPDATE schedules SET next_run_at = ?2 WHERE task_id = ?1",
            params![id.as_str(), next_run.map(ts)],
        )?;
        Ok(())
    }

    /// Atomically advances a schedule from `expected` to `new_next` and
    /// records the execution for that slot. Returns `false` (and writes
    /// nothing) if the slot was already claimed or changed — this is what
    /// prevents duplicate runs across restarts and concurrent callers.
    pub fn claim_scheduled_run(
        &self,
        id: &TaskId,
        expected: DateTime<Utc>,
        new_next: Option<DateTime<Utc>>,
        execution: &Execution,
    ) -> Result<bool> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE schedules SET next_run_at = ?3
             WHERE task_id = ?1 AND next_run_at = ?2 AND enabled = 1",
            params![id.as_str(), ts(expected), new_next.map(ts)],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        insert_execution_tx(&tx, execution)?;
        tx.commit()?;
        Ok(true)
    }

    // ---- executions ----------------------------------------------------

    pub fn insert_execution(&self, execution: &Execution) -> Result<()> {
        insert_execution_tx(&self.db(), execution)
    }

    pub fn update_execution(&self, e: &Execution) -> Result<()> {
        let changed = self.db().execute(
            "UPDATE executions SET started_at=?2, finished_at=?3, status=?4, exit_code=?5,
                    failure_reason=?6, log_file=?7, log_bytes=?8, log_truncated=?9
             WHERE id=?1",
            params![
                e.id.as_str(),
                e.started_at.map(ts),
                e.finished_at.map(ts),
                e.status.as_str(),
                e.exit_code,
                e.failure_reason,
                e.log_file,
                e.log_bytes as i64,
                e.log_truncated,
            ],
        )?;
        if changed == 0 {
            return Err(StorageError::NotFound(format!("execution {}", e.id)));
        }
        Ok(())
    }

    pub fn get_execution(&self, id: &ExecutionId) -> Result<Option<Execution>> {
        let conn = self.db();
        conn.query_row(
            &format!("{EXEC_SELECT} WHERE id = ?1"),
            [id.as_str()],
            execution_from_row,
        )
        .optional()?
        .transpose()
    }

    /// Newest first.
    pub fn list_executions(&self, q: &ExecutionQuery) -> Result<Vec<Execution>> {
        let mut sql = format!("{EXEC_SELECT} WHERE 1=1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(task_id) = &q.task_id {
            args.push(task_id.as_str().to_owned().into());
            sql.push_str(&format!(" AND task_id = ?{}", args.len()));
        }
        if let Some(status) = q.status {
            args.push(status.as_str().to_owned().into());
            sql.push_str(&format!(" AND status = ?{}", args.len()));
        }
        if let Some(since) = q.since {
            args.push(ts(since).into());
            sql.push_str(&format!(" AND created_at >= ?{}", args.len()));
        }
        if let Some(until) = q.until {
            args.push(ts(until).into());
            sql.push_str(&format!(" AND created_at < ?{}", args.len()));
        }
        args.push(i64::from(q.limit.unwrap_or(500)).into());
        sql.push_str(&format!(" ORDER BY created_at DESC, id LIMIT ?{}", args.len()));
        args.push(i64::from(q.offset).into());
        sql.push_str(&format!(" OFFSET ?{}", args.len()));

        let conn = self.db();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args), execution_from_row)?;
        rows.map(|row| row?).collect()
    }

    /// RN13: runs still marked queued/running when the service starts were
    /// cut short by a crash or restart. Marks them interrupted; returns how many.
    pub fn reconcile_orphaned_executions(&self, now: DateTime<Utc>) -> Result<usize> {
        Ok(self.db().execute(
            "UPDATE executions
             SET status = 'interrupted', finished_at = ?1,
                 failure_reason = 'service stopped while this run was in progress'
             WHERE status IN ('queued', 'running')",
            [ts(now)],
        )?)
    }

    /// Deletes the history of one task; returns the log files to remove from disk.
    pub fn clear_task_history(&self, id: &TaskId) -> Result<Vec<String>> {
        self.delete_executions(
            "task_id = ?1 AND status NOT IN ('queued','running')",
            params![id.as_str()],
        )
    }

    /// Retention (RN11): deletes finished executions created before `cutoff`.
    pub fn prune_history_before(&self, cutoff: DateTime<Utc>) -> Result<Vec<String>> {
        self.delete_executions(
            "created_at < ?1 AND status NOT IN ('queued','running')",
            params![ts(cutoff)],
        )
    }

    /// Deletes the matching rows and returns their log files, atomically.
    fn delete_executions(&self, filter: &str, params: &[&dyn ToSql]) -> Result<Vec<String>> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        let files = {
            let mut stmt = tx.prepare(&format!(
                "SELECT log_file FROM executions WHERE {filter} AND log_file IS NOT NULL"
            ))?;
            let rows = stmt.query_map(params, |r| r.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        tx.execute(&format!("DELETE FROM executions WHERE {filter}"), params)?;
        tx.commit()?;
        Ok(files)
    }

    /// Total bytes of logs currently referenced by the history.
    pub fn total_log_bytes(&self) -> Result<u64> {
        let total: i64 = self.db().query_row(
            "SELECT COALESCE(SUM(log_bytes), 0) FROM executions WHERE log_file IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(total.max(0) as u64)
    }

    /// Total-size policy: drops the oldest finished logs until the total is
    /// within `max_total_bytes`. History rows stay (without a log reference).
    /// Returns the log files to delete from disk.
    pub fn trim_logs_to_budget(&self, max_total_bytes: u64) -> Result<Vec<String>> {
        let mut total = self.total_log_bytes()?;
        let mut removed = Vec::new();
        if total <= max_total_bytes {
            return Ok(removed);
        }
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, log_file, log_bytes FROM executions
             WHERE log_file IS NOT NULL AND status NOT IN ('queued','running')
             ORDER BY created_at ASC",
        )?;
        let candidates = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (id, file, bytes) in candidates {
            if total <= max_total_bytes {
                break;
            }
            conn.execute(
                "UPDATE executions SET log_file = NULL, log_bytes = 0 WHERE id = ?1",
                [&id],
            )?;
            total = total.saturating_sub(bytes.max(0) as u64);
            removed.push(file);
        }
        Ok(removed)
    }

    // ---- settings ------------------------------------------------------

    pub fn load_settings(&self) -> Result<Settings> {
        let conn = self.db();
        let json: Option<String> = conn
            .query_row("SELECT value_json FROM settings WHERE key = 'global'", [], |r| r.get(0))
            .optional()?;
        match json {
            Some(json) => Ok(serde_json::from_str(&json)?),
            None => Ok(Settings::default()),
        }
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        settings.validate()?;
        self.db().execute(
            "INSERT INTO settings (key, value_json) VALUES ('global', ?1)
             ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
            [serde_json::to_string(settings)?],
        )?;
        Ok(())
    }
}

const TASK_SELECT: &str = "SELECT t.id, t.name, t.description, t.script_path, t.script_kind,
        t.working_dir, t.args_json, t.interpreter, t.timeout_secs, t.allow_concurrent,
        t.capture_logs, t.created_at, t.updated_at, s.spec_json, s.enabled
    FROM tasks t LEFT JOIN schedules s ON s.task_id = t.id";

const EXEC_SELECT: &str = "SELECT id, task_id, task_name, trigger, scheduled_for, created_at,
        started_at, finished_at, status, exit_code, failure_reason, log_file, log_bytes,
        log_truncated FROM executions";

fn write_schedule(tx: &rusqlite::Transaction<'_>, task: &Task, next_run: Option<DateTime<Utc>>) -> Result<()> {
    match &task.schedule {
        Some(schedule) => {
            tx.execute(
                "INSERT INTO schedules (task_id, spec_json, enabled, next_run_at) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(task_id) DO UPDATE SET spec_json = excluded.spec_json,
                     enabled = excluded.enabled, next_run_at = excluded.next_run_at",
                params![
                    task.id.as_str(),
                    serde_json::to_string(schedule)?,
                    task.schedule_enabled,
                    next_run.map(ts),
                ],
            )?;
        }
        None => {
            tx.execute("DELETE FROM schedules WHERE task_id = ?1", [task.id.as_str()])?;
        }
    }
    Ok(())
}

fn insert_execution_tx(conn: &Connection, e: &Execution) -> Result<()> {
    conn.execute(
        "INSERT INTO executions (id, task_id, task_name, trigger, scheduled_for, created_at,
                started_at, finished_at, status, exit_code, failure_reason, log_file,
                log_bytes, log_truncated)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        params![
            e.id.as_str(),
            e.task_id.as_str(),
            e.task_name,
            e.trigger.as_str(),
            e.scheduled_for.map(ts),
            ts(e.created_at),
            e.started_at.map(ts),
            e.finished_at.map(ts),
            e.status.as_str(),
            e.exit_code,
            e.failure_reason,
            e.log_file,
            e.log_bytes as i64,
            e.log_truncated,
        ],
    )?;
    Ok(())
}

fn task_from_row(r: &Row<'_>) -> rusqlite::Result<Result<Task>> {
    // Column reads fail with rusqlite errors; semantic decoding errors travel
    // in the inner Result so callers can tell them apart.
    let id: String = r.get(0)?;
    let name: String = r.get(1)?;
    let description: Option<String> = r.get(2)?;
    let script_path: String = r.get(3)?;
    let script_kind: String = r.get(4)?;
    let working_dir: Option<String> = r.get(5)?;
    let args_json: String = r.get(6)?;
    let interpreter: Option<String> = r.get(7)?;
    let timeout_secs: Option<i64> = r.get(8)?;
    let allow_concurrent: bool = r.get(9)?;
    let capture_logs: bool = r.get(10)?;
    let created_at: String = r.get(11)?;
    let updated_at: String = r.get(12)?;
    let spec_json: Option<String> = r.get(13)?;
    let enabled: Option<bool> = r.get(14)?;

    Ok((|| {
        Ok(Task {
            id: TaskId::from(id),
            name,
            description,
            script_path: PathBuf::from(script_path),
            script_kind: ScriptKind::from_str(&script_kind)?,
            working_dir: working_dir.map(PathBuf::from),
            args: serde_json::from_str(&args_json)?,
            interpreter: interpreter.map(PathBuf::from),
            schedule: spec_json.map(|j| serde_json::from_str::<Schedule>(&j)).transpose()?,
            schedule_enabled: enabled.unwrap_or(true),
            timeout_secs: timeout_secs.map(|s| s.max(0) as u64),
            allow_concurrent,
            capture_logs,
            created_at: parse_ts(&created_at)?,
            updated_at: parse_ts(&updated_at)?,
        })
    })())
}

fn execution_from_row(r: &Row<'_>) -> rusqlite::Result<Result<Execution>> {
    let id: String = r.get(0)?;
    let task_id: String = r.get(1)?;
    let task_name: String = r.get(2)?;
    let trigger: String = r.get(3)?;
    let scheduled_for: Option<String> = r.get(4)?;
    let created_at: String = r.get(5)?;
    let started_at: Option<String> = r.get(6)?;
    let finished_at: Option<String> = r.get(7)?;
    let status: String = r.get(8)?;
    let exit_code: Option<i32> = r.get(9)?;
    let failure_reason: Option<String> = r.get(10)?;
    let log_file: Option<String> = r.get(11)?;
    let log_bytes: i64 = r.get(12)?;
    let log_truncated: bool = r.get(13)?;

    Ok((|| {
        Ok(Execution {
            id: ExecutionId::from(id),
            task_id: TaskId::from(task_id),
            task_name,
            trigger: Trigger::from_str(&trigger)?,
            scheduled_for: scheduled_for.as_deref().map(parse_ts).transpose()?,
            created_at: parse_ts(&created_at)?,
            started_at: started_at.as_deref().map(parse_ts).transpose()?,
            finished_at: finished_at.as_deref().map(parse_ts).transpose()?,
            status: ExecutionStatus::from_str(&status)?,
            exit_code,
            failure_reason,
            log_file,
            log_bytes: log_bytes.max(0) as u64,
            log_truncated,
        })
    })())
}

fn ts(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_ts(text: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| StorageError::Corrupt(format!("bad timestamp {text:?}: {e}")))
}

/// Paths are stored as text; a non-UTF-8 path would be altered, which Windows
/// paths in practice never are.
fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};
    use scriptrunner_core::Schedule;

    use super::*;

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2025, 6, 9, h, m, 0).unwrap()
    }

    fn scheduled_task(name: &str) -> Task {
        let mut t = Task::new(name, "/jobs/a.py");
        t.args = vec!["--flag".into(), "with space".into()];
        t.schedule = Some(Schedule::every_minutes(10, at(0, 0)));
        t
    }

    fn finished(task: &Task, status: ExecutionStatus, created: DateTime<Utc>, log: Option<(&str, u64)>) -> Execution {
        let mut e = Execution::new(task, Trigger::Manual, None);
        e.created_at = created;
        e.status = status;
        if let Some((file, bytes)) = log {
            e.log_file = Some(file.to_owned());
            e.log_bytes = bytes;
        }
        e
    }

    #[test]
    fn task_round_trips_with_its_schedule() {
        let db = Storage::open_in_memory().unwrap();
        let mut task = scheduled_task("Backup");
        task.description = Some("nightly".into());
        task.timeout_secs = Some(30);
        task.interpreter = Some("/venv/bin/python".into());
        task.schedule_enabled = false;
        db.insert_task(&task, Some(at(0, 10))).unwrap();

        let loaded = db.get_task(&task.id).unwrap().unwrap();
        assert_eq!(loaded.name, task.name);
        assert_eq!(loaded.args, task.args);
        assert_eq!(loaded.schedule, task.schedule);
        assert!(!loaded.schedule_enabled);
        assert_eq!(loaded.timeout_secs, Some(30));
        assert_eq!(db.next_run_of(&task.id).unwrap(), Some(at(0, 10)));
    }

    #[test]
    fn manual_only_task_has_no_schedule_row_and_updates_cleanly() {
        let db = Storage::open_in_memory().unwrap();
        let mut task = Task::new("Manual", "/jobs/a.py");
        db.insert_task(&task, None).unwrap();
        assert_eq!(db.get_task(&task.id).unwrap().unwrap().schedule, None);

        task.name = "Renamed".into();
        task.schedule = Some(Schedule::Daily {
            time: chrono::NaiveTime::from_hms_opt(8, 0, 0).unwrap(),
        });
        db.update_task(&task, Some(at(8, 0))).unwrap();
        let loaded = db.get_task(&task.id).unwrap().unwrap();
        assert_eq!(loaded.name, "Renamed");
        assert!(loaded.schedule.is_some());

        task.schedule = None;
        db.update_task(&task, None).unwrap();
        assert_eq!(db.get_task(&task.id).unwrap().unwrap().schedule, None);
        assert!(db.update_task(&Task::new("ghost", "x.py"), None).is_err());
    }

    #[test]
    fn list_is_sorted_by_name_and_delete_keeps_history() {
        let db = Storage::open_in_memory().unwrap();
        let b = scheduled_task("beta");
        let a = scheduled_task("Alpha");
        db.insert_task(&b, Some(at(1, 0))).unwrap();
        db.insert_task(&a, Some(at(1, 0))).unwrap();
        let names: Vec<_> = db.list_tasks().unwrap().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["Alpha", "beta"]);

        db.insert_execution(&finished(&a, ExecutionStatus::Success, at(2, 0), None))
            .unwrap();
        assert!(db.delete_task(&a.id).unwrap());
        assert!(db.get_task(&a.id).unwrap().is_none());
        assert!(db.list_scheduled().unwrap().iter().all(|s| s.task_id != a.id));
        let history = db
            .list_executions(&ExecutionQuery {
                task_id: Some(a.id.clone()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].task_name, "Alpha");
    }

    #[test]
    fn a_scheduled_slot_can_be_claimed_only_once() {
        let db = Storage::open_in_memory().unwrap();
        let task = scheduled_task("Job");
        db.insert_task(&task, Some(at(0, 10))).unwrap();

        let first = Execution::new(&task, Trigger::Scheduled, Some(at(0, 10)));
        assert!(
            db.claim_scheduled_run(&task.id, at(0, 10), Some(at(0, 20)), &first)
                .unwrap()
        );
        assert_eq!(db.next_run_of(&task.id).unwrap(), Some(at(0, 20)));

        // Same slot again (e.g. a restart racing the old process): refused, nothing written.
        let dup = Execution::new(&task, Trigger::Scheduled, Some(at(0, 10)));
        assert!(
            !db.claim_scheduled_run(&task.id, at(0, 10), Some(at(0, 20)), &dup)
                .unwrap()
        );
        assert!(db.get_execution(&dup.id).unwrap().is_none());
        assert_eq!(db.list_executions(&ExecutionQuery::default()).unwrap().len(), 1);
    }

    #[test]
    fn disabled_schedules_are_not_listed_or_claimable() {
        let db = Storage::open_in_memory().unwrap();
        let mut task = scheduled_task("Paused");
        task.schedule_enabled = false;
        db.insert_task(&task, Some(at(0, 10))).unwrap();
        assert!(db.list_scheduled().unwrap().is_empty());
        let e = Execution::new(&task, Trigger::Scheduled, Some(at(0, 10)));
        assert!(!db.claim_scheduled_run(&task.id, at(0, 10), None, &e).unwrap());
    }

    #[test]
    fn execution_filters_and_paging() {
        let db = Storage::open_in_memory().unwrap();
        let t1 = scheduled_task("one");
        let t2 = scheduled_task("two");
        for (i, status) in [
            ExecutionStatus::Success,
            ExecutionStatus::Failed,
            ExecutionStatus::Success,
        ]
        .into_iter()
        .enumerate()
        {
            db.insert_execution(&finished(&t1, status, at(1, i as u32), None))
                .unwrap();
        }
        db.insert_execution(&finished(&t2, ExecutionStatus::Failed, at(1, 30), None))
            .unwrap();

        let by_task = ExecutionQuery {
            task_id: Some(t1.id.clone()),
            ..Default::default()
        };
        assert_eq!(db.list_executions(&by_task).unwrap().len(), 3);
        let failed = ExecutionQuery {
            status: Some(ExecutionStatus::Failed),
            ..Default::default()
        };
        assert_eq!(db.list_executions(&failed).unwrap().len(), 2);
        let window = ExecutionQuery {
            since: Some(at(1, 1)),
            until: Some(at(1, 30)),
            ..Default::default()
        };
        assert_eq!(db.list_executions(&window).unwrap().len(), 2); // 1:01 and 1:02
        let page = ExecutionQuery {
            limit: Some(2),
            offset: 1,
            ..Default::default()
        };
        let rows = db.list_executions(&page).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].created_at >= rows[1].created_at); // newest first
    }

    #[test]
    fn orphaned_runs_are_reconciled_after_a_crash() {
        let db = Storage::open_in_memory().unwrap();
        let task = scheduled_task("Job");
        let running = finished(&task, ExecutionStatus::Running, at(1, 0), None);
        let queued = finished(&task, ExecutionStatus::Queued, at(1, 1), None);
        let done = finished(&task, ExecutionStatus::Success, at(1, 2), None);
        for e in [&running, &queued, &done] {
            db.insert_execution(e).unwrap();
        }
        assert_eq!(db.reconcile_orphaned_executions(at(2, 0)).unwrap(), 2);
        let r = db.get_execution(&running.id).unwrap().unwrap();
        assert_eq!(r.status, ExecutionStatus::Interrupted);
        assert_eq!(r.finished_at, Some(at(2, 0)));
        assert!(r.failure_reason.unwrap().contains("service stopped"));
        assert_eq!(
            db.get_execution(&done.id).unwrap().unwrap().status,
            ExecutionStatus::Success
        );
    }

    #[test]
    fn retention_prunes_old_history_and_returns_log_files() {
        let db = Storage::open_in_memory().unwrap();
        let task = scheduled_task("Job");
        let old = finished(&task, ExecutionStatus::Success, at(1, 0), Some(("old.log", 10)));
        let recent = finished(&task, ExecutionStatus::Success, at(5, 0), Some(("recent.log", 10)));
        let in_flight = finished(&task, ExecutionStatus::Running, at(1, 0), Some(("live.log", 10)));
        for e in [&old, &recent, &in_flight] {
            db.insert_execution(e).unwrap();
        }
        assert_eq!(db.prune_history_before(at(3, 0)).unwrap(), ["old.log"]);
        assert!(db.get_execution(&old.id).unwrap().is_none());
        assert!(db.get_execution(&recent.id).unwrap().is_some());
        assert!(
            db.get_execution(&in_flight.id).unwrap().is_some(),
            "running executions are never pruned"
        );

        assert_eq!(db.clear_task_history(&task.id).unwrap(), ["recent.log"]);
        assert_eq!(db.list_executions(&ExecutionQuery::default()).unwrap().len(), 1);
    }

    #[test]
    fn log_budget_drops_the_oldest_logs_first() {
        let db = Storage::open_in_memory().unwrap();
        let task = scheduled_task("Job");
        let a = finished(&task, ExecutionStatus::Success, at(1, 0), Some(("a.log", 100)));
        let b = finished(&task, ExecutionStatus::Success, at(2, 0), Some(("b.log", 100)));
        let c = finished(&task, ExecutionStatus::Success, at(3, 0), Some(("c.log", 100)));
        for e in [&a, &b, &c] {
            db.insert_execution(e).unwrap();
        }
        assert_eq!(db.total_log_bytes().unwrap(), 300);
        assert_eq!(db.trim_logs_to_budget(150).unwrap(), ["a.log", "b.log"]);
        assert_eq!(db.total_log_bytes().unwrap(), 100);
        // History rows survive, just without a log reference.
        assert_eq!(db.get_execution(&a.id).unwrap().unwrap().log_file, None);
        assert!(db.trim_logs_to_budget(1_000).unwrap().is_empty());
    }

    #[test]
    fn settings_default_then_persist() {
        let db = Storage::open_in_memory().unwrap();
        assert_eq!(db.load_settings().unwrap(), Settings::default());
        let custom = Settings {
            max_concurrent_executions: 9,
            history_retention_days: 7,
            ..Settings::default()
        };
        db.save_settings(&custom).unwrap();
        assert_eq!(db.load_settings().unwrap(), custom);
        assert!(
            db.save_settings(&Settings {
                max_concurrent_executions: 0,
                ..Settings::default()
            })
            .is_err()
        );
    }

    #[test]
    fn data_survives_reopening_the_database_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("scriptrunner.db");
        let task = scheduled_task("Persistent");
        {
            let db = Storage::open(&path).unwrap();
            db.insert_task(&task, Some(at(0, 10))).unwrap();
            db.insert_execution(&finished(&task, ExecutionStatus::Success, at(1, 0), None))
                .unwrap();
        }
        let db = Storage::open(&path).unwrap(); // migrations must be idempotent
        assert_eq!(db.list_tasks().unwrap().len(), 1);
        assert_eq!(db.next_run_of(&task.id).unwrap(), Some(at(0, 10)));
        assert_eq!(db.list_executions(&ExecutionQuery::default()).unwrap().len(), 1);
    }

    #[test]
    fn timestamps_keep_millisecond_precision_in_utc() {
        let t = Utc.with_ymd_and_hms(2025, 1, 2, 3, 4, 5).unwrap() + Duration::milliseconds(678);
        assert_eq!(ts(t), "2025-01-02T03:04:05.678Z");
        assert_eq!(parse_ts(&ts(t)).unwrap(), t);
        assert!(parse_ts("yesterday").is_err());
    }
}
