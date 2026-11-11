use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
use scriptrunner_core::executor::{self, LogLine, RunOutcome};
use scriptrunner_core::logs::{self, LogEntry, LogWriter};
use scriptrunner_core::{
    CoreError, Execution, ExecutionId, ExecutionStatus, Settings, Task, TaskId, Trigger,
};
use scriptrunner_storage::{ExecutionQuery, ScheduledSlot, Storage, StorageError};
use thiserror::Error;
use tokio::sync::{Notify, Semaphore, SemaphorePermit, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// The scheduler never sleeps longer than this, so edits made by another
/// process and wall-clock jumps (suspend/resume, NTP) are noticed promptly.
const MAX_SLEEP: Duration = Duration::from_secs(30);

/// A slot more than this late is a *missed* run (machine off, service down,
/// laptop asleep) and is recorded instead of executed (RF03 / RN08).
const MISSED_GRACE: chrono::Duration = chrono::Duration::seconds(60);

const HOUSEKEEPING_EVERY: Duration = Duration::from_secs(3600);

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error("task not found: {0}")]
    TaskNotFound(String),
    #[error("task {0} is already running")]
    AlreadyRunning(String),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, EngineError>;

#[derive(Debug, Clone)]
pub enum Event {
    TasksChanged,
    /// Fired when an execution is registered, starts, or finishes.
    ExecutionChanged(Execution),
}

/// One dashboard row.
#[derive(Debug, Clone)]
pub struct TaskOverview {
    pub task: Task,
    pub running: bool,
    pub last_execution: Option<Execution>,
    pub next_run: Option<DateTime<Utc>>,
}

struct ActiveRun {
    task_id: TaskId,
    cancel: CancellationToken,
}

/// Bookkeeping for the global execution slots. `total` is how many permits
/// exist (free or held by running executions); `target` is the configured
/// limit. While `total > target` (the limit was lowered with every slot busy),
/// permits are retired as they are released.
struct SlotState {
    total: u32,
    target: u32,
}

/// A held execution slot; on release it is either returned to the semaphore or
/// retired, if the limit was lowered in the meantime.
struct SlotPermit<'a> {
    inner: &'a Inner,
    permit: Option<SemaphorePermit<'a>>,
}

impl Drop for SlotPermit<'_> {
    fn drop(&mut self) {
        let Some(permit) = self.permit.take() else { return };
        let mut state = self.inner.slot_state.lock().unwrap_or_else(|p| p.into_inner());
        if state.total > state.target {
            state.total -= 1;
            permit.forget();
        }
        // otherwise `permit` is dropped here and goes back to the semaphore
    }
}

