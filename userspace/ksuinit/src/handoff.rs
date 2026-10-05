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

/// Android init's documented first-stage entry vector.
///
/// The kernel's `rdinit=/espinit` vector does not carry the `first_stage`
/// selector. The proven qshim path explicitly invoked stock `/init` with this
/// argument; without it recovery init exits immediately.
fn real_init_argv() -> [*const u8; 3] {
    [
        cstr!("/init").as_ptr().cast(),
        cstr!("first_stage").as_ptr().cast(),
        std::ptr::null(),
    ]
}

/// Replace this process with Android's real first-stage init, preserving PID 1
/// and the kernel-provided environment.
///
/// # Safety
///
/// `envp` must be the pointer received by the process entry point.
pub unsafe fn exec_real_init(envp: *const *const u8) -> Result<(), Failure> {
    let arguments = real_init_argv();
    let error = unsafe { execve(cstr!("/init"), arguments.as_ptr(), envp) };

    Err(Failure::new(
        Stage::Handoff,
        "HandoffExecFailed",
        format!("cannot exec {REAL_INIT}: {error}"),
    ))
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;

    use super::*;

    #[test]
    fn handoff_uses_androids_proven_first_stage_vector() {
        let arguments = real_init_argv();
        let program = unsafe { CStr::from_ptr(arguments[0].cast()) };
        let stage = unsafe { CStr::from_ptr(arguments[1].cast()) };
        assert_eq!(program, c"/init");
        assert_eq!(stage, c"first_stage");
        assert!(arguments[2].is_null());
    }
}
