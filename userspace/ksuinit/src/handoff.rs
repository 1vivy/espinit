//! Handoff to the init saved by the host boot-image patcher.
//!
//! The target is a collision-checked fixed path: ESP configuration can never
//! choose an executable, and there is no fallback.  The KernelSU-style overlay
//! installs espinit as `/init` and preserves the effective prior `/init` at
//! `/init.espinit`.

use rustix::cstr;
use rustix::runtime::execve;

use crate::receipt::{Failure, Stage};

/// Fixed path holding the init that the takeover archive replaced.
pub const REAL_INIT: &str = "/init.espinit";

/// Replace this process with the real init, preserving the original `argv`,
/// `envp`, and PID. This returns only on failure, which is a fatal handoff
/// error: the caller must not continue normal boot.
///
/// # Safety
///
/// `argv` and `envp` must be the pointers received by the process entry point.
pub unsafe fn exec_real_init(
    argv: *const *const u8,
    envp: *const *const u8,
) -> Result<(), Failure> {
    let error = unsafe { execve(cstr!("/init.espinit"), argv, envp) };

    Err(Failure::new(
        Stage::Handoff,
        "HandoffExecFailed",
        format!("cannot exec {REAL_INIT}: {error}"),
    ))
}
