use crate::{defs, ksucalls, module, overlay, rom_isolation};
use anyhow::{Result, ensure};
use std::fmt;
use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub enum Stage {
    Early,
    PostFs,
    PostFsData,
    Service,
    BootCompleted,
    Recovery,
}
impl Stage {
    const fn name(self) -> &'static str {
        match self {
            Self::Early => "early",
            Self::PostFs => "post-fs",
            Self::PostFsData => "post-fs-data",
            Self::Service => "service",
            Self::BootCompleted => "boot-completed",
            Self::Recovery => "recovery",
        }
    }

    const fn accepts_mode(self, mode: u32) -> bool {
        // The shared core RC emits these lifecycle stages in both boot modes.
        // Only the explicit recovery callback is recovery-only.
        matches!(mode, 1 | 2) && (!matches!(self, Self::Recovery) || mode == 2)
    }
}

/// A required ROM identity, credential-store or projection fault. Normal
/// Android must not continue without it: there is no partial-isolation boot.
/// Recovery records the gap and keeps the rescue path usable instead.
#[derive(Debug)]
pub struct RequiredBootError(anyhow::Error);

impl fmt::Display for RequiredBootError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "required boot isolation failed: {:#}", self.0)
    }
}

impl std::error::Error for RequiredBootError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

/// True when the error is a required ROM identity/isolation failure, anywhere
/// in its context chain. Optional module work, log capture and safe-mode skips
/// never classify here.
pub fn is_required_boot_failure(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(<dyn std::error::Error>::is::<RequiredBootError>)
}

fn boot_mode() -> Result<u32> {
    ksucalls::ensure_uapi_version_matched()?;
    let mode = ksucalls::get_info().boot_mode;
    ensure!(mode == 1 || mode == 2, "core boot mode is not selected");
    Ok(mode)
}

/// Safe mode boots without any ESP module work, so a broken module cannot take
/// the device down. It is requested by the standard Android safe-mode
/// properties or by the core module's retained `CHECK_SAFEMODE` signal, which
/// observes the boot-time volume key the daemon cannot see.
pub fn safe_mode() -> bool {
    for name in ["persist.sys.safemode", "ro.sys.safemode"] {
        if crate::utils::getprop(name).as_deref() == Some("1") {
            log::info!("safe mode requested by {name}");
            return true;
        }
    }
    if ksucalls::check_safemode() {
        log::info!("safe mode reported by the core module");
        return true;
    }
    false
}

/// Normal Android must not continue past a required isolation fault; recovery
/// records it and stays running.
fn required(error: anyhow::Error, recovery: bool) -> Result<()> {
    let error = error.context("required ROM identity/isolation");
    if recovery {
        rom_isolation::report(&format!("{error:#}"));
        return Ok(());
    }
    Err(RequiredBootError(error).into())
}

pub fn reload() -> Result<()> {
    let mode = boot_mode()?;
    crate::esp_lifecycle::prepare()?;
    if safe_mode() {
        log::info!("safe mode: skipping ESP module overlays");
        return Ok(());
    }
    let manifest = module::manifest()?;
    overlay::apply(
        Path::new(defs::MODULE_DIR),
        &manifest.modules_order,
        mode == 2,
    )
}

