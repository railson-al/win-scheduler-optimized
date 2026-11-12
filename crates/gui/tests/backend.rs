#![cfg(unix)]
//! The backend against a real engine behind a real IPC server (a Unix socket here; the scripts are
//! `sh`, hence unix-only). There is no window: the tests play the part of the UI by sending commands
//! and reading updates.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use scriptrunner_core::logs::LogKind;
use scriptrunner_core::{ExecutionId, ExecutionStatus, Settings, Task};
use scriptrunner_gui::backend::{
    Action, Backend, BackendConfig, Command, ConnectionState, Failure, LogUpdate, Reason, Update,
};
use scriptrunner_ipc::PROTOCOL_VERSION;
use scriptrunner_ipc::transport::{Endpoint, Listener};
use scriptrunner_service::engine::Engine;
use scriptrunner_service::ipc_server;
use scriptrunner_storage::Storage;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(20);

/// A daemon on its own runtime, so the tests themselves can stay synchronous like the UI.
struct Daemon {
    runtime: tokio::runtime::Runtime,
    shutdown: CancellationToken,
    server: JoinHandle<()>,
}

impl Daemon {
    fn start(dir: &Path) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let shutdown = CancellationToken::new();
        // The engine spawns tasks of its own, so it is built inside the runtime.
        let (engine, listener) = runtime.block_on(async {
            let storage = Storage::open(&dir.join("scriptrunner.db")).unwrap();
            let engine = Engine::new(storage, dir.to_path_buf()).unwrap();
            engine.startup().unwrap();
            let listener = Listener::bind(&Endpoint::for_data_dir(dir)).await.unwrap();
            (engine, listener)
        });
        let server = runtime.spawn(ipc_server::serve(engine, listener, shutdown.clone()));
        Self {
            runtime,
            shutdown,
            server,
        }
    }

    fn stop(self) {
        self.shutdown.cancel();
        self.runtime.block_on(self.server).unwrap();
    }
}

/// Short waits, so the tests do not sit through the real backoff.
fn fast() -> BackendConfig {
    BackendConfig {
        initial_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(200),
        call_timeout: Duration::from_secs(5),
        refresh_delay: Duration::from_millis(20),
        status_interval: Duration::from_secs(1),
        log_poll: Duration::from_millis(30),
    }
}

fn backend(dir: &Path) -> Backend {
    Backend::spawn(Endpoint::for_data_dir(dir), fast(), || {}).unwrap()
}

/// Reads updates until `pick` takes one, ignoring the others.
fn wait_for<T>(backend: &Backend, what: &str, mut pick: impl FnMut(Update) -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for {what}"));
        match backend.recv_timeout(left) {
            Some(update) => {
                if let Some(found) = pick(update) {
                    return found;
                }
            }
            None => panic!("timed out waiting for {what}"),
        }
    }
}

fn wait_connected(backend: &Backend) {
    wait_for(backend, "a connection", |update| {
        matches!(update, Update::Connection(ConnectionState::Connected { .. })).then_some(())
    });
}

fn script(dir: &Path, name: &str, body: &str) -> Task {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    Task::new(name, path)
}

/// Creates the task and waits until the daemon has it.
fn create(backend: &Backend, task: Task) -> Task {
    backend.send(Command::CreateTask(task));
    wait_for(backend, "the task to be saved", |update| match update {
        Update::TaskSaved(saved) => Some(saved),
        Update::Failed { action, failure } => panic!("{action:?} failed: {failure:?}"),
        _ => None,
    })
}

fn run(backend: &Backend, task: &Task) -> ExecutionId {
    backend.send(Command::RunNow(task.id.clone()));
    wait_for(backend, "the run to start", |update| match update {
        Update::Started { execution, .. } => Some(execution),
        Update::Failed { action, failure } => panic!("{action:?} failed: {failure:?}"),
        _ => None,
    })
}

fn wait_for_end(backend: &Backend, id: &ExecutionId) -> ExecutionStatus {
    wait_for(backend, "the execution to end", |update| match update {
        Update::ExecutionChanged(e) if &e.id == id && e.status.is_terminal() => Some(e.status),
        _ => None,
    })
}

#[test]
fn it_delivers_the_first_picture_on_connecting() {
    let dir = tempfile::tempdir().unwrap();
    let _daemon = Daemon::start(dir.path());
    let backend = backend(dir.path());

    let (version, protocol) = wait_for(&backend, "a connection", |update| match update {
        Update::Connection(ConnectionState::Connected { version, protocol }) => Some((version, protocol)),
        _ => None,
    });
    assert!(!version.is_empty());
    assert_eq!(protocol, PROTOCOL_VERSION);
    wait_for(&backend, "the daemon status", |update| {
        matches!(update, Update::Status(_)).then_some(())
    });
    let rows = wait_for(&backend, "the overview", |update| match update {
        Update::Overview(rows) => Some(rows),
        _ => None,
    });
    assert!(rows.is_empty());
    let settings = wait_for(&backend, "the settings", |update| match update {
        Update::Settings(settings) => Some(settings),
        _ => None,
    });
    assert_eq!(settings, Settings::default());
}

