#![deny(clippy::all, clippy::pedantic)]
#![warn(clippy::nursery)]
#![allow(
    clippy::module_name_repetitions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::redundant_field_names
)]

#[cfg(target_os = "linux")]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod boot_patch;
#[cfg(target_os = "android")]
mod cli;
#[cfg(target_os = "android")]
mod debug;
mod defs;
mod esp_lifecycle;
#[cfg(target_os = "linux")]
mod host;
#[cfg(target_os = "android")]
mod init_event;
#[cfg(target_os = "android")]
mod ksucalls;
mod module;
mod overlay;
#[cfg(target_os = "android")]
mod resetprop;
mod rom_isolation;
mod sepolicy;
#[cfg(target_os = "android")]
mod utils;

#[allow(nonstandard_style, unused, unsafe_op_in_unsafe_fn)]
mod ksu_uapi;

#[cfg(target_os = "android")]
fn report_fatal(error: &anyhow::Error) {
    use std::io::Write;

    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = writeln!(kmsg, "<3>esud fatal: {error:#}");
    }
}

fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "android")]
    {
        let result = cli::run();
        if let Err(error) = &result {
            if (module::is_critical_failure(error) || init_event::is_required_boot_failure(error))
                && ksucalls::get_info().boot_mode == 1
            {
                esuinit::init::fatal_boot(|| report_fatal(error));
            }
            report_fatal(error);
        }
        result
    }
    #[cfg(target_os = "linux")]
    {
        host::run()
    }
    #[cfg(not(any(target_os = "android", target_os = "linux")))]
    {
        anyhow::bail!("esud requires Android or a Linux artifact-building host")
    }
}
