use crate::{defs, ksucalls, module, overlay, rom_isolation};
use anyhow::{Result, ensure};
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
}
fn boot_mode() -> Result<u32> {
    ksucalls::ensure_uapi_version_matched()?;
    let mode = ksucalls::get_info().boot_mode;
    ensure!(mode == 1 || mode == 2, "core boot mode is not selected");
    Ok(mode)
}

pub fn reload() -> Result<()> {
    boot_mode()?;
    let manifest = module::manifest()?;
    overlay::apply(Path::new(defs::MODULE_DIR), &manifest.modules_order)
}

pub fn on_stage(stage: Stage) -> Result<()> {
    let mode = boot_mode()?;
    ensure!(
        mode == if matches!(stage, Stage::Recovery) {
            2
        } else {
            1
        },
        "stage does not match core boot mode"
    );
    let manifest = module::manifest()?;
    let rom = rom_isolation::runtime_rom()?;
    if matches!(stage, Stage::Early) {
        crate::boot_watchdog::arm();
        overlay::apply(Path::new(defs::MODULE_DIR), &manifest.modules_order)?;
    }
    if matches!(stage, Stage::PostFsData) {
        std::fs::create_dir_all(defs::LOG_DIR)?;
        ksucalls::report_post_fs_data();
        if let Some(rom) = &rom {
            rom_isolation::post_fs_data(&rom.config)?;
        }
        catch_bootlog("logcat", &["logcat", "-b", "all"])?;
        catch_bootlog("dmesg", &["dmesg", "-w", "-r"])?;
    }
    if matches!(stage, Stage::Service) {
        let _ = ksucalls::report_services()?;
    }
    if matches!(stage, Stage::BootCompleted) {
        ksucalls::report_boot_complete();
    }
    module::scripts(&manifest.modules_order, stage.name(), rom.as_ref())?;
    if matches!(stage, Stage::Early)
        && let Some(rom) = &rom
    {
        rom_isolation::early(&rom.config, rom.number)?;
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
    std::process::Command::new(defs::BUSYBOX)
        .args(["timeout", "-s", "9", "30s"])
        .args(command)
        .process_group(0)
        .stdout(log)
        .spawn()?;
    Ok(())
}