#[test]
fn a_new_task_appears_in_the_overview_without_asking() {
    let dir = tempfile::tempdir().unwrap();
    let _daemon = Daemon::start(dir.path());
    let backend = backend(dir.path());
    wait_connected(&backend);

    let task = create(&backend, script(dir.path(), "hello.sh", "echo hi"));
    // Nobody sent `Refresh`: the daemon's `TasksChanged` event is what brings the new row.
    let rows = wait_for(&backend, "the overview with the task", |update| match update {
        Update::Overview(rows) if rows.iter().any(|row| row.task.id == task.id) => Some(rows),
        _ => None,
    });
    assert!(rows.iter().all(|row| !row.running));
}

#[test]
fn a_run_is_reported_from_start_to_overview() {
    let dir = tempfile::tempdir().unwrap();
    let _daemon = Daemon::start(dir.path());
    let backend = backend(dir.path());
    wait_connected(&backend);
    let task = create(&backend, script(dir.path(), "ok.sh", "echo done"));

    let id = run(&backend, &task);
    assert_eq!(wait_for_end(&backend, &id), ExecutionStatus::Success);
    wait_for(&backend, "the overview to show the result", |update| match update {
        Update::Overview(rows) => rows
            .iter()
            .any(|row| {
                let succeeded = row
                    .last_execution
                    .as_ref()
                    .is_some_and(|e| e.status == ExecutionStatus::Success);
                !row.running && row.task.id == task.id && succeeded
            })
            .then_some(()),
        _ => None,
    });
}

#[test]
fn following_a_log_streams_the_lines_as_they_are_written() {
    let dir = tempfile::tempdir().unwrap();
    let _daemon = Daemon::start(dir.path());
    let backend = backend(dir.path());
    wait_connected(&backend);
    let task = create(
        &backend,
        script(
            dir.path(),
            "slow.sh",
            "echo one\nsleep 0.4\necho two\nsleep 0.4\necho three",
        ),
    );
    let id = run(&backend, &task);
    backend.send(Command::FollowLog {
        id: id.clone(),
        offset: 0,
    });

    let mut chunks = 0;
    let mut lines = Vec::new();
    let mut meta = Vec::new();
    let mut last_offset = 0;
    wait_for(&backend, "the log to finish", |update| match update {
        Update::Log(LogUpdate::Chunk { id: from, chunk }) if from == id => {
            chunks += 1;
            assert!(chunk.next_offset > last_offset, "offsets only move forward");
            last_offset = chunk.next_offset;
            for entry in chunk.entries {
                match entry.kind {
                    LogKind::Stdout => lines.push(entry.text),
                    // The daemon's own "started" and "finished" lines frame the script's output.
                    LogKind::Meta => meta.push(entry.text),
                    LogKind::Stderr => panic!("unexpected stderr line {:?}", entry.text),
                }
            }
            None
        }
        Update::Log(LogUpdate::Finished { id: from }) if from == id => Some(()),
        Update::Log(LogUpdate::Failed { failure, .. }) => panic!("following failed: {failure:?}"),
        _ => None,
    });
    assert_eq!(lines, ["one", "two", "three"]);
    // The last line is written after the script ends: it only arrives if following keeps reading
    // until the execution is over, instead of stopping at the first "caught up".
    assert!(
        meta.last().is_some_and(|line| line.starts_with("finished:")),
        "{meta:?}"
    );
    assert!(
        chunks >= 2,
        "the lines arrived while the script ran, not all at the end ({chunks} chunk)"
    );
}

#[test]
fn a_run_can_be_stopped_while_its_log_is_followed() {
    let dir = tempfile::tempdir().unwrap();
    let _daemon = Daemon::start(dir.path());
    let backend = backend(dir.path());
    wait_connected(&backend);
    let task = create(&backend, script(dir.path(), "long.sh", "echo started\nsleep 60"));
    let id = run(&backend, &task);
    backend.send(Command::FollowLog {
        id: id.clone(),
        offset: 0,
    });
    wait_for(&backend, "the first line", |update| match update {
        Update::Log(LogUpdate::Chunk { chunk, .. }) => chunk.entries.iter().any(|e| e.text == "started").then_some(()),
        _ => None,
    });

    backend.send(Command::Stop(task.id.clone()));
    let stopped = wait_for(&backend, "the stop to be confirmed", |update| match update {
        Update::Stopped { count, .. } => Some(count),
        _ => None,
    });
    assert_eq!(stopped, 1);
    assert_eq!(wait_for_end(&backend, &id), ExecutionStatus::Interrupted);
    wait_for(&backend, "the log to finish", |update| {
        matches!(update, Update::Log(LogUpdate::Finished { .. })).then_some(())
    });
}

