//! Installing, starting, stopping and removing the service, and locking down
//! its data directory. Everything here needs an elevated (Administrator) process.

use std::ffi::{OsStr, c_void};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use scriptrunner_core::SERVICE_NAME;
use windows_service::service::{
    Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::Authorization::{ConvertStringSidToSidW, GetNamedSecurityInfoW};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::Security::{EqualSid, OWNER_SECURITY_INFORMATION, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT, SERVICE_CHANGE_CONFIG,
    SERVICE_CONFIG_PRESHUTDOWN_INFO, SERVICE_PRESHUTDOWN_INFO,
};

const DISPLAY_NAME: &str = "ScriptRunner";
const DESCRIPTION: &str = "Schedules and runs scripts (.py, .bat, .cmd, .ps1, .exe) and keeps their history and logs.";

/// `ERROR_ACCESS_DENIED` and `ERROR_SERVICE_DOES_NOT_EXIST`.
const ACCESS_DENIED: i32 = 5;
const SERVICE_DOES_NOT_EXIST: i32 = 1060;
/// `ERROR_SERVICE_ALREADY_RUNNING`, `ERROR_SERVICE_NOT_ACTIVE` and `ERROR_SERVICE_EXISTS`.
const SERVICE_ALREADY_RUNNING: i32 = 1056;
const SERVICE_NOT_ACTIVE: i32 = 1062;
const SERVICE_EXISTS: i32 = 1073;

/// How long Windows waits for the service to wind down at a machine shutdown
/// (running scripts are stopped, the database is closed) before it kills it.
const PRESHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const WAIT_FOR_STATE: Duration = Duration::from_secs(30);

/// Restart after 5 s, 30 s and 60 s; the failure count resets after a day without failures.
const RESTART_DELAYS: [Duration; 3] = [Duration::from_secs(5), Duration::from_secs(30), Duration::from_secs(60)];
const FAILURE_RESET_AFTER: Duration = Duration::from_secs(24 * 3600);

/// Well-known SIDs of the accounts allowed to own the data directory.
const SYSTEM_SID: &str = "S-1-5-18";
const ADMINISTRATORS_SID: &str = "S-1-5-32-544";

/// Where the service's own copy of the executable lives. `%ProgramFiles%` is
/// writable only by administrators, which a build or download folder is not.
const INSTALL_DIR_NAME: &str = "ScriptRunner";
const INSTALLED_EXE_NAME: &str = "scriptrunner.exe";

/// A protected DACL (`P`, no inherited entries) that SYSTEM and Administrators
/// own completely, inherited by everything created inside (`OICI`).
const DATA_DIR_SDDL: &str = "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    NotInstalled,
    Stopped,
    StartPending,
    Running,
    StopPending,
    Other,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotInstalled => "not installed",
            Self::Stopped => "stopped",
            Self::StartPending => "starting",
            Self::Running => "running",
            Self::StopPending => "stopping",
            Self::Other => "in a state this tool does not recognise",
        })
    }
}

/// Registers the service (LocalSystem, automatic start, restart on failure).
///
/// Order matters. The privilege check (opening the manager with CREATE_SERVICE)
/// comes first, so a regular user fails before anything is written. Then the data
/// directory is locked down before the service can create the database in it, and
/// the executable is copied somewhere only administrators can write (the service
/// runs it as SYSTEM). If a step fails after the service exists, it is removed
/// again, so no half-configured service is left behind.
pub fn install(source_executable: &Path, data_dir: &Path) -> Result<()> {
    let manager = open_manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    if status()? != Status::NotInstalled {
        bail!("the {SERVICE_NAME} service is already installed; run `scriptrunner service uninstall` first");
    }
    secure_data_dir(data_dir).context("restricting the data directory to SYSTEM and Administrators")?;
    let executable = install_binary(source_executable).context("copying the executable to Program Files")?;

    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: executable.clone(),
        launch_arguments: vec![
            "service".into(),
            "run".into(),
            "--data-dir".into(),
            data_dir.as_os_str().to_owned(),
        ],
        dependencies: vec![],
        // `None` means LocalSystem.
        account_name: None,
        account_password: None,
    };
    // START is not for starting the service here: Windows refuses to set restart-on-failure actions
    // (`configure`) on a handle that lacks it, with ERROR_ACCESS_DENIED, even for an administrator.
    // The `windows-service` documentation shows the same pair, START | CHANGE_CONFIG, for that call.
    let service = manager
        .create_service(
            &info,
            ServiceAccess::CHANGE_CONFIG | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE | ServiceAccess::START,
        )
        .map_err(explain)
        .context("creating the service")?;

    if let Err(err) = configure(&service) {
        // Do not leave behind a service that starts at boot without its recovery settings.
        let _ = service.delete();
        remove_binary();
        return Err(err);
    }
    Ok(())
}

