#![cfg(unix)]
//! End-to-end tests: a real engine behind a real IPC server, driven by real
//! clients over a real Unix socket. (Scripts are `sh`, hence unix-only; the
//! Windows named-pipe transport is covered by the `scriptrunner-ipc` tests.)

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use scriptrunner_core::logs::LogKind;
use scriptrunner_core::{Execution, ExecutionId, ExecutionQuery, ExecutionStatus, Settings, Task};
use scriptrunner_ipc::client::{Client, ClientError};
use scriptrunner_ipc::codec::{FrameReader, FrameWriter, frame_reader, frame_writer, read_frame, write_frame};
use scriptrunner_ipc::transport::{self, Endpoint, Listener};
use scriptrunner_ipc::{
    ClientFrame, ErrorCode, Event, MIN_PROTOCOL_VERSION, Outcome, PROTOCOL_VERSION, Request, ServerFrame,
};
use scriptrunner_service::engine::Engine;
use scriptrunner_service::ipc_server::{self, MAX_CONNECTIONS};
use scriptrunner_storage::Storage;
use tempfile::TempDir;
use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

struct Daemon {
    dir: TempDir,
    endpoint: Endpoint,
    shutdown: CancellationToken,
    server: JoinHandle<()>,
}

impl Daemon {
    async fn start() -> Self {
        Self::start_in(tempfile::tempdir().unwrap()).await
    }

    async fn start_in(dir: TempDir) -> Self {
        let storage = Storage::open(&dir.path().join("scriptrunner.db")).unwrap();
        let engine = Engine::new(storage, dir.path().to_path_buf()).unwrap();
        engine.startup().unwrap();
        let endpoint = Endpoint::for_data_dir(dir.path());
        let listener = Listener::bind(&endpoint).await.unwrap();
        let shutdown = CancellationToken::new();
        let server = tokio::spawn(ipc_server::serve(engine, listener, shutdown.clone()));
        Self {
            dir,
            endpoint,
            shutdown,
            server,
        }
    }

    async fn client(&self) -> Client {
        Client::connect(&self.endpoint).await.unwrap()
    }

    fn script(&self, name: &str, body: &str) -> Task {
        let path = self.dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Task::new(name, path)
    }

    /// Stops the server and hands back the data directory, for a restart.
    async fn stop(self) -> TempDir {
        self.shutdown.cancel();
        self.server.await.unwrap();
        self.dir
    }
}

fn code_of(err: ClientError) -> ErrorCode {
    match err {
        ClientError::Remote(body) => body.code,
        other => panic!("expected a daemon error, got {other:?}"),
    }
}

