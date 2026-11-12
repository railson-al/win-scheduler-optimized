//! Starting a task's script in the session of the user who is logged on (`RunAs::UserSession`).
//! Windows only.
//!
//! The scheduler is a service: LocalSystem, in session 0, which since Windows Vista has no desktop. A
//! program started there works but cannot show a window to anyone (a browser opened by a script is real
//! and invisible), and it sees the machine's profile, not a person's. To show a window, the process has to
//! be created in the user's session, with the user's token. `std::process::Command` and `tokio::process`
//! cannot do that, so this module calls `CreateProcessAsUserW` itself:
//!
//! 1. find the user's session (the console, else an active remote desktop) and ask for their token;
//! 2. if it is the elevated half of an administrator's split token, take the limited half instead, which
//!    is what a double click gives (a browser, mapped network drives and the user's OneDrive behave as the
//!    user expects, and the script does not run with more rights than they normally have);
//! 3. read the user's environment, find the program on the *user's* `PATH`, build the command line;
//! 4. create the output pipes and start the process suspended on `winsta0\default`, handing it exactly
//!    three handles (nul as stdin, the two pipes) so it cannot inherit anything else this service holds;
//! 5. put it in a job object (so stopping a task kills what it started), then let it run.
//!
//! Nothing here can be run on Linux, and it has to be checked on a real Windows machine with a user logged
//! on. The text-shaped parts (command line, environment, program search) are in `winlaunch.rs`, which is
//! tested everywhere.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::ptr::{null, null_mut};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SECURITY_ATTRIBUTES, SecurityImpersonation, TOKEN_ALL_ACCESS,
    TOKEN_LINKED_TOKEN, TokenElevationType, TokenElevationTypeFull, TokenLinkedToken, TokenPrimary,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW, WTSActive, WTSEnumerateSessionsW, WTSFreeMemory,
    WTSGetActiveConsoleSessionId, WTSQueryUserToken,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
    InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::tree::ProcessTree;
use super::winlaunch::{
    batch_command_line, command_line, encode_environment_block, environment_value, is_batch, merge_environment,
    parse_environment_block, resolve_program,
};
use super::{CommandPlan, LogLine, LogStream, Process, RunningProcess, pump};

/// "No session" from `WTSGetActiveConsoleSessionId`.
const NO_SESSION: u32 = 0xFFFF_FFFF;

const NO_USER: &str = "no user is logged on to a desktop session, so a task set to run in the user's session \
                       cannot start (log in, or set the task to run as the service)";

/// An open Win32 handle, closed when dropped.
struct Handle(HANDLE);

// A handle is a kernel object reference; any thread may use or close it.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    /// Gives the handle up without closing it (it now belongs to whoever takes it).
    fn into_raw(mut self) -> HANDLE {
        std::mem::replace(&mut self.0, null_mut())
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: a handle this value owns, closed exactly once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// The last OS error, saying what was being done.
fn failed(what: &str) -> io::Error {
    let error = io::Error::last_os_error();
    io::Error::new(error.kind(), format!("{what}: {error}"))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The token of the user who is logged on, or `NotFound` when nobody is.
fn user_token() -> io::Result<Handle> {
    // SAFETY: a plain query with no arguments.
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    if console != NO_SESSION
        && let Ok(token) = query_token(console)
    {
        return Ok(token);
    }
    // Nobody at the console: a user may be on a remote desktop session.
    for session in active_sessions() {
        if let Ok(token) = query_token(session) {
            return Ok(token);
        }
    }
    Err(io::Error::new(io::ErrorKind::NotFound, NO_USER))
}

fn query_token(session: u32) -> io::Result<Handle> {
    let mut token: HANDLE = null_mut();
    // SAFETY: `token` is a valid place for the handle. Needs SeTcbPrivilege, which LocalSystem has.
    if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
        return Err(failed("getting the logged-on user's token"));
    }
    Ok(Handle(token))
}

/// Ids of the sessions in the Active state, other than session 0 (the service's own).
fn active_sessions() -> Vec<u32> {
    let mut info: *mut WTS_SESSION_INFOW = null_mut();
    let mut count = 0u32;
    // SAFETY: both out-parameters are valid; the list is freed below with `WTSFreeMemory`.
    let listed = unsafe { WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut info, &mut count) };
    if listed == 0 || info.is_null() {
        return Vec::new();
    }
    // SAFETY: the call succeeded, so `info` points at `count` entries until freed.
    let sessions = unsafe { std::slice::from_raw_parts(info, count as usize) };
    let ids = sessions
        .iter()
        .filter(|session| session.State == WTSActive && session.SessionId != 0)
        .map(|session| session.SessionId)
        .collect();
    // SAFETY: freeing the buffer `WTSEnumerateSessionsW` gave us, once.
    unsafe { WTSFreeMemory(info as *mut c_void) };
    ids
}

