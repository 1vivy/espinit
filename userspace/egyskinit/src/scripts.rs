//! Vendor-module list selection uses the same userspace boot context as runtime.
pub(crate) use egysk_runtime::context::BootMode;
pub(crate) const RECOVERY_EXECUTABLE: &str = "/system/bin/recovery";
pub(crate) fn classify_boot_mode(
    bootconfig: &str,
    cmdline: &str,
    recovery_present: bool,
) -> BootMode {
    egysk_runtime::boot_mode(bootconfig, cmdline, recovery_present)
}
