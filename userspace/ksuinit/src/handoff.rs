//! Handoff to Android's real init.
//!
//! The target is a single fixed path: ESP configuration can never choose an
//! executable, and there is no `/init.real` or `/system/bin/init` fallback. The
//! boot-image integration installs espinit as early PID 1 without taking over
//! the raw ramdisk `/init`, so the real init is always `/init`.

use rustix::cstr;
use rustix::runtime::execve;

use crate::receipt::{Failure, Stage};

/// The fixed real-init target.
pub const REAL_INIT: &str = "/init";

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
    let error = unsafe { execve(cstr!("/init"), argv, envp) };

    Err(Failure::new(
        Stage::Handoff,
        "HandoffExecFailed",
        format!("cannot exec {REAL_INIT}: {error}"),
    ))
}