/// The token the user would get by double-clicking: for an administrator with a split token, the limited
/// half (see the module notes). Anyone else's token is returned as it is.
fn as_a_double_click_would(token: Handle) -> io::Result<Handle> {
    let mut kind = 0i32;
    let mut returned = 0u32;
    // SAFETY: the buffer is an `i32`, the size of what `TokenElevationType` returns.
    let known = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevationType,
            (&mut kind as *mut i32).cast::<c_void>(),
            size_of::<i32>() as u32,
            &mut returned,
        )
    };
    if known == 0 || kind != TokenElevationTypeFull {
        return Ok(token);
    }
    let mut linked = TOKEN_LINKED_TOKEN {
        LinkedToken: null_mut(),
    };
    // SAFETY: the buffer is a `TOKEN_LINKED_TOKEN`, what `TokenLinkedToken` returns.
    let got = unsafe {
        GetTokenInformation(
            token.0,
            TokenLinkedToken,
            (&mut linked as *mut TOKEN_LINKED_TOKEN).cast::<c_void>(),
            size_of::<TOKEN_LINKED_TOKEN>() as u32,
            &mut returned,
        )
    };
    if got == 0 {
        return Err(failed("getting the user's non-elevated token"));
    }
    let linked = Handle(linked.LinkedToken);
    // The linked token is an impersonation token; a process needs a primary one.
    let mut primary: HANDLE = null_mut();
    // SAFETY: valid token handle in, valid place for the new handle out.
    let duplicated = unsafe {
        DuplicateTokenEx(
            linked.0,
            TOKEN_ALL_ACCESS,
            null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    };
    if duplicated == 0 {
        return Err(failed("making the user's non-elevated token usable for a process"));
    }
    Ok(Handle(primary))
}

/// The user's environment variables, as `CreateEnvironmentBlock` builds them for that token.
fn user_environment(token: &Handle) -> io::Result<Vec<(String, String)>> {
    let mut block: *mut c_void = null_mut();
    // SAFETY: a valid token and a valid place for the pointer; destroyed below.
    if unsafe { CreateEnvironmentBlock(&mut block, token.0, 0) } == 0 {
        return Err(failed("reading the user's environment"));
    }
    // SAFETY: the call succeeded, so `block` is a double-NUL-terminated UTF-16 list until destroyed.
    let units = unsafe { read_block(block.cast::<u16>()) };
    // SAFETY: destroying the block we were given, once, after copying it.
    unsafe { DestroyEnvironmentBlock(block) };
    Ok(parse_environment_block(&units))
}

/// Copies an environment block (up to and including its double NUL).
///
/// # Safety
/// `block` must point at a UTF-16 list ended by two NULs in a row.
unsafe fn read_block(block: *const u16) -> Vec<u16> {
    let mut units = Vec::new();
    let mut index = 0;
    loop {
        // SAFETY: the caller guarantees the list ends with two NULs, which stops the loop before the end.
        let unit = unsafe { *block.add(index) };
        units.push(unit);
        if unit == 0 && index > 0 && units[index - 1] == 0 {
            return units;
        }
        index += 1;
    }
}

/// An output pipe: the end the service reads, and the end the script writes.
struct Pipe {
    read: Handle,
    write: Handle,
}

fn inheritable() -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    }
}