/// Each step says what it was doing: `explain` alone turns any ERROR_ACCESS_DENIED into "run this command as
/// Administrator", which misleads when the caller already is one and the handle merely lacks a right.
fn configure(service: &Service) -> Result<()> {
    service
        .set_description(DESCRIPTION)
        .map_err(explain)
        .context("setting the service description")?;
    service
        .update_failure_actions(ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(FAILURE_RESET_AFTER),
            reboot_msg: None,
            command: None,
            actions: Some(
                RESTART_DELAYS
                    .iter()
                    .map(|delay| ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: *delay,
                    })
                    .collect(),
            ),
        })
        .map_err(explain)
        .context("setting the restart-on-failure actions")?;
    // A non-zero exit code also counts as a failure, not only a crash.
    service
        .set_failure_actions_on_non_crash_failures(true)
        .map_err(explain)
        .context("applying the failure actions to non-zero exit codes")?;
    set_preshutdown_timeout(PRESHUTDOWN_TIMEOUT).context("setting the shutdown timeout")?;
    Ok(())
}

/// Stops the service if it is running, then removes it. The data directory is kept.
pub fn uninstall() -> Result<()> {
    let manager = open_manager(ServiceManagerAccess::CONNECT)?;
    let service = open(
        &manager,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    )?;
    if state_of(&service)? != ServiceState::Stopped {
        let _ = service.stop();
        wait_for(&service, ServiceState::Stopped)?;
    }
    service.delete().map_err(explain).context("deleting the service")?;
    remove_binary();
    Ok(())
}

/// Copies the running executable to `%ProgramFiles%\ScriptRunner` and returns the
/// copy. The service is registered with *that* path: an executable in a folder an
/// ordinary user can write to (Downloads, a build directory) would let them swap
/// it for their own code, which the service then runs as SYSTEM.
fn install_binary(source: &Path) -> Result<PathBuf> {
    let base = std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .map(PathBuf::from)
        .context("%ProgramFiles% is not set")?;
    let dir = base.join(INSTALL_DIR_NAME);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let target = dir.join(INSTALLED_EXE_NAME);
    // Running the installed copy itself (e.g. to reinstall) needs no copy.
    if std::fs::canonicalize(source).ok() != std::fs::canonicalize(&target).ok() {
        std::fs::copy(source, &target)
            .with_context(|| format!("copying {} to {}", source.display(), target.display()))?;
    }
    Ok(target)
}

/// Best effort: the service is already gone, and a leftover file does no harm.
fn remove_binary() {
    let Some(base) = std::env::var_os("ProgramW6432").or_else(|| std::env::var_os("ProgramFiles")) else {
        return;
    };
    let dir = PathBuf::from(base).join(INSTALL_DIR_NAME);
    let _ = std::fs::remove_file(dir.join(INSTALLED_EXE_NAME));
    let _ = std::fs::remove_dir(&dir);
}

pub fn start() -> Result<()> {
    let manager = open_manager(ServiceManagerAccess::CONNECT)?;
    let service = open(&manager, ServiceAccess::QUERY_STATUS | ServiceAccess::START)?;
    service
        .start::<&OsStr>(&[])
        .map_err(explain)
        .context("starting the service")?;
    wait_for(&service, ServiceState::Running)
}

pub fn stop() -> Result<()> {
    let manager = open_manager(ServiceManagerAccess::CONNECT)?;
    let service = open(&manager, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP)?;
    service.stop().map_err(explain).context("stopping the service")?;
    wait_for(&service, ServiceState::Stopped)
}

/// Does not need elevation: querying a service's state is open to any user.
pub fn status() -> Result<Status> {
    let manager = open_manager(ServiceManagerAccess::CONNECT)?;
    match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(service) => Ok(match state_of(&service)? {
            ServiceState::Stopped => Status::Stopped,
            ServiceState::StartPending => Status::StartPending,
            ServiceState::Running => Status::Running,
            ServiceState::StopPending => Status::StopPending,
            _ => Status::Other,
        }),
        Err(windows_service::Error::Winapi(err)) if err.raw_os_error() == Some(SERVICE_DOES_NOT_EXIST) => {
            Ok(Status::NotInstalled)
        }
        Err(err) => Err(explain(err)).context("querying the service"),
    }
}

