//! Handoff to the stock init.
//!
//! The takeover archive is entered through `rdinit=/esuinit` and carries no
//! init at all: the stock `/init` is never renamed, copied or inspected, so the
//! target is always the fixed, kernel-defined init path and there is no
//! fallback. `argv[0]` is set to that path with every later argument and the
//! environment preserved, so Android's init sees exactly the argv it would have
//! seen from the kernel.

use rustix::cstr;
use rustix::runtime::execve;

use crate::receipt::{Failure, Stage};

/// Fixed path of the stock init the kernel would have executed.
pub const REAL_INIT: &str = "/init";

/// Replace this process with the stock init, preserving the original
/// `argv[1..]`, `envp` and PID. This returns only on failure, which is a fatal
/// handoff error: the caller must not continue normal boot.
///
/// # Safety
///
/// `argc` and `argv` must be the count and pointers received by the process
/// entry point.
pub unsafe fn exec_real_init(
    argc: i32,
    argv: *const *const u8,
    envp: *const *const u8,
) -> Result<(), Failure> {
    if argc < 1 {
        return Err(Failure::new(
            Stage::Handoff,
            "HandoffExecFailed",
            format!("cannot exec {REAL_INIT}: the kernel passed no argv[0]"),
        ));
    }

    let mut arguments: Vec<*const u8> = Vec::with_capacity(argc as usize + 1);
    arguments.push(cstr!("/init").as_ptr().cast());
    for index in 1..argc as usize {
        // SAFETY: the kernel passed a terminated `argc`-entry vector.
        arguments.push(unsafe { *argv.add(index) });
    }
    arguments.push(std::ptr::null());

    let error = unsafe { execve(cstr!("/init"), arguments.as_ptr(), envp) };

    Err(Failure::new(
        Stage::Handoff,
        "HandoffExecFailed",
        format!("cannot exec {REAL_INIT}: {error}"),
    ))
}