fn pipe() -> io::Result<Pipe> {
    let attributes = inheritable();
    let (mut read, mut write): (HANDLE, HANDLE) = (null_mut(), null_mut());
    // SAFETY: valid places for both handles and valid attributes.
    if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
        return Err(failed("creating an output pipe"));
    }
    let pipe = Pipe {
        read: Handle(read),
        write: Handle(write),
    };
    // The service's end must not be inherited, or the pipe would never report that the script is done.
    // SAFETY: a handle we own.
    if unsafe { SetHandleInformation(pipe.read.0, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(failed("keeping the pipe's reading end private"));
    }
    Ok(pipe)
}

/// A read-only handle on the `NUL` device, which the script gets as its standard input.
fn nul_device() -> io::Result<Handle> {
    let attributes = inheritable();
    let name = wide("NUL");
    // SAFETY: a NUL-terminated name and valid attributes.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(failed("opening the NUL device"));
    }
    Ok(Handle(handle))
}

/// A process attribute list naming the handles the new process may inherit, and only those. Without it,
/// `bInheritHandles` would pass on every inheritable handle this service holds at that moment, among them
/// the pipes of other scripts starting at the same time.
struct AttributeList {
    buffer: Vec<u64>,
}

impl AttributeList {
    /// `handles` must stay alive (and unmoved) until the process has been created: the list refers to it.
    fn inheriting(handles: &[HANDLE]) -> io::Result<Self> {
        let mut size = 0usize;
        // Asking for the size fails on purpose (ERROR_INSUFFICIENT_BUFFER); `size` is what matters.
        // SAFETY: a null list with a size out-parameter is how the size is queried.
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size) };
        if size == 0 {
            return Err(failed("sizing the process attribute list"));
        }
        // u64 elements keep the buffer 8-byte aligned.
        let mut buffer = vec![0u64; size.div_ceil(size_of::<u64>())];
        // SAFETY: the buffer is at least `size` bytes, aligned.
        if unsafe { InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast::<c_void>(), 1, 0, &mut size) } == 0 {
            return Err(failed("creating the process attribute list"));
        }
        // From here `Drop` deletes the (initialised) list.
        let list = Self { buffer };
        // SAFETY: the list is initialised; `handles` is a valid slice that outlives the process creation.
        let updated = unsafe {
            UpdateProcThreadAttribute(
                list.pointer(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast::<c_void>(),
                std::mem::size_of_val(handles),
                null_mut(),
                null(),
            )
        };
        if updated == 0 {
            return Err(failed("listing the handles the script inherits"));
        }
        Ok(list)
    }

    fn pointer(&self) -> *mut c_void {
        self.buffer.as_ptr() as *mut c_void
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: the list was initialised in `inheriting`, and is deleted once.
        unsafe { DeleteProcThreadAttributeList(self.pointer()) };
    }
}

/// A script running in the user's session. Dropping it before it has finished ends it.
pub struct UserProcess {
    process: Arc<Handle>,
    exit: oneshot::Receiver<u32>,
    finished: bool,
}

impl UserProcess {
    /// Waits for the exit code. Safe to abandon and call again.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        let code = (&mut self.exit)
            .await
            .map_err(|_| io::Error::other("lost track of the process that was started in the user's session"))?;
        self.finished = true;
        Ok(ExitStatus::from_raw(code))
    }

    /// Ends the main process (what it started is ended through the job object).
    pub fn kill(&self) {
        // SAFETY: a live process handle; failing because it already ended is harmless.
        unsafe {
            TerminateProcess(self.process.0, 1);
        }
    }
}

impl Drop for UserProcess {
    fn drop(&mut self) {
        if !self.finished {
            self.kill();
        }
    }
}

