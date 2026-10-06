// SPDX-License-Identifier: GPL-3.0-only
mod block;
mod plan;

use lvm2_meta::ReadAt;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

struct Source(File);

impl ReadAt for Source {
    fn read_exact_at(&mut self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        self.0.read_exact_at(buffer, offset)
    }
}

fn identity() -> Result<(String, u32), String> {
    let rom = std::env::var("ESU_ROM").map_err(|_| "ESU_ROM missing")?;
    let number = std::env::var("ESU_ROM_NUMBER")
        .map_err(|_| "ESU_ROM_NUMBER missing")?
        .parse::<u32>()
        .map_err(|_| "ESU_ROM_NUMBER invalid")?;
    if rom.is_empty() || !(1..=esu_platform::efivars::MAX_ROM_NUMBER).contains(&number) {
        return Err("invalid PID1 ROM identity".into());
    }
    Ok((rom, number))
}

fn run() -> Result<(), String> {
    if std::env::args_os().len() != 1 {
        return Err("thin-activate accepts no arguments".to_owned());
    }
    let (rom, rom_number) = identity()?;
    let physical = block::open_userdata()
        .map_err(|error| format!("cannot open physical userdata PV: {error}"))?;
    let bytes = physical
        .sectors
        .checked_mul(lvm2_meta::SECTOR_SIZE)
        .ok_or("physical userdata size overflow")?;
    let mut source = Source(physical.file);
    let metadata = lvm2_meta::read(&mut source).map_err(|error| error.to_string())?;
    if metadata.pv.device_size != bytes {
        return Err(format!(
            "LVM PV records {} bytes but physical userdata has {bytes}",
            metadata.pv.device_size
        ));
    }
    let declared = metadata
        .vg
        .physical_volumes()
        .values()
        .next()
        .ok_or("VG rom has no physical volume")?;
    if declared.id.replace('-', "") != metadata.pv.id {
        return Err("LVM label and VG metadata name different physical volumes".to_owned());
    }
    if declared.dev_size != Some(physical.sectors) {
        return Err("VG metadata device size differs from physical userdata".to_owned());
    }

    let mut mapper = dm::DeviceMapper::open()?;
    let devices = plan::activate_visible(&metadata.vg, physical.number, &mut mapper)?;
    let count = devices.logical_volumes.len();
    mapper.commit();
    println!(
        "thin-activate: rom={rom} rom_number={rom_number} vg={} seqno={} activated_lvs={count}",
        metadata.vg.name(),
        metadata.vg.seqno(),
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("thin-activate: {error}");
        std::process::exit(1);
    }
}
