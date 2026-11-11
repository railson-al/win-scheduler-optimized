//! Asks the Service Control Manager which process is the ScriptRunner service
//! (Windows only). The client uses it to refuse a pipe that some other process
//! created first: the pipe name is fixed, so without this check any local
//! process could squat on it and receive task definitions and script paths.

use std::ffi::OsStr;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use scriptrunner_core::SERVICE_NAME;
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO,
    SERVICE_QUERY_STATUS, SERVICE_STATUS_PROCESS,
};

/// `ERROR_SERVICE_DOES_NOT_EXIST`.
const SERVICE_DOES_NOT_EXIST: i32 = 1060;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceProcess {
    /// No such service: a development setup running `scriptrunner daemon` in a console.
    NotInstalled,
    /// Installed, but no process is running it.
    NotRunning,
    /// Installed and running as this process id.
    Running(u32),
}

pub fn query() -> io::Result<ServiceProcess> {
    let name: Vec<u16> = OsStr::new(SERVICE_NAME)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: plain SCM calls with valid, NUL-terminated arguments and valid out-pointers.
    // Every handle that was opened is closed on every path.
    unsafe {
        let manager = OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT);
        if manager.is_null() {
            return Err(io::Error::last_os_error());
        }
        let service = OpenServiceW(manager, name.as_ptr(), SERVICE_QUERY_STATUS);
        if service.is_null() {
            let err = io::Error::last_os_error();
            CloseServiceHandle(manager);
            return if err.raw_os_error() == Some(SERVICE_DOES_NOT_EXIST) {
                Ok(ServiceProcess::NotInstalled)
            } else {
                Err(err)
            };
        }

        let mut status: SERVICE_STATUS_PROCESS = zeroed();
        let mut needed = 0u32;
        let queried = QueryServiceStatusEx(
            service,
            SC_STATUS_PROCESS_INFO,
            ptr::from_mut(&mut status).cast(),
            size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut needed,
        );
        let outcome = if queried == 0 {
            Err(io::Error::last_os_error())
        } else if status.dwProcessId != 0 {
            // A process id is present from the moment the service is starting until it has stopped.
            Ok(ServiceProcess::Running(status.dwProcessId))
        } else {
            Ok(ServiceProcess::NotRunning)
        };
        CloseServiceHandle(service);
        CloseServiceHandle(manager);
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_manager_can_be_queried_without_elevation() {
        // Whether the service is installed depends on the machine; asking must not fail.
        query().expect("querying the service state should work for any user");
    }
}
