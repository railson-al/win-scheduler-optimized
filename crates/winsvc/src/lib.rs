//! Windows service plumbing for ScriptRunner: the Service Control Manager (SCM)
//! dispatcher and its control handler. Install, uninstall and the rest of the
//! service management live next to it (`manage`, added with the CLI commands).
//!
//! This crate knows nothing about the scheduler. The caller hands over a
//! [`ServiceBody`]; this crate runs it on its own thread, reports the state the
//! SCM expects at each point, and turns Stop, Shutdown and Preshutdown into a
//! cancelled [`CancellationToken`]. It has no SQLite dependency, so it can be
//! type-checked for Windows from Linux without a C cross-compiler.
#![cfg(windows)]

use std::ffi::OsString;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use scriptrunner_core::SERVICE_NAME;
use tokio_util::sync::CancellationToken;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle};
use windows_service::{define_windows_service, service_dispatcher};

/// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`: the process was not started by the SCM.
const NOT_STARTED_BY_SCM: i32 = 1063;

/// How often the service thread re-checks the worker and re-reports a pending state.
const POLL: Duration = Duration::from_millis(500);
/// The SCM gives up on a pending state this long after the *last* status report,
/// so each report with a higher checkpoint buys another window.
const START_WAIT_HINT: Duration = Duration::from_secs(30);
const STOP_WAIT_HINT: Duration = Duration::from_secs(20);
/// After this long without finishing, stop reporting progress (see the loop in `run_service`).
const START_DEADLINE: Duration = Duration::from_secs(120);
const STOP_DEADLINE: Duration = Duration::from_secs(60);

/// Called by the body once it is really serving, so the SCM is told "running"
/// only when that is true.
pub type ReadyFn = Box<dyn FnOnce() + Send>;

/// The service's work. It must return once its token is cancelled. Run on a
/// dedicated thread, so it is free to block (for instance on a tokio runtime).
pub type ServiceBody = Box<dyn FnOnce(CancellationToken, ReadyFn) -> Result<()> + Send>;

pub mod manage;

// `define_windows_service!` generates an `extern "system"` entry point that cannot
// capture anything, so the body and its outcome travel through statics.
static BODY: Mutex<Option<ServiceBody>> = Mutex::new(None);
static FAILURE: Mutex<Option<String>> = Mutex::new(None);

define_windows_service!(ffi_service_main, service_main);

fn lock<T>(mutex: &'static Mutex<T>) -> MutexGuard<'static, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Hands this process to the SCM and blocks until the service has stopped.
///
/// Fails fast with a clear message when started from a console instead of by
/// the SCM, and with the body's error if the service did not stop cleanly.
pub fn run(body: ServiceBody) -> Result<()> {
    *lock(&BODY) = Some(body);
    match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        Ok(()) => {}
        Err(windows_service::Error::Winapi(err)) if err.raw_os_error() == Some(NOT_STARTED_BY_SCM) => {
            bail!(
                "this command is only for the Windows Service Control Manager; to run in a console use `scriptrunner daemon`"
            )
        }
        Err(err) => return Err(err).context("connecting to the Service Control Manager"),
    }
    match lock(&FAILURE).take() {
        Some(message) => Err(anyhow!(message)),
        None => Ok(()),
    }
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(err) = run_service() {
        let message = format!("{err:#}");
        tracing::error!(error = %message, "the service stopped with an error");
        *lock(&FAILURE) = Some(message);
    }
}

fn run_service() -> Result<()> {
    let body = lock(&BODY).take().context("the service entry point ran twice")?;
    let token = CancellationToken::new();

    let handler_token = token.clone();
    let status = service_control_handler::register(SERVICE_NAME, move |control| match control {
        // Return to the SCM at once; the service thread reports `StopPending` while the work winds down.
        ServiceControl::Stop | ServiceControl::Shutdown | ServiceControl::Preshutdown => {
            handler_token.cancel();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })
    .context("registering the service control handler")?;

    report(
        &status,
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        1,
        START_WAIT_HINT,
        ServiceExitCode::Win32(0),
    )?;

    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<Result<()>>();
    let worker_token = token.clone();
    let spawned = std::thread::Builder::new()
        .name("scriptrunner-service".into())
        .spawn(move || {
            let result = body(
                worker_token,
                Box::new(move || {
                    let _ = ready_tx.send(());
                }),
            );
            let _ = done_tx.send(result);
        });
    if let Err(err) = spawned {
        let _ = report(
            &status,
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            0,
            Duration::ZERO,
            ServiceExitCode::ServiceSpecific(1),
        );
        return Err(err).context("starting the service thread");
    }

    let started_at = Instant::now();
    let mut stopping_since: Option<Instant> = None;
    let mut running = false;
    let mut checkpoint = 1u32;
    let result = loop {
        // A stop that arrives before the body is ready must not be reported as "running".
        if !running && !token.is_cancelled() && ready_rx.try_recv().is_ok() {
            running = true;
            let accepted =
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN | ServiceControlAccept::PRESHUTDOWN;
            report(
                &status,
                ServiceState::Running,
                accepted,
                0,
                Duration::ZERO,
                ServiceExitCode::Win32(0),
            )?;
        }
        match done_rx.recv_timeout(POLL) {
            Ok(result) => break result,
            Err(RecvTimeoutError::Timeout) => {
                // Each report with a higher checkpoint tells the SCM "still making progress". Past the
                // deadline, stop saying so: a start or stop that hangs must eventually time out
                // instead of showing "starting" or "stopping" forever.
                if token.is_cancelled() {
                    if stopping_since.get_or_insert_with(Instant::now).elapsed() < STOP_DEADLINE {
                        checkpoint = checkpoint.saturating_add(1);
                        report(
                            &status,
                            ServiceState::StopPending,
                            ServiceControlAccept::empty(),
                            checkpoint,
                            STOP_WAIT_HINT,
                            ServiceExitCode::Win32(0),
                        )?;
                    }
                } else if !running && started_at.elapsed() < START_DEADLINE {
                    checkpoint = checkpoint.saturating_add(1);
                    report(
                        &status,
                        ServiceState::StartPending,
                        ServiceControlAccept::empty(),
                        checkpoint,
                        START_WAIT_HINT,
                        ServiceExitCode::Win32(0),
                    )?;
                }
            }
            // The worker dropped its sender without a result: it panicked.
            Err(RecvTimeoutError::Disconnected) => break Err(anyhow!("the service thread ended unexpectedly")),
        }
    };

    let exit_code = match &result {
        Ok(()) => ServiceExitCode::Win32(0),
        // A problem while stopping *on request* must not look like a crash: with
        // "non-crash failures" enabled, the SCM would restart a service the user just stopped.
        Err(_) if token.is_cancelled() => ServiceExitCode::Win32(0),
        Err(_) => ServiceExitCode::ServiceSpecific(1),
    };
    // Recorded *before* reporting Stopped: Windows may end the process any time after that.
    if let Err(err) = &result {
        let message = format!("{err:#}");
        tracing::error!(error = %message, "the service stopped with an error");
        *lock(&FAILURE) = Some(message);
    }
    report(
        &status,
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        0,
        Duration::ZERO,
        exit_code,
    )?;
    Ok(())
}

fn report(
    handle: &ServiceStatusHandle,
    state: ServiceState,
    controls_accepted: ServiceControlAccept,
    checkpoint: u32,
    wait_hint: Duration,
    exit_code: ServiceExitCode,
) -> Result<()> {
    handle
        .set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted,
            exit_code,
            checkpoint,
            wait_hint,
            process_id: None,
        })
        .context("reporting the service status to the SCM")
}
