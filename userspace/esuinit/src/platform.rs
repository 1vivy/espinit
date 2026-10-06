//! Stage the exact daemon for every boot; managed boots require verified projection first.
use crate::config::{Manifest, RomConfig};
use crate::receipt::{Failure, Stage};
use anyhow::{Context, Result, ensure};
use std::fs;
use std::path::Path;

/// Publish the PID1-classified mode after the core passes its generation check.
/// The core accepts this only from global PID1 and cannot switch modes later.
pub(crate) fn select_core_boot_mode() -> Result<(), Failure> {
    let mode = if crate::scripts::is_recovery() { 2 } else { 1 };
    let result = (|| -> Result<()> {
        crate::set_core_boot_mode(mode)?;
        ensure!(
            crate::query_core_info()?.boot_mode == mode,
            "core boot mode readback mismatch"
        );
        Ok(())
    })();
    result.map_err(|error| {
        Failure::new(
            Stage::ModuleCheck,
            "PlatformBootModeFailed",
            format!("{error:#}"),
        )
    })
}

pub fn stage(payload: &Path, manifest: &Manifest, rom: &RomConfig) -> Result<(), Failure> {
    stage_payload(payload, manifest, rom).map_err(|error| {
        Failure::new(
            Stage::Storage,
            "PlatformStagingFailed",
            format!("{error:#}"),
        )
    })
}

fn stage_payload(payload: &Path, manifest: &Manifest, rom: &RomConfig) -> Result<()> {
    let platform = manifest
        .platform
        .as_ref()
        .context("every esu payload requires manifest.platform")?;
    let recovery = crate::scripts::is_recovery();
    let mode = if recovery {
        esu_platform::BootMode::Recovery
    } else if rom.managed {
        esu_platform::BootMode::Managed
    } else {
        esu_platform::BootMode::Unmanaged
    };
    let rom_path =
        crate::config::rom_path(manifest, &rom.id).map_err(|error| anyhow::anyhow!("{error}"))?;
    let plan = esu_platform::plan(payload, platform, &manifest.generation, &rom_path, mode)?;
    let device = if rom.managed {
        let required: &[&str] = if recovery {
            &["metadata"]
        } else {
            &["metadata", "bdsvars", "misc"]
        };
        for name in required {
            ensure!(
                rom.partitions
                    .iter()
                    .any(|entry| entry.name == *name && !entry.read_only),
                "platform requires writable projected {name}"
            );
        }
        projected_metadata()?
    } else {
        // Unmanaged boot preserves the native view, but the unconditional init
        // exec still needs its daemon. Resolve exactly metadata, never fall back
        // to another partition or to an uninstalled Android runtime.
        crate::init::resolve_native_metadata()
            .context("native metadata unavailable")?
            .rdev
    };
    esu_platform::staging::mount_and_publish(device, &platform.metadata_filesystem, plan)
}

fn projected_metadata() -> Result<u64> {
    let disk = fs::canonicalize("/sys/class/block/esu-gpt").context("projected disk missing")?;
    let mut found = None;
    for entry in fs::read_dir(&disk)? {
        let path = entry?.path();
        if !path.join("partition").is_file() {
            continue;
        }
        let uevent = fs::read_to_string(path.join("uevent"))?;
        if !uevent.lines().any(|line| line == "PARTNAME=metadata") {
            continue;
        }
        ensure!(found.is_none(), "ambiguous projected metadata");
        let dev = fs::read_to_string(path.join("dev"))?;
        let (major, minor) = dev.trim().split_once(':').context("bad metadata dev_t")?;
        found = Some(rustix::fs::makedev(major.parse()?, minor.parse()?));
    }
    found.context("metadata is absent from the projected disk")
}
