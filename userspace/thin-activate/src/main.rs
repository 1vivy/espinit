// SPDX-License-Identifier: GPL-3.0-only
mod block;
mod generation;
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

fn check_generation(expected: Option<&std::ffi::OsStr>, built: &str) -> Result<(), String> {
    let expected = expected
        .and_then(std::ffi::OsStr::to_str)
        .ok_or("ESPINIT_GENERATION is missing or non-UTF-8")?;
    if expected != built {
        return Err("compiled generation does not match ESPINIT_GENERATION".to_owned());
    }
    Ok(())
}

fn run() -> Result<(), String> {
    if std::env::args_os().len() != 1 {
        return Err("thin-activate accepts no arguments".to_owned());
    }
    check_generation(
        std::env::var_os("ESPINIT_GENERATION").as_deref(),
        generation::generation(),
    )?;
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
        "thin-activate: generation={} vg={} seqno={} activated_lvs={count}",
        generation::generation(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn generation_must_be_present_and_exact() {
        check_generation(Some(OsStr::new("release-1")), "release-1").unwrap();
        assert!(check_generation(None, "release-1").is_err());
        assert!(check_generation(Some(OsStr::new("release-2")), "release-1").is_err());
    }
}