/// Starts `plan` in the logged-on user's session. See the module notes for the steps.
pub fn spawn(plan: &CommandPlan, logs: Option<mpsc::Sender<LogLine>>) -> io::Result<RunningProcess> {
    let token = as_a_double_click_would(user_token()?)?;
    let user_variables = user_environment(&token)?;

    let program = plan.program.to_string_lossy().into_owned();
    let args: Vec<String> = plan.args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
    let overrides: Vec<(String, String)> = plan
        .env
        .iter()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect();
    let cwd = plan.cwd.as_ref().map(|dir| dir.to_string_lossy().into_owned());
    let environment = merge_environment(user_variables, &overrides);

    // The program is searched on the user's PATH, not the service's.
    let resolved = resolve_program(
        &program,
        environment_value(&environment, "PATH").unwrap_or(""),
        environment_value(&environment, "PATHEXT").unwrap_or(".COM;.EXE;.BAT;.CMD"),
        cwd.as_deref(),
        |candidate| Path::new(candidate).is_file(),
    )
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{program:?} was not found on the user's PATH"),
        )
    })?;

    let line = if is_batch(&resolved) {
        let comspec = environment_value(&environment, "ComSpec").unwrap_or(r"C:\Windows\System32\cmd.exe");
        batch_command_line(comspec, &resolved, &args)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?
    } else {
        command_line(&resolved, &args)
    };

    let stdin = nul_device()?;
    let stdout = pipe()?;
    let stderr = pipe()?;
    // Declared before the attribute list, which refers to it, so it is dropped after it.
    let inherited = [stdin.0, stdout.write.0, stderr.write.0];
    let attributes = AttributeList::inheriting(&inherited)?;

    let mut command = wide(&line);
    let mut desktop = wide(r"winsta0\default");
    let environment_block = encode_environment_block(&environment);
    let cwd_wide = cwd.as_deref().map(wide);

    // SAFETY: an all-zero STARTUPINFOEXW is the documented starting point.
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.lpDesktop = desktop.as_mut_ptr();
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdin.0;
    startup.StartupInfo.hStdOutput = stdout.write.0;
    startup.StartupInfo.hStdError = stderr.write.0;
    startup.lpAttributeList = attributes.pointer();

    // SAFETY: an all-zero PROCESS_INFORMATION is a valid out-parameter.
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: every pointer is valid for the call: `command` is a mutable NUL-terminated buffer, the
    // environment block is double-NUL-terminated, and `startup` is an extended structure (flagged by
    // EXTENDED_STARTUPINFO_PRESENT) whose attribute list and handles outlive the call.
    let created = unsafe {
        CreateProcessAsUserW(
            token.0,
            null(),
            command.as_mut_ptr(),
            null(),
            null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | CREATE_SUSPENDED,
            environment_block.as_ptr().cast::<c_void>(),
            cwd_wide.as_ref().map_or(null(), |dir| dir.as_ptr()),
            (&startup as *const STARTUPINFOEXW).cast(),
            &mut info,
        )
    };
    if created == 0 {
        return Err(failed(&format!("starting {resolved:?} in the user's session")));
    }

    // The process exists, suspended. From here, any failure must end it (`UserProcess` does that on drop).
    let thread = Handle(info.hThread);
    let process = Arc::new(Handle(info.hProcess));
    let (sender, exit) = oneshot::channel();
    let waiting_on = Arc::clone(&process);
    tokio::task::spawn_blocking(move || {
        let mut code = 0u32;
        // SAFETY: a live process handle, kept open by the `Arc` for the whole wait.
        unsafe {
            WaitForSingleObject(waiting_on.0, INFINITE);
            GetExitCodeProcess(waiting_on.0, &mut code);
        }
        let _ = sender.send(code);
    });
    let user_process = UserProcess {
        process: Arc::clone(&process),
        exit,
        finished: false,
    };

    let tree = ProcessTree::attach_raw(process.0);
    // SAFETY: the thread handle of the process just created.
    if unsafe { ResumeThread(thread.0) } == u32::MAX {
        return Err(failed("letting the script start running"));
    }
    drop(thread);

    // The service's copies of the script's ends must go, or the pipes would never close.
    drop(stdin);
    drop(stdout.write);
    drop(stderr.write);
    let mut readers = Vec::with_capacity(2);
    for (pipe_end, stream) in [(stdout.read, LogStream::Stdout), (stderr.read, LogStream::Stderr)] {
        // SAFETY: a pipe handle we own, whose ownership passes to the `File`.
        let file = unsafe { File::from_raw_handle(pipe_end.into_raw() as RawHandle) };
        readers.push(tokio::spawn(pump(
            tokio::fs::File::from_std(file),
            stream,
            logs.clone(),
        )));
    }

    Ok(RunningProcess {
        process: Process::UserSession(user_process),
        tree,
        readers,
        pid: Some(info.dwProcessId),
    })
}
