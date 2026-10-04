#![cfg_attr(not(test), no_main)]

#[cfg(not(test))]
use std::io::Write;

#[cfg(not(test))]
use espinit::{handoff, init, receipt};

/// PID-1 entry point.
///
/// The early managed boot runs first, and only as process 1: any other caller
/// gets a nonzero exit status immediately, without touching the platform. As
/// PID 1, any failure stops the handoff, persists a bounded receipt, and enters
/// the fatal-boot stop path; the real init is never executed after an init
/// error. On success the fixed real `/init` is executed with the original
/// `argv`/`envp`, preserving PID 1.
///
/// # Safety
/// Called by the kernel as the process entry point.
#[cfg(not(test))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn main(_argc: i32, argv: *const *const u8, envp: *const *const u8) -> i32 {
    if !rustix::process::getpid().is_init() {
        // Not the boot init: report the usage error and return instead of
        // rebooting the machine or parking the caller. Kernel logging is not
        // set up yet, so this goes to stderr, best-effort.
        let _ = writeln!(
            std::io::stderr().lock(),
            "espinit: must run as process 1; refusing to continue"
        );
        return 1;
    }

    let mut state = receipt::ReceiptState::default();

    if let Err(failure) = init::run(&mut state) {
        receipt::record(&mut state, &failure);
        init::stop_boot();
    }

    if let Err(failure) = unsafe { handoff::exec_real_init(argv, envp) } {
        receipt::record(&mut state, &failure);
        init::stop_boot();
    }

    init::stop_boot()
}