struct Inner {
    storage: Storage,
    logs_dir: PathBuf,
    active: Mutex<HashMap<ExecutionId, ActiveRun>>,
    slots: Semaphore,
    slot_state: Mutex<SlotState>,
    wake: Notify,
    events: broadcast::Sender<Event>,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl Engine {
    /// `data_dir` receives the log files; the database is whatever `storage` wraps.
    pub fn new(storage: Storage, data_dir: PathBuf) -> Result<Self> {
        let logs_dir = data_dir.join("logs");
        std::fs::create_dir_all(&logs_dir)?;
        let settings = storage.load_settings()?;
        let (events, _) = broadcast::channel(256);
        Ok(Self {
            inner: Arc::new(Inner {
                storage,
                logs_dir,
                active: Mutex::new(HashMap::new()),
                slots: Semaphore::new(settings.max_concurrent_executions as usize),
                slot_state: Mutex::new(SlotState {
                    total: settings.max_concurrent_executions,
                    target: settings.max_concurrent_executions,
                }),
                wake: Notify::new(),
                events,
            }),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Run once when the service starts, before the scheduler loop.
    pub fn startup(&self) -> Result<()> {
        let orphans = self.inner.storage.reconcile_orphaned_executions(Utc::now())?;
        if orphans > 0 {
            warn!(orphans, "marked executions left over from a previous run as interrupted");
        }
        self.housekeeping()
    }

    // ---- task management ----------------------------------------------

    pub fn create_task(&self, task: Task) -> Result<Task> {
        task.validate()?;
        self.inner.storage.insert_task(&task, next_run_for(&task, Utc::now()))?;
        self.changed();
        Ok(task)
    }

    pub fn update_task(&self, mut task: Task) -> Result<Task> {
        task.validate()?;
        task.updated_at = Utc::now();
        self.inner.storage.update_task(&task, next_run_for(&task, Utc::now()))?;
        self.changed();
        Ok(task)
    }

    pub fn delete_task(&self, id: &TaskId) -> Result<bool> {
        let deleted = self.inner.storage.delete_task(id)?;
        if deleted {
            self.changed();
        }
        Ok(deleted)
    }

    pub fn duplicate_task(&self, id: &TaskId) -> Result<Task> {
        let copy = self.get_task(id)?.duplicate();
        self.create_task(copy)
    }

    /// Pauses/resumes the schedule. Manual runs stay possible either way (RN02).
    pub fn set_schedule_enabled(&self, id: &TaskId, enabled: bool) -> Result<Task> {
        let mut task = self.get_task(id)?;
        task.schedule_enabled = enabled;
        self.update_task(task)
    }

    pub fn get_task(&self, id: &TaskId) -> Result<Task> {
        self.inner
            .storage
            .get_task(id)?
            .ok_or_else(|| EngineError::TaskNotFound(id.to_string()))
    }

    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        Ok(self.inner.storage.list_tasks()?)
    }

    pub fn overview(&self) -> Result<Vec<TaskOverview>> {
        let mut rows = Vec::new();
        for task in self.inner.storage.list_tasks()? {
            let last_execution = self
                .inner
                .storage
                .list_executions(&ExecutionQuery { task_id: Some(task.id.clone()), limit: Some(1), ..Default::default() })?
                .into_iter()
                .next();
            rows.push(TaskOverview {
                running: self.is_running(&task.id),
                next_run: self.inner.storage.next_run_of(&task.id)?,
                last_execution,
                task,
            });
        }
        Ok(rows)
    }

    // ---- executions ----------------------------------------------------

    /// Starts a manual run. Returns `AlreadyRunning` (and records a skipped
    /// attempt) if overlap is not allowed for this task.
    pub fn run_now(&self, id: &TaskId) -> Result<ExecutionId> {
        let task = self.get_task(id)?;
        let (execution, reserved) = self.prepare(&task, Trigger::Manual, None);
        if let Err(err) = self.inner.storage.insert_execution(&execution) {
            if reserved {
                self.active().remove(&execution.id);
            }
            return Err(err.into());
        }
        self.emit(Event::ExecutionChanged(execution.clone()));
        if reserved {
            self.launch(task, execution.clone());
            Ok(execution.id)
        } else {
            Err(EngineError::AlreadyRunning(task.name))
        }
    }

    /// Cancels every running or queued execution of the task; returns how many.
    pub fn stop(&self, id: &TaskId) -> usize {
        let active = self.active();
        let mut count = 0;
        for run in active.values().filter(|run| &run.task_id == id) {
            run.cancel.cancel();
            count += 1;
        }
        count
    }

    pub fn is_running(&self, id: &TaskId) -> bool {
        self.active().values().any(|run| &run.task_id == id)
    }

    pub fn executions(&self, query: &ExecutionQuery) -> Result<Vec<Execution>> {
        Ok(self.inner.storage.list_executions(query)?)
    }

    pub fn get_execution(&self, id: &ExecutionId) -> Result<Option<Execution>> {
        Ok(self.inner.storage.get_execution(id)?)
    }

    pub fn clear_history(&self, id: &TaskId) -> Result<()> {
        let files = self.inner.storage.clear_task_history(id)?;
        self.remove_log_files(&files);
        Ok(())
    }

    /// Reads a page of log entries (works while the run is still going).
    pub fn read_log(&self, id: &ExecutionId, skip: usize, limit: usize) -> Result<Vec<LogEntry>> {
        let Some(file) = self.log_file_of(id)? else { return Ok(Vec::new()) };
        Ok(logs::read_log(&self.inner.logs_dir.join(file), skip, limit)?)
    }

    pub fn export_log(&self, id: &ExecutionId) -> Result<String> {
        let Some(file) = self.log_file_of(id)? else { return Ok(String::new()) };
        Ok(logs::export_text(&self.inner.logs_dir.join(file))?)
    }

    fn log_file_of(&self, id: &ExecutionId) -> Result<Option<String>> {
        Ok(self.inner.storage.get_execution(id)?.and_then(|e| e.log_file))
    }

    // ---- settings ------------------------------------------------------

    pub fn settings(&self) -> Result<Settings> {
        Ok(self.inner.storage.load_settings()?)
    }

    /// Applies immediately; the concurrency limit follows without a restart
    /// (a *lowered* limit takes effect as running executions finish).
    pub fn update_settings(&self, settings: Settings) -> Result<()> {
        self.inner.storage.save_settings(&settings)?;
        let mut state = self.inner.slot_state.lock().unwrap_or_else(|p| p.into_inner());
        let wanted = settings.max_concurrent_executions;
        state.target = wanted;
        if wanted > state.total {
            self.inner.slots.add_permits((wanted - state.total) as usize);
            state.total = wanted;
        } else if wanted < state.total {
            // Only idle permits can be removed now; the excess held by running
            // executions is retired by `SlotPermit::drop` as they finish.
            let forgotten = self.inner.slots.forget_permits((state.total - wanted) as usize);
            state.total -= forgotten as u32;
        }
        Ok(())
    }

    // ---- scheduler -----------------------------------------------------

    /// Runs until `shutdown` is cancelled.
    pub async fn run_scheduler(&self, shutdown: CancellationToken) {
        let mut last_housekeeping = Instant::now();
        loop {
            let sleep_for = match self.fire_due(Utc::now()) {
                Ok(next) => next
                    .map(|at| (at - Utc::now()).to_std().unwrap_or(Duration::ZERO))
                    .map_or(MAX_SLEEP, |wait| wait.min(MAX_SLEEP)),
                Err(err) => {
                    error!(%err, "scheduler pass failed");
                    MAX_SLEEP
                }
            };
            if last_housekeeping.elapsed() >= HOUSEKEEPING_EVERY {
                if let Err(err) = self.housekeeping() {
                    error!(%err, "housekeeping failed");
                }
                last_housekeeping = Instant::now();
            }
            tokio::select! {
                _ = tokio::time::sleep(sleep_for) => {}
                _ = self.inner.wake.notified() => debug!("schedule changed, re-evaluating"),
                _ = shutdown.cancelled() => break,
            }
        }
    }

    /// Fires every due slot; returns the earliest remaining one.
    fn fire_due(&self, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        loop {
            let slots = self.inner.storage.list_scheduled()?;
            let Some(first) = slots.first() else { return Ok(None) };
            if first.next_run_at > now {
                return Ok(Some(first.next_run_at));
            }
            // Each pass either claims the slot (advancing it into the future)
            // or fails because someone else did; both make progress.
            self.fire(first.clone(), now)?;
        }
    }

    fn fire(&self, slot: ScheduledSlot, now: DateTime<Utc>) -> Result<()> {
        let Some(task) = self.inner.storage.get_task(&slot.task_id)? else {
            // Row vanished between listing and loading: nothing left to fire.
            self.inner.storage.set_next_run(&slot.task_id, None)?;
            return Ok(());
        };
        let Some(schedule) = &task.schedule else {
            self.inner.storage.set_next_run(&task.id, None)?;
            return Ok(());
        };

        let missed = now - slot.next_run_at > MISSED_GRACE;
        let from = if missed { now } else { slot.next_run_at.max(now) };
        let new_next = schedule.next_after(from, &Local);

        let (execution, reserved) = if missed {
            let mut skipped = Execution::new(&task, Trigger::Scheduled, Some(slot.next_run_at));
            finish_as_skipped(&mut skipped, "missed: the service or computer was not running at the scheduled time");
            (skipped, false)
        } else {
            self.prepare(&task, Trigger::Scheduled, Some(slot.next_run_at))
        };

        let claimed =
            self.inner.storage.claim_scheduled_run(&task.id, slot.next_run_at, new_next, &execution)?;
        if !claimed {
            if reserved {
                self.active().remove(&execution.id);
            }
            return Ok(());
        }
        self.emit(Event::ExecutionChanged(execution.clone()));
        if reserved {
            self.launch(task, execution);
        } else {
            info!(task = %execution.task_name, reason = ?execution.failure_reason, "scheduled run skipped");
        }
        Ok(())
    }

    // ---- execution plumbing -------------------------------------------

    /// Builds the execution record, deciding overlap. When it may run, the
    /// task is marked active *inside the same lock* so two triggers can never
    /// both pass the overlap check. Returns `(execution, reserved)`.
    fn prepare(&self, task: &Task, trigger: Trigger, scheduled_for: Option<DateTime<Utc>>) -> (Execution, bool) {
        let mut execution = Execution::new(task, trigger, scheduled_for);
        let mut active = self.active();
        if !task.allow_concurrent && active.values().any(|run| run.task_id == task.id) {
            finish_as_skipped(&mut execution, "skipped: the previous run of this task is still in progress");
            return (execution, false);
        }
        active.insert(execution.id.clone(), ActiveRun { task_id: task.id.clone(), cancel: CancellationToken::new() });
        (execution, true)
    }

    fn launch(&self, task: Task, execution: Execution) {
        let cancel = match self.active().get(&execution.id) {
            Some(run) => run.cancel.clone(),
            None => return,
        };
        let engine = self.clone();
        tokio::spawn(async move { engine.run_execution(task, execution, cancel).await });
    }

    async fn run_execution(&self, task: Task, mut execution: Execution, cancel: CancellationToken) {
        // Wait for a free global slot; the run counts as "active" (so overlap
        // rules hold) while it waits.
        let permit = tokio::select! {
            permit = self.inner.slots.acquire() => permit.ok(),
            _ = cancel.cancelled() => None,
        };
        let Some(_slot) = permit.map(|permit| SlotPermit { inner: &self.inner, permit: Some(permit) }) else {
            execution.status = ExecutionStatus::Interrupted;
            execution.failure_reason = Some("stopped while waiting for a free execution slot".into());
            execution.finished_at = Some(Utc::now());
            self.finish(&execution);
            return;
        };

        let settings = self.inner.storage.load_settings().unwrap_or_else(|err| {
            warn!(%err, "could not load settings, using defaults");
            Settings::default()
        });
        let plan = executor::plan_command(&task, &settings);

        let mut writer = None;
        if task.capture_logs {
            let file = format!("{}.log", execution.id);
            match LogWriter::create(&self.inner.logs_dir.join(&file), settings.max_log_bytes_per_execution) {
                Ok(mut w) => {
                    let args: Vec<_> = plan.args.iter().map(|a| a.to_string_lossy()).collect();
                    let _ = w.write_meta(&format!("started: {} {}", plan.program.to_string_lossy(), args.join(" ")));
                    execution.log_file = Some(file);
                    writer = Some(w);
                }
                Err(err) => warn!(%err, "could not create the log file; output will not be kept"),
            }
        }

        execution.status = ExecutionStatus::Running;
        execution.started_at = Some(Utc::now());
        self.persist(&execution);
        self.emit(Event::ExecutionChanged(execution.clone()));

        let (sink, consumer) = match writer {
            Some(mut w) => {
                let (tx, mut rx) = mpsc::channel::<LogLine>(256);
                let handle = tokio::spawn(async move {
                    while let Some(line) = rx.recv().await {
                        let _ = w.write_line(&line);
                        while let Ok(more) = rx.try_recv() {
                            let _ = w.write_line(&more);
                        }
                        let _ = w.flush(); // keeps the log readable while the script runs
                    }
                    w
                });
                (Some(tx), Some(handle))
            }
            None => (None, None),
        };

        let timeout = task.timeout_secs.or(settings.default_timeout_secs).map(Duration::from_secs);
        let outcome = match executor::spawn(&plan, sink) {
            Ok(process) => process.wait(timeout, &cancel).await,
            Err(err) => RunOutcome {
                status: ExecutionStatus::Failed,
                exit_code: None,
                failure_reason: Some(format!("failed to start {:?}: {err}", plan.program)),
            },
        };

        execution.finished_at = Some(Utc::now());
        execution.status = outcome.status;
        execution.exit_code = outcome.exit_code;
        execution.failure_reason = outcome.failure_reason;
        if let Some(handle) = consumer
            && let Ok(mut w) = handle.await
        {
            let seconds = execution.duration().map_or(0.0, |d| d.num_milliseconds() as f64 / 1000.0);
            let _ = w.write_meta(&format!(
                "finished: {} (exit code {}, {seconds:.1}s){}",
                execution.status,
                execution.exit_code.map_or_else(|| "n/a".to_owned(), |c| c.to_string()),
                execution.failure_reason.as_deref().map(|r| format!(" - {r}")).unwrap_or_default(),
            ));
            let _ = w.flush();
            execution.log_bytes = w.bytes_written();
            execution.log_truncated = w.truncated();
        }
        self.finish(&execution);
    }

    fn persist(&self, execution: &Execution) {
        if let Err(err) = self.inner.storage.update_execution(execution) {
            error!(%err, execution = %execution.id, "could not persist execution state");
        }
    }

    fn finish(&self, execution: &Execution) {
        self.persist(execution);
        self.active().remove(&execution.id);
        self.emit(Event::ExecutionChanged(execution.clone()));
    }

    /// Stops everything that is running and waits (bounded) for it to wind down.
    pub async fn shutdown(&self) {
        for run in self.active().values() {
            run.cancel.cancel();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.active().is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // ---- housekeeping --------------------------------------------------

    /// Applies the retention and log-size policies.
    pub fn housekeeping(&self) -> Result<()> {
        let settings = self.inner.storage.load_settings()?;
        let mut files = Vec::new();
        if settings.history_retention_days > 0 {
            let cutoff = Utc::now() - chrono::Duration::days(i64::from(settings.history_retention_days));
            files.extend(self.inner.storage.prune_history_before(cutoff)?);
        }
        files.extend(self.inner.storage.trim_logs_to_budget(settings.max_total_log_bytes)?);
        self.remove_log_files(&files);
        Ok(())
    }

    fn remove_log_files(&self, files: &[String]) {
        for file in files {
            if let Err(err) = std::fs::remove_file(self.inner.logs_dir.join(file))
                && err.kind() != std::io::ErrorKind::NotFound
            {
                warn!(%err, file, "could not delete log file");
            }
        }
    }

    // ---- small helpers -------------------------------------------------

    fn active(&self) -> MutexGuard<'_, HashMap<ExecutionId, ActiveRun>> {
        self.inner.active.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn emit(&self, event: Event) {
        let _ = self.inner.events.send(event); // no subscribers is fine
    }

    /// Tells the scheduler loop to re-read the schedules, and listeners to refresh.
    fn changed(&self) {
        self.inner.wake.notify_one();
        self.emit(Event::TasksChanged);
    }
}

fn next_run_for(task: &Task, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if !task.schedule_enabled {
        return None;
    }
    task.schedule.as_ref()?.next_after(from, &Local)
}

fn finish_as_skipped(execution: &mut Execution, reason: &str) {
    execution.status = ExecutionStatus::Skipped;
    execution.failure_reason = Some(reason.to_owned());
    execution.finished_at = Some(Utc::now());
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use chrono::Duration as ChronoDuration;
    use scriptrunner_core::Schedule;
    use scriptrunner_core::logs::LogKind;
    use tempfile::TempDir;

    use super::*;

    struct Fixture {
        dir: TempDir,
        engine: Engine,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("db.sqlite")).unwrap();
        let engine = Engine::new(storage, dir.path().to_path_buf()).unwrap();
        Fixture { dir, engine }
    }

    impl Fixture {
        fn script(&self, name: &str, body: &str) -> Task {
            let path = self.dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            Task::new(name, path)
        }

        fn reopen(&self) -> Engine {
            let storage = Storage::open(&self.dir.path().join("db.sqlite")).unwrap();
            Engine::new(storage, self.dir.path().to_path_buf()).unwrap()
        }

        fn start_scheduler(&self) -> (CancellationToken, tokio::task::JoinHandle<()>) {
            let shutdown = CancellationToken::new();
            let handle = tokio::spawn({
                let engine = self.engine.clone();
                let shutdown = shutdown.clone();
                async move { engine.run_scheduler(shutdown).await }
            });
            (shutdown, handle)
        }
    }

    async fn wait_until(mut check: impl FnMut() -> bool, what: &str) {
        for _ in 0..200 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    async fn finished(engine: &Engine, id: &ExecutionId) -> Execution {
        wait_until(
            || engine.get_execution(id).unwrap().is_some_and(|e| e.status.is_terminal()),
            "execution to finish",
        )
        .await;
        engine.get_execution(id).unwrap().unwrap()
    }

    #[tokio::test]
    async fn manual_run_records_history_and_logs() {
        let fx = fixture();
        let task = fx.engine.create_task(fx.script("ok.sh", "echo hello; echo oops >&2")).unwrap();
        let id = fx.engine.run_now(&task.id).unwrap();
        let exec = finished(&fx.engine, &id).await;

        assert_eq!(exec.status, ExecutionStatus::Success);
        assert_eq!(exec.exit_code, Some(0));
        assert_eq!(exec.trigger, Trigger::Manual);
        assert!(exec.started_at.is_some() && exec.finished_at.is_some());

        let entries = fx.engine.read_log(&id, 0, 100).unwrap();
        assert!(entries.iter().any(|e| e.kind == LogKind::Stdout && e.text == "hello"));
        assert!(entries.iter().any(|e| e.kind == LogKind::Stderr && e.text == "oops"));
        assert!(entries.first().unwrap().text.starts_with("started:"));
        assert!(entries.last().unwrap().text.starts_with("finished: success"));
        assert!(fx.engine.export_log(&id).unwrap().contains("[ERR] oops"));
    }

    #[tokio::test]
    async fn failures_are_recorded_with_code_and_reason() {
        let fx = fixture();
        let task = fx.engine.create_task(fx.script("fail.sh", "exit 7")).unwrap();
        let exec = finished(&fx.engine, &fx.engine.run_now(&task.id).unwrap()).await;
        assert_eq!(exec.status, ExecutionStatus::Failed);
        assert_eq!(exec.exit_code, Some(7));

        let ghost = fx.engine.create_task(Task::new("ghost", fx.dir.path().join("missing.sh"))).unwrap();
        let exec = finished(&fx.engine, &fx.engine.run_now(&ghost.id).unwrap()).await;
        assert_eq!(exec.status, ExecutionStatus::Failed);
        assert!(exec.failure_reason.unwrap().contains("failed to start"));
    }

    #[tokio::test]
    async fn overlap_is_refused_recorded_and_stop_interrupts() {
        let fx = fixture();
        let task = fx.engine.create_task(fx.script("slow.sh", "sleep 30")).unwrap();
        let first = fx.engine.run_now(&task.id).unwrap();
        wait_until(|| fx.engine.get_execution(&first).unwrap().unwrap().status == ExecutionStatus::Running, "running").await;

        assert!(matches!(fx.engine.run_now(&task.id), Err(EngineError::AlreadyRunning(_))));
        let history = fx.engine.executions(&ExecutionQuery { task_id: Some(task.id.clone()), ..Default::default() }).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history.iter().filter(|e| e.status == ExecutionStatus::Skipped).count(), 1);

        assert_eq!(fx.engine.stop(&task.id), 1);
        let exec = finished(&fx.engine, &first).await;
        assert_eq!(exec.status, ExecutionStatus::Interrupted);
        wait_until(|| !fx.engine.is_running(&task.id), "task to become idle").await;
        // Idle again: a new run is accepted.
        assert!(fx.engine.run_now(&task.id).is_ok());
        fx.engine.shutdown().await;
    }

    #[tokio::test]
    async fn concurrent_runs_are_allowed_when_configured() {
        let fx = fixture();
        let mut task = fx.script("slow.sh", "sleep 30");
        task.allow_concurrent = true;
        let task = fx.engine.create_task(task).unwrap();
        fx.engine.run_now(&task.id).unwrap();
        fx.engine.run_now(&task.id).unwrap();
        assert_eq!(fx.engine.stop(&task.id), 2);
        fx.engine.shutdown().await;
    }

    #[tokio::test]
    async fn timeout_fails_the_run() {
        let fx = fixture();
        let mut task = fx.script("hang.sh", "sleep 30");
        task.timeout_secs = Some(1);
        let task = fx.engine.create_task(task).unwrap();
        let exec = finished(&fx.engine, &fx.engine.run_now(&task.id).unwrap()).await;
        assert_eq!(exec.status, ExecutionStatus::Failed);
        assert_eq!(exec.failure_reason.as_deref(), Some("timed out after 1s"));
    }

    #[tokio::test]
    async fn global_concurrency_limit_queues_extra_runs() {
        let fx = fixture();
        fx.engine.update_settings(Settings { max_concurrent_executions: 1, ..Settings::default() }).unwrap();
        let a = fx.engine.create_task(fx.script("a.sh", "sleep 30")).unwrap();
        let b = fx.engine.create_task(fx.script("b.sh", "echo b")).unwrap();
        let first = fx.engine.run_now(&a.id).unwrap();
        wait_until(|| fx.engine.get_execution(&first).unwrap().unwrap().status == ExecutionStatus::Running, "a running").await;

        let second = fx.engine.run_now(&b.id).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(fx.engine.get_execution(&second).unwrap().unwrap().status, ExecutionStatus::Queued);

        fx.engine.stop(&a.id);
        assert_eq!(finished(&fx.engine, &second).await.status, ExecutionStatus::Success);
    }

    #[tokio::test]
    async fn lowering_the_limit_while_slots_are_busy_still_takes_effect() {
        let fx = fixture();
        fx.engine.update_settings(Settings { max_concurrent_executions: 2, ..Settings::default() }).unwrap();
        let a = fx.engine.create_task(fx.script("a.sh", "sleep 30")).unwrap();
        let b = fx.engine.create_task(fx.script("b.sh", "sleep 30")).unwrap();
        let ra = fx.engine.run_now(&a.id).unwrap();
        let rb = fx.engine.run_now(&b.id).unwrap();
        wait_until(
            || [&ra, &rb].iter().all(|id| fx.engine.get_execution(id).unwrap().unwrap().status == ExecutionStatus::Running),
            "both running",
        )
        .await;

        // Every permit is in use, so none can be forgotten immediately.
        fx.engine.update_settings(Settings { max_concurrent_executions: 1, ..Settings::default() }).unwrap();
        fx.engine.stop(&a.id);
        fx.engine.stop(&b.id);
        finished(&fx.engine, &ra).await;
        finished(&fx.engine, &rb).await;
        wait_until(|| !fx.engine.is_running(&a.id) && !fx.engine.is_running(&b.id), "idle").await;

        let c = fx.engine.create_task(fx.script("c.sh", "sleep 30")).unwrap();
        let d = fx.engine.create_task(fx.script("d.sh", "echo d")).unwrap();
        let rc = fx.engine.run_now(&c.id).unwrap();
        wait_until(|| fx.engine.get_execution(&rc).unwrap().unwrap().status == ExecutionStatus::Running, "c running").await;
        let rd = fx.engine.run_now(&d.id).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(fx.engine.get_execution(&rd).unwrap().unwrap().status, ExecutionStatus::Queued);

        fx.engine.stop(&c.id);
        assert_eq!(finished(&fx.engine, &rd).await.status, ExecutionStatus::Success);
    }

    #[tokio::test]
    async fn scheduler_fires_interval_tasks_without_duplicates() {
        let fx = fixture();
        let mut task = fx.script("tick.sh", "echo tick");
        task.schedule = Some(Schedule::Interval { every_secs: 1, anchor: Utc::now() });
        let task = fx.engine.create_task(task).unwrap();

        let (shutdown, scheduler) = fx.start_scheduler();
        tokio::time::sleep(Duration::from_millis(3600)).await;
        shutdown.cancel();
        scheduler.await.unwrap();
        fx.engine.shutdown().await;

        let runs = fx.engine.executions(&ExecutionQuery { task_id: Some(task.id.clone()), ..Default::default() }).unwrap();
        assert!((2..=4).contains(&runs.len()), "expected ~3 runs, got {}", runs.len());
        assert!(runs.iter().all(|r| r.trigger == Trigger::Scheduled));
        let mut slots: Vec<_> = runs.iter().map(|r| r.scheduled_for.unwrap()).collect();
        slots.sort();
        slots.dedup();
        assert_eq!(slots.len(), runs.len(), "each slot must run exactly once");
    }

    #[tokio::test]
    async fn disabled_schedules_do_not_fire_but_manual_runs_work() {
        let fx = fixture();
        let mut task = fx.script("tick.sh", "echo tick");
        task.schedule = Some(Schedule::Interval { every_secs: 1, anchor: Utc::now() });
        task.schedule_enabled = false;
        let task = fx.engine.create_task(task).unwrap();

        let (shutdown, scheduler) = fx.start_scheduler();
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert!(fx.engine.executions(&ExecutionQuery::default()).unwrap().is_empty());
        assert_eq!(finished(&fx.engine, &fx.engine.run_now(&task.id).unwrap()).await.status, ExecutionStatus::Success);

        // Enabling at runtime takes effect without restarting the scheduler.
        fx.engine.set_schedule_enabled(&task.id, true).unwrap();
        wait_until(
            || fx.engine.executions(&ExecutionQuery::default()).unwrap().iter().any(|e| e.trigger == Trigger::Scheduled),
            "a scheduled run after enabling",
        )
        .await;
        shutdown.cancel();
        scheduler.await.unwrap();
        fx.engine.shutdown().await;
    }

    #[tokio::test]
    async fn missed_slots_are_recorded_not_executed() {
        let fx = fixture();
        let mut task = fx.script("tick.sh", "echo tick");
        task.schedule = Some(Schedule::every_hours(1, Utc::now() - ChronoDuration::hours(5)));
        let task = fx.engine.create_task(task).unwrap();
        // Pretend the computer was off: the stored slot is 3 hours old.
        // (Truncated to ms: that is the precision the database keeps.)
        let stale = DateTime::from_timestamp_millis((Utc::now() - ChronoDuration::hours(3)).timestamp_millis()).unwrap();
        fx.engine.inner.storage.set_next_run(&task.id, Some(stale)).unwrap();

        let (shutdown, scheduler) = fx.start_scheduler();
        tokio::time::sleep(Duration::from_millis(500)).await;
        shutdown.cancel();
        scheduler.await.unwrap();

        let runs = fx.engine.executions(&ExecutionQuery::default()).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, ExecutionStatus::Skipped);
        assert!(runs[0].failure_reason.as_deref().unwrap().starts_with("missed"));
        assert_eq!(runs[0].scheduled_for, Some(stale));
        let next = fx.engine.inner.storage.next_run_of(&task.id).unwrap().unwrap();
        assert!(next > Utc::now(), "the schedule must move on to the future");
    }

    #[tokio::test]
    async fn restart_reconciles_orphaned_runs() {
        let fx = fixture();
        let task = fx.engine.create_task(fx.script("slow.sh", "sleep 30")).unwrap();
        // Simulate a crash mid-run: the row says running, but nobody owns it.
        let mut orphan = Execution::new(&task, Trigger::Manual, None);
        orphan.status = ExecutionStatus::Running;
        fx.engine.inner.storage.insert_execution(&orphan).unwrap();

        let restarted = fx.reopen();
        restarted.startup().unwrap();
        let after = restarted.get_execution(&orphan.id).unwrap().unwrap();
        assert_eq!(after.status, ExecutionStatus::Interrupted);
        assert!(after.failure_reason.unwrap().contains("service stopped"));
        assert!(!restarted.is_running(&task.id));
    }

    #[tokio::test]
    async fn housekeeping_applies_retention_and_deletes_log_files() {
        let fx = fixture();
        let task = fx.engine.create_task(fx.script("ok.sh", "echo hi")).unwrap();

        let mut old = Execution::new(&task, Trigger::Manual, None);
        old.status = ExecutionStatus::Success;
        old.created_at = Utc::now() - ChronoDuration::days(400);
        old.log_file = Some("old.log".into());
        old.log_bytes = 10;
        let log_path = fx.dir.path().join("logs").join("old.log");
        std::fs::write(&log_path, "x").unwrap();
        fx.engine.inner.storage.insert_execution(&old).unwrap();

        let recent = finished(&fx.engine, &fx.engine.run_now(&task.id).unwrap()).await;
        fx.engine.housekeeping().unwrap();

        assert!(fx.engine.get_execution(&old.id).unwrap().is_none(), "past retention");
        assert!(!log_path.exists(), "its log file is deleted too");
        assert!(fx.engine.get_execution(&recent.id).unwrap().is_some());
    }
}