/// Makes `dir` a SYSTEM-and-Administrators-only directory, and refuses to use
/// it if anyone else set it up. Safe to repeat; the service does it at every start.
///
/// * A directory that does not exist is created *with* the restrictive DACL in a
///   single call, so there is never a moment when it is open to other users.
/// * One that already exists must not be a link and must be owned by SYSTEM or
///   Administrators. Any user can create folders in %ProgramData%, and an owner
///   can always rewrite the DACL, so a folder pre-created by a user is an attack:
///   it is refused, not "fixed".
/// * Once the DACL is applied, every entry inside must be owned by SYSTEM or
///   Administrators too: a file planted earlier, with its own protected DACL,
///   would not pick up the inherited one.
pub fn secure_data_dir(dir: &Path) -> Result<()> {
    let trusted = [Sid::parse(SYSTEM_SID)?, Sid::parse(ADMINISTRATORS_SID)?];
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => {
            if is_reparse_point(&meta) {
                bail!("{} is a link; refusing to use it as the data directory", dir.display());
            }
            if !meta.is_dir() {
                bail!("{} is not a directory", dir.display());
            }
            if !owned_by(dir, &trusted)? {
                bail!(
                    "{} is owned by an account other than SYSTEM or Administrators, so a regular user may \
                     have set it up; move or delete it and try again",
                    dir.display()
                );
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => create_private_dir(dir)?,
        Err(err) => return Err(err).with_context(|| format!("inspecting {}", dir.display())),
    }
    apply_dacl(dir)?;
    check_entries(dir, &trusted)
}

/// Creates `dir` already carrying the restrictive DACL. If someone else creates
/// it first, this fails (closed); the next attempt then hits the owner check.
fn create_private_dir(dir: &Path) -> Result<()> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let sddl = wide(OsStr::new(DATA_DIR_SDDL));
    let mut descriptor: *mut c_void = ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated and outlives the call; `descriptor` is a valid out-pointer.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error()).context("building the directory's security descriptor");
    }
    let _free = LocalAlloc(descriptor);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let path = wide(dir.as_os_str());
    // SAFETY: `path` is NUL-terminated; `attributes` points at `descriptor`, alive until `_free` drops.
    if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error()).with_context(|| format!("creating {}", dir.display()));
    }
    Ok(())
}

/// Refuses links and anything owned by an account other than the trusted ones.
fn check_entries(dir: &Path, trusted: &[Sid]) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let meta = std::fs::symlink_metadata(&path).with_context(|| format!("inspecting {}", path.display()))?;
        if is_reparse_point(&meta) {
            bail!(
                "{} is a link inside the data directory; refusing to continue",
                path.display()
            );
        }
        if !owned_by(&path, trusted)? {
            bail!(
                "{} is owned by an account other than SYSTEM or Administrators; delete it and try again",
                path.display()
            );
        }
        if meta.is_dir() {
            check_entries(&path, trusted)?;
        }
    }
    Ok(())
}

fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    /// `FILE_ATTRIBUTE_REPARSE_POINT`: symlinks and junctions.
    const REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & REPARSE_POINT != 0
}

fn owned_by(path: &Path, trusted: &[Sid]) -> Result<bool> {
    let wide_path = wide(path.as_os_str());
    let mut owner: *mut c_void = ptr::null_mut();
    let mut descriptor: *mut c_void = ptr::null_mut();
    // SAFETY: `wide_path` is NUL-terminated and outlives the call; the out-pointers are valid locals.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32))
            .with_context(|| format!("reading the owner of {}", path.display()));
    }
    let _free = LocalAlloc(descriptor);
    // SAFETY: `owner` points into `descriptor`, which lives until `_free` drops, and the trusted SIDs are valid.
    Ok(trusted.iter().any(|sid| unsafe { EqualSid(owner, sid.0) } != 0))
}

/// A SID parsed from its string form; freed on drop.
struct Sid(*mut c_void);

