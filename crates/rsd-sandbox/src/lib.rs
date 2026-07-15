//! rsd-sandbox: process self-sealing via `sandbox_init` (DESIGN.md P3).
//!
//! The worker seals itself AFTER startup — dyld, libSystem, and its socket are
//! already in hand — so the profile can be the tightest one that exists:
//! `(deny default)`. After `seal()`, the process cannot open files, connect
//! sockets, or exec; it can only compute and use file descriptors it already
//! holds. That is precisely the capability model: the host passes a read-only
//! fd per request, and nothing else in the universe is reachable.

use std::ffi::{c_char, c_int, CString};
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;

extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// Irreversibly drop all ambient authority: deny-default Seatbelt profile.
/// Everything the process needs from now on must already be an open fd.
pub fn seal() -> Result<(), String> {
    let profile = CString::new("(version 1)\n(deny default)").expect("static profile");
    let mut err: *mut c_char = std::ptr::null_mut();
    let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &mut err) };
    if rc != 0 {
        let msg = if err.is_null() {
            "sandbox_init failed".to_string()
        } else {
            let m = unsafe { std::ffi::CStr::from_ptr(err) }
                .to_string_lossy()
                .into_owned();
            unsafe { sandbox_free_error(err) };
            m
        };
        return Err(msg);
    }
    Ok(())
}

/// Claim fd 0 (stdin) as the worker's UnixStream — the host wires the
/// socketpair end there at spawn. Call exactly once, before any stdin use.
pub fn stdin_unix_stream() -> UnixStream {
    unsafe { UnixStream::from_raw_fd(0) }
}
