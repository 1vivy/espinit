// SPDX-License-Identifier: GPL-3.0-only
//! Per-ROM firmware views from the shared thin pool.
//!
//! Runs as the `fw-views` module script before the `gpt` entry, after
//! `thin-activate` has published the pool layer. For every `firmware_views`
//! entry of the selected ROM it creates one thin device whose origin is the
//! physical firmware partition, so the projection that follows sees
//! `/dev/mapper/rom<N>-fw-<name>` serving the physical bytes until this ROM's
//! OTA writes to it. Every failure exits nonzero, and PID 1 treats a managed
//! module script failure as fatal: a ROM never boots with a missing view or a
//! shared, unwritten-through guess.
//!
//! The origin partitions are resolved before the projection runs, so the views
//! read the physical `PARTNAME` sysfs name rather than a hidden or projected
//! one, and the kernel opens them read-only. No argument is accepted; the
//! selection is the same `androidboot.esu.rom` PID 1 used.

use dm::{DeviceMapper, DeviceNumber, Mapper, MessageError};
use esu_platform::generation;
use esuinit::config;
use esuinit::esp::{ESP_MOUNT_POINT, payload_root};
use fw_views::plan;
use std::fs;
use std::path::Path;

/// sysfs per-device directory, addressed by device number so the resolution
/// works without any `/dev` node.
const SYS_DEV_BLOCK: &str = "/sys/dev/block";

/// Read one small text file, reporting its path on failure.
fn read_text(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| format!("cannot read {}: {error}", path.display()))
}

/// The payload generation this binary was built for. The note is the shared
/// Apache-2.0 source of `esu-platform`, which is the same one `esud`
/// carries, so this binary must not link a second copy of it.
fn check_generation() -> Result<(), String> {
    let expected = std::env::var("ESU_GENERATION")
        .map_err(|_| "ESU_GENERATION is missing or non-UTF-8".to_owned())?;

    if expected != generation::generation() {
        return Err("compiled generation does not match ESU_GENERATION".to_owned());
    }

    Ok(())
}

/// The selected ROM's validated config, read exactly like PID 1 reads it: the
/// bootconfig/cmdline key selects `<id>.toml`, and the manifest supplies the
/// generation every file must match.
fn rom_config() -> Result<config::RomConfig, String> {
    let root = payload_root(ESP_MOUNT_POINT);
    let manifest = config::parse_manifest(&read_text(&root.join("manifest.toml"))?)
        .map_err(|error| error.to_string())?;
    let bootconfig = read_text(Path::new("/proc/bootconfig")).unwrap_or_default();
    let cmdline = read_text(Path::new("/proc/cmdline")).unwrap_or_default();
    let id = config::selected_rom_id(&bootconfig, &cmdline).map_err(|error| error.to_string())?;
    let rom_path = config::rom_path(&manifest, id).map_err(|error| error.to_string())?;
    let rom =
        config::parse_selected_rom(&read_text(&root.join(&rom_path))?, &manifest.generation, id)
            .map_err(|error| error.to_string())?;

    Ok(rom)
}

/// One physical partition's size in 512-byte sectors.
fn device_sectors(device: DeviceNumber) -> Result<u64, String> {
    let path = Path::new(SYS_DEV_BLOCK).join(format!("{}:{}", device.major, device.minor));
    let size = read_text(&path.join("size"))?;

    size.trim()
        .parse()
        .map_err(|_| format!("{} is not a sector count", path.display()))
}

/// The sysfs device number of a resolved partition.
fn number(rdev: libc::dev_t) -> Result<DeviceNumber, String> {
    if rdev == 0 {
        return Err("physical partition has no device number".to_owned());
    }

    // `libc::major`/`libc::minor` return `c_uint` on glibc and `i32` on bionic,
    // so exactly one of the two hosts needs the cast.
    #[allow(clippy::unnecessary_cast)]
    let (major, minor) = (libc::major(rdev) as u32, libc::minor(rdev) as u32);

    Ok(DeviceNumber { major, minor })
}

fn run() -> Result<(), String> {
    if std::env::args_os().len() != 1 {
        return Err("fw-views accepts no arguments".to_owned());
    }

    check_generation()?;

    let rom = rom_config()?;
    let views = plan::views(&rom);

    if views.is_empty() {
        println!("fw-views: ROM {} has no firmware views", rom.id);
        return Ok(());
    }

    // The pool layer is published by the earlier `thin` entry, so an absent pool
    // is a broken payload and never a reason to guess another device.
    let pool = DeviceMapper::device_number(plan::POOL)?;
    let mut mapper = DeviceMapper::open()?;

    for view in &views {
        let origin = esu_platform::block::partition_by_name(&view.origin)
            .map_err(|error| format!("cannot resolve physical {}: {error}", view.origin))?;
        let origin = number(origin)?;
        let sectors = device_sectors(origin)?;

        match mapper.message(plan::POOL, 0, &plan::create_thin(view.thin_id)) {
            // An id created on an earlier boot is the expected outcome; the
            // device, not the message, is the state this ROM owns.
            Ok(()) | Err(MessageError::AlreadyExists) => {}
            Err(MessageError::Failed(error)) => {
                return Err(format!(
                    "{} failed on {}: {error}",
                    plan::create_thin(view.thin_id),
                    plan::POOL
                ));
            }
        }

        mapper.activate(
            &view.device,
            &[plan::table(origin, sectors, view.thin_id, pool)],
        )?;
    }

    mapper.commit();

    println!(
        "fw-views: rom={} views={} pool={}:{}",
        rom.id,
        views.len(),
        pool.major,
        pool.minor
    );

    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("fw-views: {error}");
        std::process::exit(1);
    }
}
