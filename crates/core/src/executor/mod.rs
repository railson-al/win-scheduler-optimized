//! Launches a script as a child process, streams its output and enforces
//! timeout / cancellation by killing the whole process tree.

#[cfg(windows)]
mod interactive;
mod tree;
mod winlaunch;

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::models::{ExecutionStatus, RunAs, ScriptKind, Settings, Task};
use tree::ProcessTree;

/// Longest chunk emitted as a single log line; longer lines are split so a
/// script printing without newlines cannot grow our memory without bound.
const MAX_LINE_BYTES: u64 = 64 * 1024;

/// After the main process exits, how long we wait for its output pipes to
/// close. Detached children that inherited the pipes would otherwise hold us up.
const READER_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPlan {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: Option<PathBuf>,
}

/// Decides *what* to run for a task, without running it.
pub fn plan_command(task: &Task, settings: &Settings) -> CommandPlan {
    let script = task.script_path.as_os_str().to_owned();
    let user_args = task.args.iter().map(OsString::from);
    let mut env = Vec::new();

    let (program, args): (OsString, Vec<OsString>) = match task.script_kind {
        ScriptKind::Python => {
            let python = task
                .interpreter
                .clone()
                .or_else(|| settings.default_python.clone())
                .map(PathBuf::into_os_string)
                .unwrap_or_else(|| default_python().into());
            // -u: unbuffered, so logs stream live instead of arriving at exit.
            let mut args = vec![OsString::from("-u"), script];
            args.extend(user_args);
            env.push((OsString::from("PYTHONUTF8"), OsString::from("1")));
            env.push((OsString::from("PYTHONIOENCODING"), OsString::from("utf-8")));
            (python, args)
        }
        // `.bat`/`.cmd` are spawned directly: Rust's std routes them through
        // cmd.exe itself and handles the quoting of paths with spaces, which a
        // hand-built `cmd /C "..."` line famously gets wrong.
        ScriptKind::Bat | ScriptKind::Cmd | ScriptKind::Exe => (script, user_args.collect()),
        ScriptKind::PowerShell => {
            let shell = task
                .interpreter
                .clone()
                .map(PathBuf::into_os_string)
                .unwrap_or_else(|| OsString::from("powershell.exe"));
            let mut args: Vec<OsString> = ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"]
                .into_iter()
                .map(OsString::from)
                .collect();
            args.push(script);
            args.extend(user_args);
            (shell, args)
        }
    };

    let cwd = task.working_dir.clone().or_else(|| {
        task.script_path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(Path::to_path_buf)
    });

    CommandPlan {
        program,
        args,
        env,
        cwd,
    }
}

fn default_python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub at: DateTime<Utc>,
    pub stream: LogStream,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// One of `Success`, `Failed` or `Interrupted`.
    pub status: ExecutionStatus,
    pub exit_code: Option<i32>,
    pub failure_reason: Option<String>,
}

impl RunOutcome {
    fn new(status: ExecutionStatus, exit_code: Option<i32>, reason: impl Into<Option<String>>) -> Self {
        Self {
            status,
            exit_code,
            failure_reason: reason.into(),
        }
    }

    fn from_exit(status: ExitStatus) -> Self {
        match status.code() {
            Some(0) => Self::new(ExecutionStatus::Success, Some(0), None),
            Some(code) => Self::new(ExecutionStatus::Failed, Some(code), format!("exited with code {code}")),
            None => Self::new(ExecutionStatus::Failed, None, describe_abnormal_exit(status)),
        }
    }
}

#[cfg(unix)]
fn describe_abnormal_exit(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status.signal() {
        Some(signal) => format!("terminated by signal {signal}"),
        None => "terminated abnormally".to_owned(),
    }
}

#[cfg(not(unix))]
fn describe_abnormal_exit(_status: ExitStatus) -> String {
    "terminated abnormally".to_owned()
}

/// A spawned script. Dropping it without calling [`RunningProcess::wait`]
/// kills the main process.
pub struct RunningProcess {
    process: Process,
    tree: ProcessTree,
    readers: Vec<JoinHandle<()>>,
    pid: Option<u32>,
}

/// What is actually running: an ordinary child of this process, or (Windows only) a program started in
/// the logged-on user's session, which `tokio::process` cannot start (see `interactive.rs`).
// One per running script, so boxing the big variant would only add an allocation.
#[allow(clippy::large_enum_variant)]
enum Process {
    Child(Child),
    #[cfg(windows)]
    UserSession(interactive::UserProcess),
}

impl Process {
    async fn wait(&mut self) -> io::Result<ExitStatus> {
        match self {
            Self::Child(child) => child.wait().await,
            #[cfg(windows)]
            Self::UserSession(process) => process.wait().await,
        }
    }

