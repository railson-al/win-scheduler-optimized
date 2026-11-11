//! Access control for the daemon's named pipe (Windows only).
//!
//! The service runs as LocalSystem, so anyone who can open the pipe can create
//! a task and run code as SYSTEM. The pipe therefore admits SYSTEM and the
//! Administrators group, and nobody else.

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::ptr;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

/// `D:P` is a protected DACL (no inherited entries). `GA` is generic-all, for
/// `SY` (LocalSystem) and `BA` (BUILTIN\Administrators). Everyone else has no
/// entry, which means no access.
pub const ADMINS_ONLY_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)";

/// A security descriptor built from SDDL, with the `SECURITY_ATTRIBUTES` that
/// points at it. Owns the descriptor and frees it on drop.
pub struct PipeSecurity {
    descriptor: *mut c_void,
    attributes: SECURITY_ATTRIBUTES,
}

// SAFETY: the descriptor is a private, immutable heap block owned by this value,
// and nothing hands out a way to mutate it. Moving it between threads is fine.
unsafe impl Send for PipeSecurity {}

impl PipeSecurity {
    pub fn admins_only() -> io::Result<Self> {
        Self::from_sddl(ADMINS_ONLY_SDDL)
    }

    pub fn from_sddl(sddl: &str) -> io::Result<Self> {
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut descriptor: *mut c_void = ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated and outlives the call, `descriptor` is a valid
        // out-pointer, and the size out-parameter is documented as optional (null).
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
        })
    }

    /// The pointer `ServerOptions::create_with_security_attributes_raw` expects.
    /// It stays valid while `self` is alive and unmoved; callers pass it straight
    /// into the create call, which copies what it needs.
    pub fn as_attributes_ptr(&mut self) -> *mut c_void {
        ptr::addr_of_mut!(self.attributes).cast()
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            // SAFETY: allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`,
            // which documents `LocalFree` as the way to release it, and freed only here.
            unsafe { LocalFree(self.descriptor) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_admins_only_descriptor_is_accepted_by_windows() {
        let mut security = PipeSecurity::admins_only().expect("the SDDL must parse");
        assert!(!security.as_attributes_ptr().is_null());
    }

    #[test]
    fn a_malformed_descriptor_is_an_error_not_a_panic() {
        assert!(PipeSecurity::from_sddl("this is not sddl").is_err());
    }
}
