//! Development front end for the engine. `daemon` runs the scheduler and owns
//! the database. Every other command is a client of that running daemon: it
//! talks to it over local IPC (a named pipe on Windows, a Unix socket
//! elsewhere) and never opens the database itself, so start the daemon first.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveTime, Utc, Weekday};
use clap::{Args, Parser, Subcommand};
use scriptrunner_core::logs::LogKind;
use scriptrunner_core::{Execution, ExecutionId, ExecutionQuery, ExecutionStatus, Schedule, ScriptKind, Task};
use scriptrunner_ipc::Event;
use scriptrunner_ipc::client::{Client, ClientError, LOG_CHUNK_BYTES};
use scriptrunner_ipc::transport::Endpoint;
use scriptrunner_service::{daemon, logging};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(name = "scriptrunner", version, about = "Script scheduler (engine harness)")]
struct Cli {
    /// Where the database and logs live.
    #[arg(long, global = true, env = "SCRIPTRUNNER_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the scheduler in the foreground until Ctrl+C.
    Daemon,
    /// Install, start, stop or remove the Windows service (Windows only; needs Administrator).
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Register a task.
    Add(Box<AddArgs>),
    /// List tasks with status and next run.
    List,
    /// Run a task now, streaming its output.
    Run { task: String },
    /// Pause a task's schedule.
    Disable { task: String },
    /// Resume a task's schedule.
    Enable { task: String },
    /// Delete a task (its history is kept).
    Delete { task: String },
    /// Show execution history, newest first.
    History {
        #[arg(long)]
        task: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Print the log of an execution.
    Logs { execution: String },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Create the service (LocalSystem, automatic start) and lock down the data directory.
    Install,
    /// Stop the service if needed and remove it. The data directory is kept.
    Uninstall,
    /// Start the service and wait until it is running.
    Start,
    /// Stop the service and wait until it has stopped.
    Stop,
    /// Show whether the service is installed and running.
    Status,
    /// What the Service Control Manager runs. Not meant to be typed by hand.
    #[command(hide = true)]
    Run,
}

#[derive(Args)]
struct AddArgs {
    name: String,
    script: PathBuf,
    /// Argument passed to the script (repeatable).
    #[arg(long = "arg")]
    args: Vec<String>,
    /// Interpreter override (e.g. a venv's python).
    #[arg(long)]
    interpreter: Option<PathBuf>,
    #[arg(long)]
    workdir: Option<PathBuf>,
    #[arg(long)]
    timeout: Option<u64>,
    /// Allow overlapping runs of this task.
    #[arg(long)]
    concurrent: bool,
    /// Every N minutes/hours, e.g. `15m` or `2h`.
    #[arg(long, conflicts_with_all = ["daily", "weekly", "cron"])]
    every: Option<String>,
    /// Every day at HH:MM (local time).
    #[arg(long, conflicts_with_all = ["weekly", "cron"])]
    daily: Option<String>,
    /// Days and time, e.g. `mon,wed,fri 09:30`.
    #[arg(long, conflicts_with = "cron")]
    weekly: Option<String>,
    /// Standard 5-field cron expression.
    #[arg(long)]
    cron: Option<String>,
}

fn parse_every(text: &str) -> Result<u64> {
    let usage = || format!("bad interval {text:?} (use e.g. 15m or 2h)");
    // Split on the last *character*, not the last byte: a multibyte suffix must
    // be an error, not a panic.
    let unit = text.chars().next_back().with_context(usage)?;
    let digits = &text[..text.len() - unit.len_utf8()];
    let n: u64 = digits.parse().with_context(usage)?;
    let seconds_per_unit: u64 = match unit {
        'm' => 60,
        'h' => 3600,
        _ => bail!(usage()),
    };
    n.checked_mul(seconds_per_unit)
        .with_context(|| format!("interval {text:?} is too large"))
}

fn parse_time(text: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(text, "%H:%M").with_context(|| format!("bad time {text:?} (use HH:MM)"))
}

fn build_schedule(args: &AddArgs) -> Result<Option<Schedule>> {
    if let Some(every) = &args.every {
        return Ok(Some(Schedule::Interval {
            every_secs: parse_every(every)?,
            anchor: Utc::now(),
        }));
    }
    if let Some(time) = &args.daily {
        return Ok(Some(Schedule::Daily {
            time: parse_time(time)?,
        }));
    }
    if let Some(spec) = &args.weekly {
        let (days, time) = spec.split_once(' ').context("use `--weekly \"mon,fri 09:30\"`")?;
        let days = days
            .split(',')
            .map(|d| Weekday::from_str(d.trim()).map_err(|_| anyhow::anyhow!("unknown weekday {d:?}")))
            .collect::<Result<Vec<_>>>()?;
        return Ok(Some(Schedule::Weekly {
            days,
            time: parse_time(time.trim())?,
        }));
    }
    Ok(args.cron.clone().map(|expr| Schedule::Cron { expr }))
}

/// Connects to the daemon that owns `dir`.
async fn connect(dir: &Path) -> Result<Client> {
    match Client::connect(&Endpoint::for_data_dir(dir)).await {
        Ok(client) => Ok(client),
        Err(ClientError::NotRunning(_)) => {
            if cfg!(windows) {
                bail!(
                    "the ScriptRunner daemon is not running; start the service with `scriptrunner service start` \
                     (or run `scriptrunner daemon` in a console if the service is not installed)"
                )
            }
            bail!("the ScriptRunner daemon is not running; start it with `scriptrunner daemon`")
        }
        Err(err) => Err(err).context("connecting to the ScriptRunner daemon"),
    }
}

/// Resolves a task by exact name, full id or unique id prefix.
async fn find_task(client: &Client, key: &str) -> Result<Task> {
    let tasks = client.list_tasks().await?;
    if let Some(task) = tasks.iter().find(|t| t.name == key || t.id.as_str() == key) {
        return Ok(task.clone());
    }
    let matches: Vec<_> = tasks.iter().filter(|t| t.id.as_str().starts_with(key)).collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("no task named or numbered {key:?}"),
        _ => bail!("{key:?} matches several tasks; use more of the id"),
    }
}