    /// Ends the main process directly (the tree is killed separately) and reaps it.
    async fn kill(&mut self) {
        match self {
            Self::Child(child) => {
                let _ = child.kill().await;
            }
            #[cfg(windows)]
            Self::UserSession(process) => process.kill(),
        }
    }
}

/// Starts the task's script where the task says it runs: as the service (an ordinary child), or in the
/// session of the user who is logged on. The second exists on Windows only, and fails with a clear
/// reason when nobody is logged on.
pub fn spawn_as(run_as: RunAs, plan: &CommandPlan, logs: Option<mpsc::Sender<LogLine>>) -> io::Result<RunningProcess> {
    match run_as {
        RunAs::Service => spawn(plan, logs),
        RunAs::UserSession => spawn_in_user_session(plan, logs),
    }
}

#[cfg(windows)]
fn spawn_in_user_session(plan: &CommandPlan, logs: Option<mpsc::Sender<LogLine>>) -> io::Result<RunningProcess> {
    interactive::spawn(plan, logs)
}

#[cfg(not(windows))]
fn spawn_in_user_session(_plan: &CommandPlan, _logs: Option<mpsc::Sender<LogLine>>) -> io::Result<RunningProcess> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "running a task in the user's session is only available on Windows",
    ))
}

/// Serialises the moments at which a child process is created, so that handles made inheritable for one
/// child (the user-session pipes) cannot be inherited by another. Held briefly and never across an `await`.
fn spawn_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // Nothing is protected by the lock but the ordering, so a panic while holding it changes nothing.
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Starts the process. Output lines are sent to `logs` (bounded, so a slow
/// consumer applies back-pressure instead of growing memory); when `None`,
/// the pipes are still drained and discarded so the script never blocks.
pub fn spawn(plan: &CommandPlan, logs: Option<mpsc::Sender<LogLine>>) -> io::Result<RunningProcess> {
    let mut command = Command::new(&plan.program);
    command
        .args(&plan.args)
        .envs(plan.env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &plan.cwd {
        command.current_dir(cwd);
    }
    tree::configure(&mut command);

    // Not while a script is being started in the user's session: its pipes are inheritable for a moment
    // (see `interactive::spawn`), and this child would inherit them.
    let spawn_guard = spawn_lock();
    let mut child = command.spawn()?;
    drop(spawn_guard);
    let tree = ProcessTree::attach(&child);
    let pid = child.id();

    let mut readers = Vec::with_capacity(2);
    if let Some(stdout) = child.stdout.take() {
        readers.push(tokio::spawn(pump(stdout, LogStream::Stdout, logs.clone())));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(tokio::spawn(pump(stderr, LogStream::Stderr, logs)));
    }
    Ok(RunningProcess {
        process: Process::Child(child),
        tree,
        readers,
        pid,
    })
}

impl RunningProcess {
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Waits for the script to end, enforcing `timeout` and `cancel`. On
    /// timeout or cancellation the whole process tree is killed.
    pub async fn wait(mut self, timeout: Option<Duration>, cancel: &CancellationToken) -> RunOutcome {
        enum End {
            Exited(io::Result<ExitStatus>),
            TimedOut,
            Cancelled,
        }

        let end = tokio::select! {
            status = self.process.wait() => End::Exited(status),
            _ = sleep_or_pending(timeout) => End::TimedOut,
            _ = cancel.cancelled() => End::Cancelled,
        };

        let outcome = match end {
            End::Exited(Ok(status)) => RunOutcome::from_exit(status),
            End::Exited(Err(err)) => RunOutcome::new(
                ExecutionStatus::Failed,
                None,
                format!("failed while waiting for the process: {err}"),
            ),
            End::TimedOut => {
                self.terminate().await;
                let secs = timeout.map_or(0, |t| t.as_secs());
                RunOutcome::new(ExecutionStatus::Failed, None, format!("timed out after {secs}s"))
            }
            End::Cancelled => {
                self.terminate().await;
                RunOutcome::new(ExecutionStatus::Interrupted, None, "stopped by user".to_owned())
            }
        };

        self.drain_readers().await;
        outcome
    }

    async fn terminate(&mut self) {
        self.tree.kill();
        // Also kills the main process directly (fallback if the tree could not
        // be tracked) and reaps it.
        self.process.kill().await;
    }

    async fn drain_readers(&mut self) {
        for mut reader in self.readers.drain(..) {
            if tokio::time::timeout(READER_GRACE, &mut reader).await.is_err() {
                reader.abort();
            }
        }
    }
}

async fn sleep_or_pending(timeout: Option<Duration>) {
    match timeout {
        Some(duration) => tokio::time::sleep(duration).await,
        None => std::future::pending().await,
    }
}

async fn pump<R>(source: R, stream: LogStream, mut sink: Option<mpsc::Sender<LogLine>>)
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(source);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match (&mut reader).take(MAX_LINE_BYTES).read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // Keep reading (to keep the script unblocked) even if the consumer is gone.
        if let Some(tx) = &sink {
            let line = LogLine {
                at: Utc::now(),
                stream,
                text: decode_output(trim_eol(&buf)),
            };
            if tx.send(line).await.is_err() {
                sink = None;
            }
        }
    }
}

