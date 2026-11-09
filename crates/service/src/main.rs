//! Development front end for the engine. It stands in for the Windows
//! service + GUI until those exist: `daemon` runs the scheduler, the other
//! commands manage tasks and inspect history.
//!
//! Commands other than `daemon`/`run` edit the database directly; a running
//! daemon notices within 30 seconds (the real service gets IPC in phase 2).

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveTime, Utc, Weekday};
use clap::{Args, Parser, Subcommand};
use scriptrunner_core::logs::LogKind;
use scriptrunner_core::{Execution, ExecutionStatus, Schedule, ScriptKind, Task};
use scriptrunner_service::engine::{Engine, Event};
use scriptrunner_storage::{ExecutionQuery, Storage};
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
        let base = std::env::var_os("PROGRAMDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\ProgramData"));
        return base.join("ScriptRunner");
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("scriptrunner")
}

fn parse_every(text: &str) -> Result<u64> {
    let (digits, unit) = text.split_at(text.len().saturating_sub(1));
    let n: u64 = digits.parse().with_context(|| format!("bad interval {text:?} (use e.g. 15m or 2h)"))?;
    match unit {
        "m" => Ok(n * 60),
        "h" => Ok(n * 3600),
        _ => bail!("bad interval {text:?} (use e.g. 15m or 2h)"),
    }
}

fn parse_time(text: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(text, "%H:%M").with_context(|| format!("bad time {text:?} (use HH:MM)"))
}

fn build_schedule(args: &AddArgs) -> Result<Option<Schedule>> {
    if let Some(every) = &args.every {
        return Ok(Some(Schedule::Interval { every_secs: parse_every(every)?, anchor: Utc::now() }));
    }
    if let Some(time) = &args.daily {
        return Ok(Some(Schedule::Daily { time: parse_time(time)? }));
    }
    if let Some(spec) = &args.weekly {
        let (days, time) = spec.split_once(' ').context("use `--weekly \"mon,fri 09:30\"`")?;
        let days = days
            .split(',')
            .map(|d| Weekday::from_str(d.trim()).map_err(|_| anyhow::anyhow!("unknown weekday {d:?}")))
            .collect::<Result<Vec<_>>>()?;
        return Ok(Some(Schedule::Weekly { days, time: parse_time(time.trim())? }));
    }
    Ok(args.cron.clone().map(|expr| Schedule::Cron { expr }))
}

/// Resolves a task by exact name, full id or unique id prefix.
fn find_task(engine: &Engine, key: &str) -> Result<Task> {
    let tasks = engine.list_tasks()?;
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

fn local(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string()
}

fn print_execution(e: &Execution) {
    let took = e.duration().map_or_else(|| "-".to_owned(), |d| format!("{:.1}s", d.num_milliseconds() as f64 / 1000.0));
    println!(
        "{}  {:<11} {:<9} {:<20} exit={:<4} {:>7}  {}{}",
        &e.id.as_str()[..8],
        e.status,
        e.trigger,
        e.task_name,
        e.exit_code.map_or_else(|| "-".to_owned(), |c| c.to_string()),
        took,
        local(e.created_at),
        e.failure_reason.as_deref().map(|r| format!("  ({r})")).unwrap_or_default(),
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cli = Cli::parse();
    let dir = data_dir(&cli);
    let storage = Storage::open(&dir.join("scriptrunner.db")).with_context(|| format!("opening database in {}", dir.display()))?;
    let engine = Engine::new(storage, dir.clone())?;

    match cli.command {
        Command::Daemon => {
            engine.startup()?;
            tracing::info!(data_dir = %dir.display(), "scheduler started");
            let shutdown = CancellationToken::new();
            let scheduler = tokio::spawn({
                let engine = engine.clone();
                let shutdown = shutdown.clone();
                async move { engine.run_scheduler(shutdown).await }
            });
            tokio::signal::ctrl_c().await?;
            tracing::info!("shutting down");
            shutdown.cancel();
            scheduler.await?;
            engine.shutdown().await;
        }
        Command::Add(args) => {
            let mut task = Task::new(&args.name, &args.script);
            if ScriptKind::from_path(&args.script).is_none() {
                tracing::warn!("unknown extension; treating {} as a directly executable file", args.script.display());
            }
            task.args = args.args.clone();
            task.interpreter = args.interpreter.clone();
            task.working_dir = args.workdir.clone();
            task.timeout_secs = args.timeout;
            task.allow_concurrent = args.concurrent;
            task.schedule = build_schedule(&args)?;
            let task = engine.create_task(task)?;
            println!("created {} ({})", task.name, &task.id.as_str()[..8]);
        }
        Command::List => {
            for row in engine.overview()? {
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
                    row.last_execution.as_ref().map_or("-".to_owned(), |e| e.status.to_string()),
                    row.next_run.map_or("-".to_owned(), local),
                );
            }
        }
        Command::Run { task } => {
            let task = find_task(&engine, &task)?;
            let mut events = engine.subscribe();
            let id = engine.run_now(&task.id)?;
            let mut shown = 0;
            loop {
                let finished = match events.recv().await {
                    Ok(Event::ExecutionChanged(e)) if e.id == id => e.status.is_terminal(),
                    Ok(_) => false,
                    Err(_) => true,
                };
                for entry in engine.read_log(&id, shown, usize::MAX)? {
                    let tag = match entry.kind {
                        LogKind::Stdout => "out",
                        LogKind::Stderr => "err",
                        LogKind::Meta => "---",
                    };
                    println!("[{tag}] {}", entry.text);
                    shown += 1;
                }
                if finished {
                    break;
                }
            }
            let execution = engine.get_execution(&id)?.context("execution vanished")?;
            print_execution(&execution);
            if execution.status != ExecutionStatus::Success {
                std::process::exit(1);
            }
        }
        Command::Disable { task } => {
            let task = engine.set_schedule_enabled(&find_task(&engine, &task)?.id, false)?;
            println!("paused {}", task.name);
        }
        Command::Enable { task } => {
            let task = engine.set_schedule_enabled(&find_task(&engine, &task)?.id, true)?;
            println!("resumed {}", task.name);
        }
        Command::Delete { task } => {
            let task = find_task(&engine, &task)?;
            engine.delete_task(&task.id)?;
            println!("deleted {}", task.name);
        }
        Command::History { task, limit } => {
            let task_id = task.map(|key| find_task(&engine, &key)).transpose()?.map(|t| t.id);
            for execution in engine.executions(&ExecutionQuery { task_id, limit: Some(limit), ..Default::default() })? {
                print_execution(&execution);
            }
        }
        Command::Logs { execution } => {
            let all = engine.executions(&ExecutionQuery { limit: Some(10_000), ..Default::default() })?;
            let Some(found) = all.iter().find(|e| e.id.as_str().starts_with(&execution)) else {
                bail!("no execution starting with {execution:?}");
            };
            print!("{}", engine.export_log(&found.id)?);
        }
    }
    Ok(())
}