async fn wait_for_terminal(events: &mut broadcast::Receiver<Event>, id: &ExecutionId) -> Execution {
    timeout(Duration::from_secs(20), async {
        loop {
            match events.recv().await {
                Ok(Event::ExecutionChanged(e)) if &e.id == id && e.status.is_terminal() => return e,
                Ok(_) => {}
                Err(err) => panic!("the event stream ended: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for the execution to finish")
}

#[tokio::test]
async fn status_reports_version_and_protocol() {
    let daemon = Daemon::start().await;
    let status = daemon.client().await.status().await.unwrap();
    assert_eq!(status.protocol_version, PROTOCOL_VERSION);
    assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn task_crud_round_trips() {
    let daemon = Daemon::start().await;
    let client = daemon.client().await;

    let created = client.create_task(daemon.script("backup.sh", "true")).await.unwrap();
    assert_eq!(client.list_tasks().await.unwrap().len(), 1);
    assert_eq!(client.get_task(&created.id).await.unwrap().name, "backup.sh");

    let mut renamed = created.clone();
    renamed.name = "Backup".into();
    assert_eq!(client.update_task(renamed).await.unwrap().name, "Backup");

    let copy = client.duplicate_task(&created.id).await.unwrap();
    assert_ne!(copy.id, created.id);
    assert!(!copy.schedule_enabled, "a duplicate starts paused");

    let paused = client.set_schedule_enabled(&created.id, false).await.unwrap();
    assert!(!paused.schedule_enabled);

    let overview = client.overview().await.unwrap();
    assert_eq!(overview.len(), 2);

    assert!(client.delete_task(&created.id).await.unwrap());
    assert!(!client.delete_task(&created.id).await.unwrap());
    assert_eq!(
        code_of(client.get_task(&created.id).await.unwrap_err()),
        ErrorCode::NotFound
    );
}

#[tokio::test]
async fn invalid_input_is_reported_as_invalid() {
    let daemon = Daemon::start().await;
    let client = daemon.client().await;

    let nameless = Task::new("  ", "x.sh");
    assert_eq!(
        code_of(client.create_task(nameless).await.unwrap_err()),
        ErrorCode::Invalid
    );

    let settings = Settings {
        max_concurrent_executions: 0,
        ..Settings::default()
    };
    assert_eq!(
        code_of(client.update_settings(settings).await.unwrap_err()),
        ErrorCode::Invalid
    );
}

#[tokio::test]
async fn settings_round_trip() {
    let daemon = Daemon::start().await;
    let client = daemon.client().await;
    let settings = Settings {
        max_concurrent_executions: 2,
        history_retention_days: 7,
        ..Settings::default()
    };
    client.update_settings(settings.clone()).await.unwrap();
    assert_eq!(client.settings().await.unwrap(), settings);
}

#[tokio::test]
async fn a_run_streams_events_and_leaves_history_and_logs() {
    let daemon = Daemon::start().await;
    let client = daemon.client().await;
    let task = client
        .create_task(daemon.script("talk.sh", "echo hello; echo oops >&2"))
        .await
        .unwrap();

    let mut events = client.subscribe().await.unwrap();
    let id = client.run_now(&task.id).await.unwrap();
    let finished = wait_for_terminal(&mut events, &id).await;
    assert_eq!(finished.status, ExecutionStatus::Success);

    let entries = client.read_log(&id, 0, u32::MAX).await.unwrap();
    assert!(entries.iter().any(|e| e.kind == LogKind::Stdout && e.text == "hello"));
    assert!(entries.iter().any(|e| e.kind == LogKind::Stderr && e.text == "oops"));
    assert!(client.export_log(&id).await.unwrap().contains("hello"));

    let stored = client.get_execution(&id).await.unwrap().unwrap();
    assert_eq!(stored.status, ExecutionStatus::Success);
    let history = client
        .executions(ExecutionQuery {
            task_id: Some(task.id.clone()),
            ..ExecutionQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    let row = &client.overview().await.unwrap()[0];
    assert_eq!(row.last_execution.as_ref().map(|e| e.id.clone()), Some(id.clone()));

    client.clear_history(&task.id).await.unwrap();
    assert!(client.get_execution(&id).await.unwrap().is_none());
}

#[tokio::test]
async fn overlap_is_refused_and_stop_interrupts() {
    let daemon = Daemon::start().await;
    let client = daemon.client().await;
    let task = client.create_task(daemon.script("slow.sh", "sleep 30")).await.unwrap();

    let mut events = client.subscribe().await.unwrap();
    let id = client.run_now(&task.id).await.unwrap();
    assert_eq!(
        code_of(client.run_now(&task.id).await.unwrap_err()),
        ErrorCode::AlreadyRunning
    );

    assert_eq!(client.stop(&task.id).await.unwrap(), 1);
    let finished = wait_for_terminal(&mut events, &id).await;
    assert_eq!(finished.status, ExecutionStatus::Interrupted);
}

#[tokio::test]
async fn every_subscriber_sees_a_run_started_by_another_client() {
    let daemon = Daemon::start().await;
    let (watcher_a, watcher_b, runner) = (daemon.client().await, daemon.client().await, daemon.client().await);
    let task = runner.create_task(daemon.script("quick.sh", "true")).await.unwrap();

    let mut events_a = watcher_a.subscribe().await.unwrap();
    let mut events_b = watcher_b.subscribe().await.unwrap();
    let id = runner.run_now(&task.id).await.unwrap();

    assert_eq!(
        wait_for_terminal(&mut events_a, &id).await.status,
        ExecutionStatus::Success
    );
    assert_eq!(
        wait_for_terminal(&mut events_b, &id).await.status,
        ExecutionStatus::Success
    );
}

#[tokio::test]
async fn concurrent_calls_on_one_connection_are_correlated() {
    let daemon = Daemon::start().await;
    let client = daemon.client().await;
    let task = client.create_task(daemon.script("t.sh", "true")).await.unwrap();

    let calls = (0..20).map(|_| {
        let client = client.clone();
        let id = task.id.clone();
        tokio::spawn(async move { (client.get_task(&id).await.unwrap(), client.list_tasks().await.unwrap()) })
    });
    for call in calls {
        let (got, all) = call.await.unwrap();
        assert_eq!(got.id, task.id);
        assert_eq!(all.len(), 1);
    }
}

/// Runs a script that prints ~10 MB, more than one frame can carry once exported in one piece.
async fn run_big_script(daemon: &Daemon, client: &Client) -> ExecutionId {
    client
        .update_settings(Settings {
            max_log_bytes_per_execution: 64 * 1024 * 1024,
            ..Settings::default()
        })
        .await
        .unwrap();
    let task = client
        .create_task(daemon.script("big.sh", "head -c 10000000 /dev/zero | tr '\\0' x; echo"))
        .await
        .unwrap();
    let mut events = client.subscribe().await.unwrap();
    let id = client.run_now(&task.id).await.unwrap();
    wait_for_terminal(&mut events, &id).await;
    id
}

#[tokio::test]
async fn an_oversized_response_becomes_an_error_and_the_connection_survives() {
    let daemon = Daemon::start().await;
    let client = daemon.client().await;
    let id = run_big_script(&daemon, &client).await;

    // The version 1 way of exporting (one frame) still fails cleanly.
    let whole = client.call(Request::ExportLog { id }).await;
    assert_eq!(code_of(whole.unwrap_err()), ErrorCode::TooLarge);
    client.status().await.expect("the connection must still work");
}

#[tokio::test]
async fn a_log_larger_than_one_frame_exports_in_pages() {
    let daemon = Daemon::start().await;
    let client = daemon.client().await;
    let id = run_big_script(&daemon, &client).await;

    let text = client.export_log(&id).await.unwrap();
    assert!(text.len() > 10_000_000, "the export lost data: {} bytes", text.len());
    assert!(text.contains("[---] finished: success"), "the last line must arrive");
    assert!(
        text.starts_with(|c: char| c.is_ascii_digit()),
        "starts with a timestamp"
    );
}

#[tokio::test]
async fn a_running_log_can_be_followed_by_position() {
    let daemon = Daemon::start().await;
    let client = daemon.client().await;
    let task = client
        .create_task(daemon.script("slow.sh", "echo first; sleep 1; echo second; sleep 30"))
        .await
        .unwrap();
    let id = client.run_now(&task.id).await.unwrap();

    let mut offset = 0;
    let mut seen = Vec::new();
    timeout(Duration::from_secs(20), async {
        while !seen.iter().any(|t| t == "second") {
            let chunk = client.read_log_from(&id, offset, 4096).await.unwrap();
            assert!(chunk.next_offset >= offset);
            offset = chunk.next_offset;
            seen.extend(chunk.entries.into_iter().map(|e| e.text));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the live lines never arrived");
    let position = seen.iter().position(|t| t == "first").expect("first line");
    assert_eq!(seen.iter().filter(|t| *t == "first").count(), 1, "no line is repeated");
    assert!(position < seen.iter().position(|t| t == "second").unwrap());

    client.stop(&task.id).await.unwrap();
}

#[tokio::test]
async fn reading_the_log_of_an_unknown_execution_is_empty_not_an_error() {
    let daemon = Daemon::start().await;
    let client = daemon.client().await;
    let chunk = client.read_log_from(&"nope".into(), 0, 4096).await.unwrap();
    assert!(chunk.entries.is_empty() && chunk.eof && chunk.next_offset == 0);
}

#[tokio::test]
async fn the_event_stream_ends_when_the_daemon_goes_away() {
    let daemon = Daemon::start().await;
    let client = daemon.client().await;
    let mut events = client.subscribe().await.unwrap();

    daemon.stop().await;

    let closed = timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("the receiver must not wait forever on a dead connection");
    assert!(
        matches!(closed, Err(broadcast::error::RecvError::Closed)),
        "got {closed:?}"
    );
    // And a new subscription on the dead connection fails instead of hanging.
    assert!(client.subscribe().await.is_err());
}

#[tokio::test]
async fn a_missing_log_file_reads_as_empty() {
    let daemon = Daemon::start().await;
    let client = daemon.client().await;
    let task = client.create_task(daemon.script("gone.sh", "echo hi")).await.unwrap();
    let mut events = client.subscribe().await.unwrap();
    let id = client.run_now(&task.id).await.unwrap();
    wait_for_terminal(&mut events, &id).await;

    let logs_dir = daemon.dir.path().join("logs");
    for entry in std::fs::read_dir(&logs_dir).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    assert!(client.read_log(&id, 0, 100).await.unwrap().is_empty());
    assert!(client.export_log(&id).await.unwrap().is_empty());
}

type RawReader = FrameReader<ReadHalf<transport::Stream>>;

async fn raw_connection(daemon: &Daemon) -> (RawReader, WriteHalf<transport::Stream>) {
    let stream = transport::connect(&daemon.endpoint).await.unwrap();
    let (read_half, write_half) = tokio::io::split(stream);
    (frame_reader(read_half), write_half)
}

async fn expect_fatal(reader: &mut RawReader, code: ErrorCode) {
    match read_frame::<_, ServerFrame>(reader).await.unwrap() {
        Some(ServerFrame::Fatal { error }) => assert_eq!(error.code, code),
        other => panic!("expected a fatal {code:?}, got {other:?}"),
    }
    assert!(
        read_frame::<_, ServerFrame>(reader).await.unwrap().is_none(),
        "the server must close the connection after a fatal"
    );
}

#[tokio::test]
async fn a_client_older_than_the_oldest_supported_protocol_is_refused() {
    let daemon = Daemon::start().await;
    let (mut reader, write_half) = raw_connection(&daemon).await;
    let mut writer = frame_writer(write_half);
    write_frame(
        &mut writer,
        &ClientFrame::Hello {
            protocol_version: MIN_PROTOCOL_VERSION - 1,
        },
    )
    .await
    .unwrap();
    expect_fatal(&mut reader, ErrorCode::ProtocolMismatch).await;
}

async fn hello(reader: &mut RawReader, writer: &mut FrameWriter<WriteHalf<transport::Stream>>, version: u32) -> u32 {
    write_frame(
        writer,
        &ClientFrame::Hello {
            protocol_version: version,
        },
    )
    .await
    .unwrap();
    match read_frame::<_, ServerFrame>(reader).await.unwrap() {
        Some(ServerFrame::Hello { protocol_version }) => protocol_version,
        other => panic!("expected a hello, got {other:?}"),
    }
}

#[tokio::test]
async fn the_connection_uses_the_lower_of_the_two_protocol_versions() {
    let daemon = Daemon::start().await;

    // A client from the future is served at what this daemon speaks.
    let (mut reader, write_half) = raw_connection(&daemon).await;
    let mut writer = frame_writer(write_half);
    assert_eq!(
        hello(&mut reader, &mut writer, PROTOCOL_VERSION + 998).await,
        PROTOCOL_VERSION
    );

    // A version 1 client is served at version 1.
    let (mut reader, write_half) = raw_connection(&daemon).await;
    let mut writer = frame_writer(write_half);
    assert_eq!(hello(&mut reader, &mut writer, 1).await, 1);
}

#[tokio::test]
async fn an_unknown_request_is_answered_with_unsupported_and_the_connection_survives() {
    let daemon = Daemon::start().await;
    let (mut reader, write_half) = raw_connection(&daemon).await;
    let mut writer = frame_writer(write_half);
    hello(&mut reader, &mut writer, PROTOCOL_VERSION).await;

    write_frame(
        &mut writer,
        &serde_json::json!({"type": "request", "id": 5, "request": {"method": "from_the_future", "params": {"x": 1}}}),
    )
    .await
    .unwrap();
    match read_frame::<_, ServerFrame>(&mut reader).await.unwrap() {
        Some(ServerFrame::Response {
            id: 5,
            outcome: Outcome::Err { error },
        }) => assert_eq!(error.code, ErrorCode::Unsupported),
        other => panic!("expected an unsupported error, got {other:?}"),
    }

    write_frame(
        &mut writer,
        &ClientFrame::Request {
            id: 6,
            request: Request::Status,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        read_frame::<_, ServerFrame>(&mut reader).await.unwrap(),
        Some(ServerFrame::Response {
            id: 6,
            outcome: Outcome::Ok { .. }
        })
    ));
}

#[tokio::test]
async fn the_client_reports_the_negotiated_version() {
    let daemon = Daemon::start().await;
    assert_eq!(daemon.client().await.protocol_version(), PROTOCOL_VERSION);
}

#[tokio::test]
async fn a_request_before_the_hello_is_refused() {
    let daemon = Daemon::start().await;
    let (mut reader, write_half) = raw_connection(&daemon).await;
    let mut writer = frame_writer(write_half);
    write_frame(
        &mut writer,
        &ClientFrame::Request {
            id: 1,
            request: Request::Status,
        },
    )
    .await
    .unwrap();
    expect_fatal(&mut reader, ErrorCode::Invalid).await;
}

#[tokio::test]
async fn a_malformed_frame_ends_the_connection() {
    let daemon = Daemon::start().await;
    let (mut reader, write_half) = raw_connection(&daemon).await;
    let mut writer = frame_writer(write_half);
    write_frame(
        &mut writer,
        &ClientFrame::Hello {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        read_frame::<_, ServerFrame>(&mut reader).await.unwrap(),
        Some(ServerFrame::Hello { .. })
    ));

    write_frame(&mut writer, &serde_json::json!({"type": "nonsense"}))
        .await
        .unwrap();
    expect_fatal(&mut reader, ErrorCode::Invalid).await;
}

#[tokio::test]
async fn an_oversized_frame_header_ends_the_connection() {
    let daemon = Daemon::start().await;
    let (mut reader, mut write_half) = raw_connection(&daemon).await;
    // Claims a 9 MiB body, above the 8 MiB limit; the body never needs to be sent.
    write_half.write_all(&(9u32 * 1024 * 1024).to_be_bytes()).await.unwrap();
    expect_fatal(&mut reader, ErrorCode::Invalid).await;
}

#[tokio::test]
async fn connections_over_the_limit_are_told_the_daemon_is_busy() {
    let daemon = Daemon::start().await;
    let mut held = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        held.push(daemon.client().await);
    }
    match Client::connect(&daemon.endpoint).await {
        Err(err) => assert_eq!(code_of(err), ErrorCode::Busy),
        Ok(_) => panic!("the connection over the limit was accepted"),
    }
    // Releasing one frees a slot; the slot is returned when its server task ends.
    drop(held.pop());
    timeout(Duration::from_secs(5), async {
        loop {
            if Client::connect(&daemon.endpoint).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("a freed slot was never reused");
}

#[tokio::test]
async fn shutdown_closes_clients_and_the_daemon_reads_as_not_running() {
    let daemon = Daemon::start().await;
    let client = daemon.client().await;
    client.status().await.unwrap();
    let endpoint = daemon.endpoint.clone();

    daemon.stop().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(matches!(client.status().await, Err(ClientError::Disconnected)));
    assert!(matches!(
        Client::connect(&endpoint).await,
        Err(ClientError::NotRunning(_))
    ));
}

#[tokio::test]
async fn a_restarted_daemon_serves_the_data_from_before() {
    let daemon = Daemon::start().await;
    let task = daemon
        .client()
        .await
        .create_task(daemon.script("keep.sh", "true"))
        .await
        .unwrap();

    let dir = daemon.stop().await;
    let restarted = Daemon::start_in(dir).await;
    let tasks = restarted.client().await.list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, task.id);
}
