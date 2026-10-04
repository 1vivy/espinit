// SPDX-License-Identifier: Apache-2.0
use lvm2_meta::{ReadAt, SegmentType};
use std::io::{Read, Seek, SeekFrom};

struct Source(std::fs::File);
impl ReadAt for Source {
    fn read_exact_at(&mut self, offset: u64, bytes: &mut [u8]) -> std::io::Result<()> {
        self.0.seek(SeekFrom::Start(offset))?;
        self.0.read_exact(bytes)
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("usage: inspect <PV image or metadata-bearing prefix>")?;
    let metadata = lvm2_meta::read(&mut Source(std::fs::File::open(path)?))?;
    println!(
        "VG {} seqno={} PV={} size={}",
        metadata.vg.name(),
        metadata.vg.seqno(),
        metadata.pv.id,
        metadata.pv.device_size
    );
    for (name, lv) in metadata.vg.logical_volumes() {
        println!(
            "{name}: {} sectors",
            lv.extent_count() * metadata.vg.extent_size()
        );
        if lv
            .segments
            .iter()
            .all(|s| matches!(s.kind, SegmentType::Linear { .. }))
        {
            for extent in metadata.vg.physical_extents(name)? {
                println!(
                    "  bytes {}+{} -> PV {} at {}",
                    extent.logical_offset, extent.length, extent.pv_id, extent.physical_offset
                );
            }
        } else {
            for segment in &lv.segments {
                println!("  {:?}", segment.kind);
            }
        }
    }
    Ok(())
}