/// Resolves an execution by full id, or by a unique id prefix among the most
/// recent executions.
async fn resolve_execution(client: &Client, key: &str) -> Result<ExecutionId> {
    let full = ExecutionId::from(key.to_owned());
    if client.get_execution(&full).await?.is_some() {
        return Ok(full);
    }
    let recent = client
        .executions(ExecutionQuery {
            limit: Some(1000),
            ..Default::default()
        })
        .await?;
    let matches: Vec<_> = recent.iter().filter(|e| e.id.as_str().starts_with(key)).collect();
    match matches.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => bail!("no execution starting with {key:?}"),
        _ => bail!("{key:?} matches several executions; use more of the id"),
    }
}

/// Prints the log lines from byte `offset` on and returns where to continue.
/// The daemon caps each page, so this reads until it reports the end of the file.
async fn print_new_log_lines(client: &Client, id: &ExecutionId, mut offset: u64) -> Result<u64> {
    loop {
        let chunk = client.read_log_from(id, offset, LOG_CHUNK_BYTES).await?;
        for entry in &chunk.entries {
            let tag = match entry.kind {
                LogKind::Stdout => "out",
                LogKind::Stderr => "err",
                LogKind::Meta => "---",
            };
            println!("[{tag}] {}", entry.text);
        }
        offset = chunk.next_offset;
        if chunk.eof {
            return Ok(offset);
        }
    }
}

fn local(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string()
}

fn print_execution(e: &Execution) {
    let took = e.duration().map_or_else(
        || "-".to_owned(),
        |d| format!("{:.1}s", d.num_milliseconds() as f64 / 1000.0),
    );
    println!(
        "{}  {:<11} {:<9} {:<20} exit={:<4} {:>7}  {}{}",
        &e.id.as_str()[..8],
        e.status,
        e.trigger,
        e.task_name,
        e.exit_code.map_or_else(|| "-".to_owned(), |c| c.to_string()),
        took,
        local(e.created_at),
        e.failure_reason
            .as_deref()
            .map(|r| format!("  ({r})"))
            .unwrap_or_default(),
    );
}

