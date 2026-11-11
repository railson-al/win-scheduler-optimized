//! The daemon proper: owns the database, the scheduler and the IPC server.
//!
//! The console command (`scriptrunner daemon`) and the Windows service wrapper
//! both call [`run`]; they differ only in how they ask it to stop (Ctrl+C or a
//! service control request) and in what they do once it is up.

use std::path::Path;

use anyhow::{Context, Result};
use scriptrunner_ipc::transport::{Endpoint, Listener};
use scriptrunner_storage::Storage;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::engine::Engine;
use crate::ipc_server;

/// Runs the daemon until `shutdown` is cancelled, then stops it in order: the
/// scheduler and IPC server first, running scripts last.
///
/// `ready` is called once, after the endpoint is bound and the engine has
/// started, so a caller can report "running" only when it is true. Failing to
/// bind (another daemon owns the endpoint) is an error *before* `ready`.
pub async fn run(data_dir: &Path, shutdown: CancellationToken, ready: impl FnOnce()) -> Result<()> {
    let storage = Storage::open(&data_dir.join("scriptrunner.db"))
        .with_context(|| format!("opening database in {}", data_dir.display()))?;
    let engine = Engine::new(storage, data_dir.to_path_buf())?;

    // Claim the endpoint first. A second daemon must fail here, *before*
    // `startup()` marks the first one's running executions as interrupted.
    let endpoint = Endpoint::for_data_dir(data_dir);
    let listener = Listener::bind(&endpoint).await.context("starting the ipc server")?;
    engine.startup()?;
    info!(data_dir = %data_dir.display(), endpoint = %endpoint.address().display(), "daemon started");

    let scheduler = tokio::spawn({
        let engine = engine.clone();
        let shutdown = shutdown.clone();
        async move { engine.run_scheduler(shutdown).await }
    });
    let server = tokio::spawn(ipc_server::serve(engine.clone(), listener, shutdown.clone()));
    ready();

    shutdown.cancelled().await;
    info!("shutting down");
    // Always stop the running scripts, even if the scheduler or server task failed: skipping
    // this would orphan them (there is no kill-on-close on the job object).
    let scheduler = scheduler.await;
    let server = server.await;
    engine.shutdown().await;
    scheduler?;
    server?;
    Ok(())
}