#[test]
fn a_log_is_exported_to_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let _daemon = Daemon::start(dir.path());
    let backend = backend(dir.path());
    wait_connected(&backend);
    let task = create(&backend, script(dir.path(), "talk.sh", "echo alpha\necho beta"));
    let id = run(&backend, &task);
    wait_for_end(&backend, &id);

    let path: PathBuf = dir.path().join("exported.log");
    backend.send(Command::ExportLog {
        id: id.clone(),
        path: path.clone(),
    });
    let bytes = wait_for(&backend, "the export", |update| match update {
        Update::Exported { bytes, .. } => Some(bytes),
        Update::Failed { action, failure } => panic!("{action:?} failed: {failure:?}"),
        _ => None,
    });
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(bytes, text.len() as u64);
    assert!(text.contains("alpha") && text.contains("beta"), "{text}");
}

#[test]
fn settings_are_saved_and_read_back_from_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let _daemon = Daemon::start(dir.path());
    let backend = backend(dir.path());
    wait_connected(&backend);

    let settings = Settings {
        max_concurrent_executions: 7,
        ..Settings::default()
    };
    backend.send(Command::UpdateSettings(settings.clone()));
    wait_for(&backend, "the save", |update| {
        matches!(update, Update::Succeeded(Action::UpdateSettings)).then_some(())
    });
    let saved = wait_for(&backend, "the saved settings", |update| match update {
        Update::Settings(s) if s.max_concurrent_executions == 7 => Some(s),
        _ => None,
    });
    assert_eq!(saved, settings);

    // The daemon's own validation comes back as a failure, not a crash.
    backend.send(Command::UpdateSettings(Settings {
        max_concurrent_executions: 0,
        ..Settings::default()
    }));
    let failure = wait_for(&backend, "the refusal", |update| match update {
        Update::Failed {
            action: Action::UpdateSettings,
            failure,
        } => Some(failure),
        _ => None,
    });
    assert!(matches!(failure, Failure::Remote { .. }), "{failure:?}");
}

#[test]
fn without_a_daemon_it_says_so_retries_with_growing_waits_and_refuses_commands() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend(dir.path());

    let mut waits = Vec::new();
    wait_for(&backend, "four failed attempts", |update| {
        if let Update::Connection(ConnectionState::Down { reason, retry_in }) = update {
            assert_eq!(reason, Reason::NotRunning);
            waits.push(retry_in);
        }
        (waits.len() == 4).then_some(())
    });
    assert_eq!(
        waits,
        [50, 100, 200, 200].map(Duration::from_millis),
        "the wait doubles up to the maximum"
    );

    backend.send(Command::RunNow("nope".into()));
    let (action, failure) = wait_for(&backend, "the refusal", |update| match update {
        Update::Failed { action, failure } => Some((action, failure)),
        _ => None,
    });
    assert_eq!(action, Action::RunNow);
    assert_eq!(failure, Failure::NotConnected);
}

#[test]
fn it_connects_by_itself_when_the_daemon_shows_up_later() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend(dir.path());
    wait_for(&backend, "the first failed attempt", |update| {
        matches!(update, Update::Connection(ConnectionState::Down { .. })).then_some(())
    });

    let _daemon = Daemon::start(dir.path());
    wait_connected(&backend);
    wait_for(&backend, "the overview", |update| {
        matches!(update, Update::Overview(_)).then_some(())
    });
}

#[test]
fn a_restarted_daemon_is_picked_up_again_with_its_data() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(dir.path());
    let backend = backend(dir.path());
    wait_connected(&backend);
    let task = create(&backend, script(dir.path(), "keep.sh", "echo kept"));

    daemon.stop();
    wait_for(&backend, "the connection to be reported lost", |update| match update {
        Update::Connection(ConnectionState::Down { reason, .. }) => {
            assert_eq!(reason, Reason::Lost);
            Some(())
        }
        _ => None,
    });

    let _daemon = Daemon::start(dir.path());
    wait_connected(&backend);
    wait_for(&backend, "the overview after the restart", |update| match update {
        Update::Overview(rows) => rows.iter().any(|row| row.task.id == task.id).then_some(()),
        _ => None,
    });
}

#[test]
fn sending_never_blocks_even_without_a_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend(dir.path());
    let started = Instant::now();
    for _ in 0..1_000 {
        backend.send(Command::Refresh);
    }
    assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
    // Dropping it must not hang either.
    drop(backend);
}