async fn run_client_command(client: &Client, command: Command) -> Result<()> {
    match command {
        Command::Daemon | Command::Service { .. } => bail!("this command is handled by main, not as a client command"),
        Command::Add(args) => {
            let mut task = Task::new(&args.name, &args.script);
            if ScriptKind::from_path(&args.script).is_none() {
                tracing::warn!(
                    "unknown extension; treating {} as a directly executable file",
                    args.script.display()
                );
            }
            task.args = args.args.clone();
            task.interpreter = args.interpreter.clone();
            task.working_dir = args.workdir.clone();
            task.timeout_secs = args.timeout;
            task.allow_concurrent = args.concurrent;
            task.schedule = build_schedule(&args)?;
            let task = client.create_task(task).await?;
            println!("created {} ({})", task.name, &task.id.as_str()[..8]);
        }
        Command::List => {
            for row in client.overview().await? {
                let state = match (row.running, row.task.schedule.is_some(), row.task.schedule_enabled) {
                    (true, _, _) => "running",
                    (_, false, _) => "manual",
                    (_, true, true) => "active",
                    (_, true, false) => "paused",
                };
                println!(
                    "{}  {:<20} {:<8} last={:<11} next={}",
                    &row.task.id.as_str()[..8],
                    row.task.name,
                    state,
                    row.last_execution
                        .as_ref()
                        .map_or("-".to_owned(), |e| e.status.to_string()),
                    row.next_run.map_or("-".to_owned(), local),
                );
            }
        }
        Command::Run { task } => {
            let task = find_task(client, &task).await?;
            // Subscribe before starting the run so its first events cannot be missed.
            let mut events = client.subscribe().await?;
            let id = client.run_now(&task.id).await?;
            let mut offset = 0;
            loop {
                let finished = match events.recv().await {
                    Ok(Event::ExecutionChanged(e)) if e.id == id => e.status.is_terminal(),
                    Ok(Event::ExecutionChanged(_) | Event::TasksChanged | Event::Unknown) => false,
                    // Events were lost (this client or the daemon fell behind): ask the
                    // daemon about the execution itself instead of trusting the stream.
                    Ok(Event::Resync) | Err(RecvError::Lagged(_)) => client
                        .get_execution(&id)
                        .await?
                        .is_some_and(|execution| execution.status.is_terminal()),
                    Err(RecvError::Closed) => bail!("lost the connection to the daemon while the run was in progress"),
                };
                offset = print_new_log_lines(client, &id, offset).await?;
                if finished {
                    break;
                }
            }
            let execution = client.get_execution(&id).await?.context("execution vanished")?;
            print_execution(&execution);
            if execution.status != ExecutionStatus::Success {
                std::process::exit(1);
            }
        }
        Command::Disable { task } => {
            let task = find_task(client, &task).await?;
            let task = client.set_schedule_enabled(&task.id, false).await?;
            println!("paused {}", task.name);
        }
        Command::Enable { task } => {
            let task = find_task(client, &task).await?;
            let task = client.set_schedule_enabled(&task.id, true).await?;
            println!("resumed {}", task.name);
        }
        Command::Delete { task } => {
            let task = find_task(client, &task).await?;
            client.delete_task(&task.id).await?;
            println!("deleted {}", task.name);
        }
        Command::History { task, limit } => {
            let task_id = match task {
                Some(key) => Some(find_task(client, &key).await?.id),
                None => None,
            };
            for execution in client
                .executions(ExecutionQuery {
                    task_id,
                    limit: Some(limit),
                    ..Default::default()
                })
                .await?
            {
                print_execution(&execution);
            }
        }
        Command::Logs { execution } => {
            let id = resolve_execution(client, &execution).await?;
            print!("{}", client.export_log(&id).await?);
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let dir = scriptrunner_core::paths::data_dir(cli.data_dir.clone());

    // The service has no console, so it logs to a file and must not install the
    // console logger first. It also owns its threads and runtime (see `service_run`).
    if let Command::Service { action } = &cli.command {
        return match action {
            ServiceAction::Run => service_run(&dir),
            other => service_command(other, &dir),
        };
    }

    logging::init_console();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?
        .block_on(run_cli(cli, dir))
}

#[cfg(windows)]
fn service_run(dir: &Path) -> Result<()> {
    // Absolute, because the SCM starts the process in System32.
    let dir = std::path::absolute(dir).context("resolving the data directory")?;
    // First, before anything (the log directory included) is created inside it. If the
    // directory fails the check, nothing is written into it: it may belong to an attacker,
    // and SYSTEM appending to files there could be abused through links. The failure shows
    // up as the service failing to start; `scriptrunner service install` repeats the check
    // and says what is wrong.
    scriptrunner_winsvc::manage::secure_data_dir(&dir).context("securing the data directory")?;
    let _log = match logging::init_file(&dir.join("service")) {
        Ok(guard) => guard,
        Err(err) => {
            // Nowhere to log yet; leave a trace the administrator can find.
            logging::emergency(&dir, &format!("cannot start logging: {err:#}"));
            return Err(err);
        }
    };
    // A service has no console to print a panic to, so it goes to the log.
    std::panic::set_hook(Box::new(|info| tracing::error!(panic = %info, "the service panicked")));
    let result = scriptrunner_winsvc::run(Box::new({
        let dir = dir.clone();
        move |token, ready| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("starting the async runtime")?;
            let outcome = runtime.block_on(daemon::run(&dir, token, ready));
            // Dropping a runtime waits for its blocking tasks (SQLite, files). Do not let one
            // that is stuck keep a stopping service alive forever.
            runtime.shutdown_timeout(std::time::Duration::from_secs(5));
            outcome
        }
    }));
    if let Err(err) = &result {
        logging::emergency(&dir, &format!("the service stopped with an error: {err:#}"));
    }
    result
}

#[cfg(windows)]
fn service_command(action: &ServiceAction, dir: &Path) -> Result<()> {
    use scriptrunner_winsvc::manage;

    let dir = std::path::absolute(dir).context("resolving the data directory")?;
    match action {
        ServiceAction::Install => {
            let executable = std::env::current_exe().context("finding this executable")?;
            manage::install(&executable, &dir)?;
            println!(
                "installed the ScriptRunner service (data in {}); start it with `scriptrunner service start`",
                dir.display()
            );
        }
        ServiceAction::Uninstall => {
            manage::uninstall()?;
            println!("removed the ScriptRunner service (data kept in {})", dir.display());
        }
        ServiceAction::Start => {
            manage::start()?;
            println!("the ScriptRunner service is running");
        }
        ServiceAction::Stop => {
            manage::stop()?;
            println!("the ScriptRunner service is stopped");
        }
        ServiceAction::Status => println!("the ScriptRunner service is {}", manage::status()?),
        ServiceAction::Run => bail!("`service run` is started by the Service Control Manager"),
    }
    Ok(())
}

#[cfg(not(windows))]
fn service_run(_dir: &Path) -> Result<()> {
    bail!("the Windows service only exists on Windows; use `scriptrunner daemon` here")
}

#[cfg(not(windows))]
fn service_command(_action: &ServiceAction, _dir: &Path) -> Result<()> {
    bail!("the Windows service only exists on Windows; use `scriptrunner daemon` here")
}

async fn run_cli(cli: Cli, dir: PathBuf) -> Result<()> {
    match cli.command {
        Command::Daemon => {
            // The pipe admits only administrators, and the default data directory is the
            // production one, so the console daemon protects it the same way the service does.
            #[cfg(windows)]
            scriptrunner_winsvc::manage::secure_data_dir(&dir)
                .context("securing the data directory (run the daemon from an elevated terminal)")?;
            let shutdown = CancellationToken::new();
            tokio::spawn({
                let shutdown = shutdown.clone();
                async move {
                    // If the signal handler cannot be installed, stopping is the safe outcome:
                    // a daemon nobody can interrupt is worse than one that exits.
                    if let Err(err) = tokio::signal::ctrl_c().await {
                        tracing::error!(%err, "cannot listen for Ctrl+C; stopping");
                    }
                    shutdown.cancel();
                }
            });
            daemon::run(&dir, shutdown, || {}).await?;
        }
        command => {
            let client = connect(&dir).await?;
            run_client_command(&client, command).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_every;

    #[test]
    fn parses_minutes_and_hours() {
        assert_eq!(parse_every("15m").unwrap(), 900);
        assert_eq!(parse_every("2h").unwrap(), 7200);
    }

    #[test]
    fn rejects_malformed_input_without_panicking() {
        for bad in ["", "m", "15", "15x", "-5m", "1.5h", "15é", "é", "１５m"] {
            assert!(parse_every(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn rejects_values_that_overflow() {
        assert!(parse_every("18446744073709551615m").is_err());
        assert!(parse_every("18446744073709551615h").is_err());
    }
}