impl Sid {
    fn parse(text: &str) -> Result<Self> {
        let wide_text = wide(OsStr::new(text));
        let mut sid: *mut c_void = ptr::null_mut();
        // SAFETY: `wide_text` is NUL-terminated and outlives the call; `sid` is a valid out-pointer.
        if unsafe { ConvertStringSidToSidW(wide_text.as_ptr(), &mut sid) } == 0 {
            return Err(io::Error::last_os_error()).with_context(|| format!("parsing the SID {text}"));
        }
        Ok(Self(sid))
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by `ConvertStringSidToSidW`, which documents `LocalFree`, and freed once, here.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// Applies the DACL to the directory and, through inheritance, to what is inside it.
fn apply_dacl(dir: &Path) -> Result<()> {
    let sddl = wide(OsStr::new(DATA_DIR_SDDL));
    let mut descriptor: *mut c_void = ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated and outlives the call; `descriptor` is a valid out-pointer.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error()).context("building the directory's security descriptor");
    }
    let _free = LocalAlloc(descriptor);

    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl: *mut ACL = ptr::null_mut();
    // SAFETY: `descriptor` is valid (checked above) and the out-pointers are valid locals.
    let got = unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) };
    if got == 0 || present == 0 || dacl.is_null() {
        bail!("the directory's security descriptor has no DACL");
    }

    let path = wide(dir.as_os_str());
    // SAFETY: `path` is NUL-terminated and outlives the call; `dacl` points into `descriptor`, still alive.
    // Owner, group and SACL are left alone (null). PROTECTED stops inheriting from %ProgramData%.
    let status = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32))
            .with_context(|| format!("setting permissions on {}", dir.display()));
    }
    Ok(())
}

/// Frees a block allocated by the security APIs, on drop.
struct LocalAlloc(*mut c_void);

impl Drop for LocalAlloc {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`, freed once, here.
            unsafe { LocalFree(self.0) };
        }
    }
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

fn set_preshutdown_timeout(timeout: Duration) -> Result<()> {
    let name = wide(OsStr::new(SERVICE_NAME));
    // SAFETY: plain SCM calls with valid, NUL-terminated arguments. Both handles are closed on every path below.
    unsafe {
        let manager = OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT);
        if manager.is_null() {
            return Err(io::Error::last_os_error()).context("opening the service manager");
        }
        let service = OpenServiceW(manager, name.as_ptr(), SERVICE_CHANGE_CONFIG);
        let outcome = if service.is_null() {
            Err(io::Error::last_os_error())
        } else {
            let info = SERVICE_PRESHUTDOWN_INFO {
                dwPreshutdownTimeout: u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX),
            };
            let changed = ChangeServiceConfig2W(service, SERVICE_CONFIG_PRESHUTDOWN_INFO, ptr::from_ref(&info).cast());
            let outcome = if changed == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            };
            CloseServiceHandle(service);
            outcome
        };
        CloseServiceHandle(manager);
        outcome.map_err(Into::into)
    }
}

fn open_manager(access: ServiceManagerAccess) -> Result<ServiceManager> {
    ServiceManager::local_computer(None::<&str>, access)
        .map_err(explain)
        .context("connecting to the Service Control Manager")
}

fn open(manager: &ServiceManager, access: ServiceAccess) -> Result<Service> {
    manager.open_service(SERVICE_NAME, access).map_err(explain)
}

fn state_of(service: &Service) -> Result<ServiceState> {
    Ok(service.query_status().map_err(explain)?.current_state)
}

fn wait_for(service: &Service, wanted: ServiceState) -> Result<()> {
    let deadline = Instant::now() + WAIT_FOR_STATE;
    loop {
        let state = state_of(service)?;
        if state == wanted {
            return Ok(());
        }
        if wanted == ServiceState::Running && state == ServiceState::Stopped {
            bail!(
                "the service stopped right after starting. The reason is in the service log under the data \
                 directory (`service\\`); `scriptrunner service install` also re-checks the data directory \
                 and reports what is wrong with it"
            );
        }
        if Instant::now() >= deadline {
            bail!(
                "the service did not reach the expected state within {} s",
                WAIT_FOR_STATE.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Turns the two errors a user can actually fix into plain advice.
fn explain(err: windows_service::Error) -> anyhow::Error {
    match &err {
        windows_service::Error::Winapi(io) if io.raw_os_error() == Some(ACCESS_DENIED) => {
            anyhow!("access denied: run this command as Administrator")
        }
        windows_service::Error::Winapi(io) if io.raw_os_error() == Some(SERVICE_DOES_NOT_EXIST) => {
            anyhow!("the {SERVICE_NAME} service is not installed (use `scriptrunner service install`)")
        }
        windows_service::Error::Winapi(io) if io.raw_os_error() == Some(SERVICE_EXISTS) => {
            anyhow!("the {SERVICE_NAME} service is already installed")
        }
        windows_service::Error::Winapi(io) if io.raw_os_error() == Some(SERVICE_ALREADY_RUNNING) => {
            anyhow!("the {SERVICE_NAME} service is already running")
        }
        windows_service::Error::Winapi(io) if io.raw_os_error() == Some(SERVICE_NOT_ACTIVE) => {
            anyhow!("the {SERVICE_NAME} service is not running")
        }
        _ => anyhow!(err),
    }
}
