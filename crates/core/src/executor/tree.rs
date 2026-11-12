//! Process-tree control: lets us stop a script *and* everything it spawned.
//!
//! * Unix (development/CI): the child leads its own process group, and the
//!   whole group receives SIGKILL.
//! * Windows (production): the child is assigned to a Job Object, and the job
//!   is terminated. The job deliberately does not use KILL_ON_JOB_CLOSE, so a
//!   script that intentionally leaves a detached program running (e.g. `start
//!   app.exe`) keeps behaving the way it would under any other launcher.
//!
//! Known gap on Windows: a grandchild spawned in the few microseconds between
//! process creation and job assignment escapes the job.

use tokio::process::{Child, Command};

#[cfg(unix)]
mod imp {
    use super::*;

    pub fn configure(cmd: &mut Command) {
        cmd.process_group(0);
    }

    pub struct ProcessTree {
        pgid: Option<i32>,
    }

    impl ProcessTree {
        pub fn attach(child: &Child) -> Self {
            Self {
                pgid: child.id().and_then(|pid| i32::try_from(pid).ok()),
            }
        }

        pub fn kill(&self) {
            if let Some(pgid) = self.pgid {
                // SAFETY: plain syscall; a negative pid addresses the group
                // we created at spawn time.
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject};

    use super::*;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub fn configure(cmd: &mut Command) {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    pub struct ProcessTree {
        job: HANDLE,
    }

    // A job handle is just a kernel object reference; it may be used from any thread.
    unsafe impl Send for ProcessTree {}
    unsafe impl Sync for ProcessTree {}

    impl ProcessTree {
        pub fn attach(child: &Child) -> Self {
            match child.raw_handle() {
                Some(process) => Self::attach_raw(process as HANDLE),
                None => Self {
                    job: std::ptr::null_mut(),
                },
            }
        }

        /// Tracks a process that did not come from `tokio::process`: the one started in the logged-on user's
        /// session. `process` must stay open until this call returns; the job does not keep it open.
        pub fn attach_raw(process: HANDLE) -> Self {
            // SAFETY: FFI calls with valid (or null, where allowed) arguments;
            // the handle we create is owned by `Self` and closed in `Drop`.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if !job.is_null() {
                    // Failure is tolerated: the executor still kills the
                    // main process directly as a fallback.
                    AssignProcessToJobObject(job, process);
                }
                Self { job }
            }
        }

        pub fn kill(&self) {
            if !self.job.is_null() {
                // SAFETY: `job` is a live handle owned by `self`.
                unsafe {
                    TerminateJobObject(self.job, 1);
                }
            }
        }
    }

    impl Drop for ProcessTree {
        fn drop(&mut self) {
            if !self.job.is_null() {
                // SAFETY: closing the handle we created, exactly once.
                unsafe {
                    CloseHandle(self.job);
                }
            }
        }
    }
}

pub use imp::{ProcessTree, configure};
