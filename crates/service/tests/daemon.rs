#![cfg(unix)]
//! `daemon::run` as the console command and the Windows service use it: start,
//! stop by token, and refuse a second daemon on the same data directory.

use std::time::Duration;

use scriptrunner_ipc::client::{Client, ClientError};
use scriptrunner_ipc::transport::Endpoint;
use scriptrunner_service::daemon;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

struct Running {
    shutdown: CancellationToken,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
}

/// Starts the daemon and waits for its `ready` signal.
async fn start(dir: &std::path::Path) -> Running {
    let shutdown = CancellationToken::new();
    let (ready_tx, ready_rx) = oneshot::channel();
    let handle = tokio::spawn({
        let dir = dir.to_path_buf();
        let shutdown = shutdown.clone();
        async move {
            daemon::run(&dir, shutdown, || {
                let _ = ready_tx.send(());
            })
            .await
        }
    });
    timeout(Duration::from_secs(10), ready_rx)
        .await
        .expect("the daemon never became ready")
        .expect("the daemon failed before becoming ready");
    Running { shutdown, handle }
}

#[tokio::test]
async fn starts_serves_and_stops_when_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = Endpoint::for_data_dir(dir.path());
    let daemon = start(dir.path()).await;

    let client = Client::connect(&endpoint).await.unwrap();
    assert_eq!(
        client.status().await.unwrap().protocol_version,
        scriptrunner_ipc::PROTOCOL_VERSION
    );

    daemon.shutdown.cancel();
    timeout(Duration::from_secs(10), daemon.handle)
        .await
        .expect("the daemon did not stop")
        .unwrap()
        .unwrap();
    assert!(matches!(
        Client::connect(&endpoint).await,
        Err(ClientError::NotRunning(_))
    ));
}

#[tokio::test]
async fn a_second_daemon_on_the_same_directory_is_refused_and_the_first_is_unharmed() {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = Endpoint::for_data_dir(dir.path());
    let first = start(dir.path()).await;

    let second = daemon::run(dir.path(), CancellationToken::new(), || panic!("must not become ready")).await;
    let message = format!("{:#}", second.unwrap_err());
    assert!(message.contains("already listening"), "got: {message}");

    Client::connect(&endpoint).await.unwrap().status().await.unwrap();
    first.shutdown.cancel();
    first.handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn ready_is_not_called_when_the_database_cannot_be_opened() {
    let dir = tempfile::tempdir().unwrap();
    // A data directory nested inside a regular file can never be created.
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let result = daemon::run(&blocker.join("data"), CancellationToken::new(), || {
        panic!("must not become ready")
    })
    .await;
    assert!(result.is_err());
}