pub fn on_stage(stage: Stage) -> Result<()> {
    let mode = boot_mode()?;
    ensure!(
        stage.accepts_mode(mode),
        "stage does not match core boot mode"
    );
    let recovery = mode == 2;
    if let Err(error) = crate::esp_lifecycle::prepare() {
        return required(error, recovery);
    }
    let rom = match rom_isolation::runtime_rom() {
        Ok(rom) => rom,
        Err(error) => return required(error, recovery),
    };
    if matches!(stage, Stage::PostFsData) {
        if let Err(error) = std::fs::create_dir_all(defs::LOG_DIR) {
            log::warn!("cannot create lifecycle log directory: {error:#}");
        }
        ksucalls::report_post_fs_data();
        if let Some(rom) = &rom
            && let Err(error) = rom_isolation::post_fs_data(&rom.config)
        {
            // The core module records non-fatal isolation gaps itself, and a
            // missing first-boot marker must not suppress the module scripts.
            rom_isolation::report(&format!("post-fs-data isolation gap: {error:#}"));
        }
        // Capture is a diagnostic: failing to start it never stops a stage.
        for (name, command) in [
            ("logcat", &["logcat", "-b", "all"][..]),
            ("dmesg", &["dmesg", "-w", "-r"][..]),
        ] {
            if let Err(error) = catch_bootlog(name, command) {
                log::warn!("cannot capture {name}: {error:#}");
            }
        }
    }
    if matches!(stage, Stage::Service) {
        let _ = ksucalls::report_services()?;
    }
    if matches!(stage, Stage::BootCompleted) {
        ksucalls::report_boot_complete();
    }
    // Safe mode boots without ESP module work: no policy, no overlays, no
    // scripts and no module metadata validation, so a malformed module cannot
    // stop second-stage init before the skip.
    if !safe_mode() {
        let manifest = module::manifest()?;
        if matches!(stage, Stage::Early) {
            overlay::apply(
                Path::new(defs::MODULE_DIR),
                &manifest.modules_order,
                recovery,
            )?;
        }
        module::scripts(
            &manifest.modules_order,
            stage.name(),
            rom.as_ref(),
            recovery,
        )?;
    }
    if matches!(stage, Stage::Early)
        && let Some(rom) = &rom
        && let Err(error) = rom_isolation::early(&rom.config, rom.number)
    {
        return required(error, recovery);
    }
    Ok(())
}

fn catch_bootlog(name: &str, command: &[&str]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let path = Path::new(defs::BOOTLOG_DIR).join(format!("{name}.log"));
    if path.exists() {
        std::fs::rename(&path, path.with_extension("old.log"))?;
    }
    let log = std::fs::File::create(path)?;
    let mut capture = std::process::Command::new(defs::BUSYBOX);
    capture
        .args(["timeout", "-s", "9", "30s"])
        .args(command)
        .process_group(0)
        .stdout(log);
    // SAFETY: use the existing KernelSU child cgroup escape before exec;
    // otherwise init reaps these captures as soon as post-fs-data returns.
    unsafe {
        capture.pre_exec(|| {
            crate::utils::switch_cgroups();
            Ok(())
        });
    }
    capture.spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Stage, is_required_boot_failure, required};

    #[test]
    fn shared_stages_accept_both_selected_boot_modes() {
        for stage in [
            Stage::Early,
            Stage::PostFs,
            Stage::PostFsData,
            Stage::Service,
            Stage::BootCompleted,
        ] {
            for mode in [1, 2] {
                assert!(stage.accepts_mode(mode), "{stage:?} rejected mode {mode}");
            }
        }
    }

    #[test]
    fn recovery_callback_rejects_android_and_all_stages_reject_unselected_modes() {
        assert!(Stage::Recovery.accepts_mode(2));
        assert!(!Stage::Recovery.accepts_mode(1));
        for stage in [
            Stage::Early,
            Stage::PostFs,
            Stage::PostFsData,
            Stage::Service,
            Stage::BootCompleted,
            Stage::Recovery,
        ] {
            for mode in [0, 3, u32::MAX] {
                assert!(!stage.accepts_mode(mode), "{stage:?} accepted mode {mode}");
            }
        }
    }

    #[test]
    fn only_required_isolation_faults_stop_normal_android() {
        let error = required(anyhow::anyhow!("credential store unavailable"), false).unwrap_err();
        assert!(is_required_boot_failure(&error));
        // A required fault is classified through its whole context chain.
        let chained = error.context("post-fs-data");
        assert!(is_required_boot_failure(&chained));
        // Optional module and log-capture faults never classify here.
        for error in [
            anyhow::anyhow!("module first early: script failed"),
            anyhow::anyhow!("cannot capture dmesg"),
        ] {
            assert!(!is_required_boot_failure(&error));
            assert!(!crate::module::is_critical_failure(&error));
        }
    }

    #[test]
    fn recovery_records_required_isolation_faults_and_stays_running() {
        assert!(required(anyhow::anyhow!("credential store unavailable"), true).is_ok());
    }
}