fn trim_eol(mut bytes: &[u8]) -> &[u8] {
    if let Some(rest) = bytes.strip_suffix(b"\n") {
        bytes = rest;
    }
    if let Some(rest) = bytes.strip_suffix(b"\r") {
        bytes = rest;
    }
    bytes
}

/// UTF-8 when valid; otherwise the console's OEM code page on Windows (what
/// `cmd`, batch files and PowerShell 5 emit — e.g. cp850 on pt-BR systems),
/// lossy UTF-8 elsewhere.
fn decode_output(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => decode_legacy(bytes),
    }
}

#[cfg(windows)]
fn decode_legacy(bytes: &[u8]) -> String {
    use windows_sys::Win32::Globalization::{CP_OEMCP, MultiByteToWideChar};

    let Ok(len_in) = i32::try_from(bytes.len()) else {
        return String::from_utf8_lossy(bytes).into_owned();
    };
    // SAFETY: both calls get valid buffers whose lengths we pass alongside.
    unsafe {
        let needed = MultiByteToWideChar(CP_OEMCP, 0, bytes.as_ptr(), len_in, std::ptr::null_mut(), 0);
        if needed <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let mut wide = vec![0u16; needed as usize];
        MultiByteToWideChar(CP_OEMCP, 0, bytes.as_ptr(), len_in, wide.as_mut_ptr(), needed);
        String::from_utf16_lossy(&wide)
    }
}

#[cfg(not(windows))]
fn decode_legacy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// Off Windows there is no user session to start a script in. Refusing says so; running it as the service
    /// instead would silently do something the task did not ask for.
    #[cfg(not(windows))]
    #[test]
    fn running_in_the_users_session_is_refused_where_it_does_not_exist() {
        let plan = plan_command(&Task::new("t", "a.sh"), &Settings::default());
        let error = spawn_as(RunAs::UserSession, &plan, None)
            .err()
            .expect("must be refused");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("only available on Windows"), "{error}");
    }

    #[test]
    fn python_uses_configured_interpreter_and_unbuffered_flag() {
        let mut task = Task::new("t", "C:\\jobs\\job.py");
        task.args = vec!["--x".into(), "a b".into()];
        task.interpreter = Some(PathBuf::from("C:\\venv\\Scripts\\python.exe"));
        let plan = plan_command(&task, &Settings::default());
        assert_eq!(plan.program, OsString::from("C:\\venv\\Scripts\\python.exe"));
        assert_eq!(plan.args, os(&["-u", "C:\\jobs\\job.py", "--x", "a b"]));
        assert!(plan.env.iter().any(|(k, v)| k == "PYTHONUTF8" && v == "1"));
    }

    #[test]
    fn python_falls_back_to_settings_then_default() {
        let task = Task::new("t", "job.py");
        let settings = Settings {
            default_python: Some("C:\\py\\python.exe".into()),
            ..Settings::default()
        };
        assert_eq!(
            plan_command(&task, &settings).program,
            OsString::from("C:\\py\\python.exe")
        );
        assert_eq!(
            plan_command(&task, &Settings::default()).program,
            OsString::from(default_python())
        );
    }

    #[test]
    fn batch_and_exe_run_the_file_directly() {
        for file in ["run.bat", "run.cmd", "tool.exe"] {
            let mut task = Task::new("t", file);
            task.args = vec!["one".into()];
            let plan = plan_command(&task, &Settings::default());
            assert_eq!(plan.program, OsString::from(file));
            assert_eq!(plan.args, os(&["one"]));
        }
    }

    #[test]
    fn powershell_bypasses_execution_policy_for_the_file() {
        let task = Task::new("t", "report.ps1");
        let plan = plan_command(&task, &Settings::default());
        assert_eq!(plan.program, OsString::from("powershell.exe"));
        assert_eq!(
            plan.args,
            os(&[
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                "report.ps1"
            ])
        );
    }

    #[test]
    fn working_dir_defaults_to_script_directory() {
        let mut task = Task::new("t", "/opt/jobs/a.py");
        assert_eq!(
            plan_command(&task, &Settings::default()).cwd,
            Some(PathBuf::from("/opt/jobs"))
        );
        task.working_dir = Some(PathBuf::from("/data"));
        assert_eq!(
            plan_command(&task, &Settings::default()).cwd,
            Some(PathBuf::from("/data"))
        );
        assert_eq!(plan_command(&Task::new("t", "a.py"), &Settings::default()).cwd, None);
    }

    #[test]
    fn eol_trimming_handles_lf_and_crlf() {
        assert_eq!(trim_eol(b"hi\r\n"), b"hi");
        assert_eq!(trim_eol(b"hi\n"), b"hi");
        assert_eq!(trim_eol(b"hi"), b"hi");
    }
}

