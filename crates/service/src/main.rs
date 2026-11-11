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
use scriptrunner_ipc::client::{Client, ClientError};
use scriptrunner_ipc::transport::{Endpoint, Listener};
use scriptrunner_service::engine::Engine;
use scriptrunner_service::ipc_server;
use scriptrunner_storage::Storage;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

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

fn data_dir(cli: &Cli) -> PathBuf {
    if let Some(dir) = &cli.data_dir {
        return dir.clone();
    }
    if cfg!(windows) {
        let base = std::env::var_os("PROGRAMDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\ProgramData"));
        return base.join("ScriptRunner");
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("scriptrunner")
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

/// Prints the log lines after the first `shown` ones and returns the new count.
/// The server caps each page, so this reads until a page comes back empty.
async fn print_new_log_lines(client: &Client, id: &ExecutionId, mut shown: usize) -> Result<usize> {
    loop {
        let page = client.read_log(id, shown, usize::MAX).await?;
        if page.is_empty() {
            return Ok(shown);
        }
        for entry in &page {
            let tag = match entry.kind {
                LogKind::Stdout => "out",
                LogKind::Stderr => "err",
                LogKind::Meta => "---",
            };
            println!("[{tag}] {}", entry.text);
            shown += 1;
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
        Command::Daemon => bail!("the daemon is started by main, not run as a client command"),
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
            let mut shown = 0;
            loop {
                let finished = match events.recv().await {
                    Ok(Event::ExecutionChanged(e)) if e.id == id => e.status.is_terminal(),
                    Ok(Event::ExecutionChanged(_) | Event::TasksChanged) => false,
                    // Events were lost (this client or the daemon fell behind): ask the
                    // daemon about the execution itself instead of trusting the stream.
                    Ok(Event::Resync) | Err(RecvError::Lagged(_)) => client
                        .get_execution(&id)
                        .await?
                        .is_some_and(|execution| execution.status.is_terminal()),
                    Err(RecvError::Closed) => bail!("lost the connection to the daemon while the run was in progress"),
                };
                shown = print_new_log_lines(client, &id, shown).await?;
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cli = Cli::parse();
    let dir = data_dir(&cli);

    match cli.command {
        Command::Daemon => {
            let storage = Storage::open(&dir.join("scriptrunner.db"))
                .with_context(|| format!("opening database in {}", dir.display()))?;
            let engine = Engine::new(storage, dir.clone())?;
            // Claim the endpoint first. A second daemon must fail here, *before*
            // `startup()` marks the first one's running executions as interrupted.
            let endpoint = Endpoint::for_data_dir(&dir);
            let listener = Listener::bind(&endpoint).await.context("starting the ipc server")?;
            engine.startup()?;
            tracing::info!(data_dir = %dir.display(), endpoint = %endpoint.address().display(), "daemon started");
            let shutdown = CancellationToken::new();
            let scheduler = tokio::spawn({
                let engine = engine.clone();
                let shutdown = shutdown.clone();
                async move { engine.run_scheduler(shutdown).await }
            });
            let server = tokio::spawn(ipc_server::serve(engine.clone(), listener, shutdown.clone()));
            tokio::signal::ctrl_c().await?;
            tracing::info!("shutting down");
            shutdown.cancel();
            scheduler.await?;
            server.await?;
            engine.shutdown().await;
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
