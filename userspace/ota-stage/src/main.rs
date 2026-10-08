// SPDX-License-Identifier: GPL-3.0-only
//! Publish the per-base switch devices of a managed staging ROM at PID 1.
//!
//! Runs as the `ota` module's `pid1.sh`, after `thin-activate` has activated
//! every staging LV and before the `gpt` entry resolves the projection. For every
//! declared base image of the selected ROM it creates one device-mapper device,
//! `rom<N>-ota-<base>`, whose table covers exactly the base image's geometry:
//!
//! * while `ESU_STAGE` is empty (no transaction, or ROM 1) the device is an
//!   error target of the image's size, so the ROM config's generated
//!   `SOURCE_COPY` size checks pass and a read of the idle letter fails loudly
//!   instead of serving stale bytes;
//! * while a staged letter runs, the device is linear over that base's active
//!   staging LV, read-only exactly when the staged letter is the booted one, so
//!   an updater's writes reach whatever the boot HAL routes to it.
//!
//! The boot HAL reloads the same device as the transaction moves; this helper
//! only creates it. An identical table is accepted, so a re-run is idempotent.
//! A ROM 1 does nothing and exits 0. Every other failure exits 1 with a
//! one-line reason on stderr; PID 1 treats this critical module's failure as
//! fatal because the projection that follows would name a missing device.

use dm::DeviceMapper;
use esu_config::{Backend, IMAGE_BASES, base_image_path};
use esuinit::config;
use esuinit::esp::{ESP_MOUNT_POINT, payload_root};
use ota_stage::plan::{self, Base};
use std::fs;
use std::path::Path;

/// Read one small text file, reporting its path on failure.
fn read_text(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| format!("cannot read {}: {error}", path.display()))
}

/// The selected ROM's configuration and authoritative number, from the identity
/// PID 1 exports and the payload the loader mounted read-only.
fn rom_config() -> Result<(config::RomConfig, u32), String> {
    let root = payload_root(ESP_MOUNT_POINT);
    let manifest = config::parse_manifest(&read_text(&root.join("manifest.toml"))?)
        .map_err(|error| error.to_string())?;
    let id = std::env::var("ESU_ROM").map_err(|_| "ESU_ROM missing".to_owned())?;
    let number = std::env::var("ESU_ROM_NUMBER")
        .map_err(|_| "ESU_ROM_NUMBER missing".to_owned())?
        .parse::<u32>()
        .map_err(|_| "ESU_ROM_NUMBER invalid".to_owned())?;
    let rom_path = config::rom_path(&manifest, &id).map_err(|error| error.to_string())?;
    let rom = config::parse_selected_rom(&read_text(&root.join(&rom_path))?, &id)
        .map_err(|error| error.to_string())?;
    config::validate_rom(&rom, number).map_err(|error| error.to_string())?;
    Ok((rom, number))
}

/// Every base the ROM declares an image role for, in `IMAGE_BASES` order.
///
/// A base is declared by its two `<base>_a`/`<base>_b` partitions; admission
/// guarantees both letters carry the matching `rom-image:<base>` backend, so one
/// match is enough to own a switch device.
fn declared_bases(rom: &config::RomConfig) -> Vec<&'static str> {
    IMAGE_BASES
        .into_iter()
        .filter(|base| {
            rom.partitions.iter().any(|partition| {
                matches!(partition.backend(), Ok(Backend::RomImage(name)) if name == *base)
            })
        })
        .collect()
}

fn run() -> Result<(), String> {
    if std::env::args_os().len() != 1 {
        return Err("ota-stage accepts no arguments".to_owned());
    }

    let (rom, number) = rom_config()?;
    if number < 2 {
        println!("ota-stage: rom={} is not a managed staging ROM", rom.id);
        return Ok(());
    }

    let stage = plan::parse_stage(&std::env::var("ESU_STAGE").unwrap_or_default())?;
    let bases = declared_bases(&rom);
    if bases.is_empty() {
        println!("ota-stage: rom={} declares no image bases", rom.id);
        return Ok(());
    }

    let mut geometry = Vec::with_capacity(bases.len());
    for base in &bases {
        let path = Path::new(ESP_MOUNT_POINT).join(base_image_path(&rom.id, base));
        let sectors = ota_core::copy::exact_sectors(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        geometry.push(Base { base, sectors });
    }

    let switches = plan::switches(&geometry, number, stage);
    let mut mapper = DeviceMapper::open()?;
    for switch in &switches {
        let table = if switch.staged {
            // The staging LV was activated by the earlier `thin` entry; an
            // absent device is a broken payload, never a reason to guess an
            // error target that would hide the staged set.
            let lv = ota_core::names::stage_lv_name(number, switch.base);
            let dm = ota_core::names::stage_dm_name(number, switch.base);
            let device =
                DeviceMapper::device_number(&dm).map_err(|_| format!("stage LV missing: {lv}"))?;
            switch.table(Some(device))?
        } else {
            switch.table(None)?
        };
        mapper.create(&switch.name, &table, switch.read_only)?;
    }
    mapper.commit();

    println!(
        "ota-stage: rom={} rom_number={} bases={} staged={}",
        rom.id,
        number,
        switches.len(),
        stage.is_some()
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("ota-stage: {error}");
        std::process::exit(1);
    }
}
