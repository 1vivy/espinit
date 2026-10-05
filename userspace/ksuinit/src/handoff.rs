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

/// Fixed argument bound for the kernel's init invocation.
const MAX_INIT_ARGS: usize = 64;

/// Replace the rdinit process name while retaining the remaining arguments.
unsafe fn real_init_argv(
    argc: i32,
    argv: *const *const u8,
) -> Result<[*const u8; MAX_INIT_ARGS], Failure> {
    let count = usize::try_from(argc).map_err(|_| {
        Failure::new(
            Stage::Handoff,
            "HandoffArgvInvalid",
            "negative init argument count",
        )
    })?;
    if count == 0 || count >= MAX_INIT_ARGS || argv.is_null() {
        return Err(Failure::new(
            Stage::Handoff,
            "HandoffArgvInvalid",
            "init argument vector is empty, null, or exceeds the fixed bound",
        ));
    }

    let mut arguments = [std::ptr::null(); MAX_INIT_ARGS];
    arguments[0] = cstr!("/init").as_ptr().cast();
    for (index, destination) in arguments.iter_mut().enumerate().take(count).skip(1) {
        let argument = unsafe { *argv.add(index) };
        if argument.is_null() {
            return Err(Failure::new(
                Stage::Handoff,
                "HandoffArgvInvalid",
                "init argument vector ends before argc",
            ));
        }
        *destination = argument;
    }
    Ok(arguments)
}

/// Replace this process with the real init, preserving PID, `envp`, and every
/// argument after `argv[0]`. The kernel names the rdinit executable in
/// `argv[0]`, so handoff replaces `/espinit` with `/init`.
///
/// # Safety
///
/// `argv` and `envp` must be the pointers received by the process entry point,
/// and `argc` must describe `argv`.
pub unsafe fn exec_real_init(
    argc: i32,
    argv: *const *const u8,
    envp: *const *const u8,
) -> Result<(), Failure> {
    let arguments = unsafe { real_init_argv(argc, argv)? };
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
    fn handoff_replaces_only_the_rdinit_process_name() {
        let rdinit = c"/espinit";
        let option = c"--second-stage";
        let original = [
            rdinit.as_ptr().cast(),
            option.as_ptr().cast(),
            std::ptr::null(),
        ];
        let rewritten = unsafe { real_init_argv(2, original.as_ptr()) }.unwrap();

        let name = unsafe { CStr::from_ptr(rewritten[0].cast()) };
        assert_eq!(name, c"/init");
        assert_eq!(rewritten[1], original[1]);
        assert!(rewritten[2].is_null());
    }

    #[test]
    fn malformed_init_argument_vectors_fail_closed() {
        assert!(unsafe { real_init_argv(0, std::ptr::null()) }.is_err());
        let truncated = [c"/espinit".as_ptr().cast(), std::ptr::null()];
        assert!(unsafe { real_init_argv(2, truncated.as_ptr()) }.is_err());
        assert!(
            unsafe { real_init_argv(MAX_INIT_ARGS.try_into().unwrap(), truncated.as_ptr()) }
                .is_err()
        );
    }
}