#[cfg(all(test, unix))]
mod process_tests {
    use std::time::Instant;

    use super::*;

    fn sh(script: &str) -> CommandPlan {
        CommandPlan {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![],
            cwd: None,
        }
    }

    async fn run(
        plan: CommandPlan,
        timeout: Option<Duration>,
        cancel: &CancellationToken,
    ) -> (RunOutcome, Vec<LogLine>) {
        let (tx, mut rx) = mpsc::channel(64);
        let process = spawn(&plan, Some(tx)).expect("spawn");
        let outcome = process.wait(timeout, cancel).await;
        let mut lines = Vec::new();
        while let Some(line) = rx.recv().await {
            lines.push(line);
        }
        (outcome, lines)
    }

    #[tokio::test]
    async fn captures_both_streams_and_success() {
        let (outcome, lines) = run(sh("echo out; echo err >&2"), None, &CancellationToken::new()).await;
        assert_eq!(outcome.status, ExecutionStatus::Success);
        assert_eq!(outcome.exit_code, Some(0));
        assert!(lines.iter().any(|l| l.stream == LogStream::Stdout && l.text == "out"));
        assert!(lines.iter().any(|l| l.stream == LogStream::Stderr && l.text == "err"));
    }

    #[tokio::test]
    async fn non_zero_exit_is_a_failure_with_code() {
        let (outcome, _) = run(sh("exit 3"), None, &CancellationToken::new()).await;
        assert_eq!(outcome.status, ExecutionStatus::Failed);
        assert_eq!(outcome.exit_code, Some(3));
        assert_eq!(outcome.failure_reason.as_deref(), Some("exited with code 3"));
    }

    #[tokio::test]
    async fn spawn_failure_is_reported() {
        let plan = CommandPlan {
            program: "/definitely/not/here".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        assert!(spawn(&plan, None).is_err());
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_tree() {
        // Print the grandchild's pid, then wait on it for a minute.
        let started = Instant::now();
        let (outcome, lines) = run(
            sh("sleep 60 & echo $!; wait"),
            Some(Duration::from_millis(400)),
            &CancellationToken::new(),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(outcome.status, ExecutionStatus::Failed);
        assert_eq!(outcome.failure_reason.as_deref(), Some("timed out after 0s"));

        let grandchild: i32 = lines[0].text.trim().parse().expect("grandchild pid");
        let mut gone = false;
        for _ in 0..50 {
            // SAFETY: signal 0 only probes for existence.
            if unsafe { libc::kill(grandchild, 0) } == -1 {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "grandchild process survived the timeout");
    }

    #[tokio::test]
    async fn cancellation_interrupts_the_run() {
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            trigger.cancel();
        });
        let (outcome, _) = run(sh("sleep 60"), None, &cancel).await;
        assert_eq!(outcome.status, ExecutionStatus::Interrupted);
    }

    #[tokio::test]
    async fn detached_child_holding_the_pipes_does_not_hang_us() {
        let started = Instant::now();
        let (outcome, lines) = run(sh("sleep 30 & echo done"), None, &CancellationToken::new()).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(outcome.status, ExecutionStatus::Success);
        assert!(lines.iter().any(|l| l.text == "done"));
    }

    #[tokio::test]
    async fn overlong_lines_are_split_and_output_without_sink_does_not_block() {
        // ~200 KB without a newline.
        let script = "head -c 200000 /dev/zero | tr '\\0' 'a'; echo";
        let (_, lines) = run(sh(script), None, &CancellationToken::new()).await;
        assert!(lines.len() >= 3);
        assert!(lines.iter().all(|l| l.text.len() <= MAX_LINE_BYTES as usize));

        let process = spawn(&sh(script), None).unwrap();
        let outcome = process
            .wait(Some(Duration::from_secs(10)), &CancellationToken::new())
            .await;
        assert_eq!(outcome.status, ExecutionStatus::Success);
    }

    #[tokio::test]
    async fn non_utf8_output_is_decoded_lossily_on_unix() {
        let (_, lines) = run(sh("printf 'caf\\351\\n'"), None, &CancellationToken::new()).await;
        assert_eq!(lines[0].text, "caf\u{fffd}");
    }
}
