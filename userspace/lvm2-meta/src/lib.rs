// SPDX-License-Identifier: Apache-2.0
//! Read-only, bounded LVM2 text metadata and device-mapper table derivation.
//! All LVM and DM sector counts are in 512-byte units, including on 4Kn media.
#![forbid(unsafe_code)]

mod disk;
mod model;
mod table;
pub mod text;

pub use disk::{DiskArea, Metadata, MetadataArea, PhysicalVolumeHeader, RawLocation, read};
pub use model::{Discards, LogicalVolume, PhysicalVolume, Segment, SegmentType, VolumeGroup};
pub use table::{DeviceNumber, Devices, Layer, PhysicalExtent, TableOptions};

pub const SECTOR_SIZE: u64 = 512;
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;
pub const MAX_DEPTH: usize = 32;
pub const MAX_NODES: usize = 32768;
pub const MAX_TOKEN_BYTES: usize = 4096;

/// The adapter must either fill the entire buffer or return an error. It must
/// enforce its own media/partition bounds; reads never include LV data.
pub trait ReadAt {
    fn read_exact_at(&mut self, offset: u64, buffer: &mut [u8]) -> std::io::Result<()>;
}

impl ReadAt for &[u8] {
    fn read_exact_at(&mut self, offset: u64, buffer: &mut [u8]) -> std::io::Result<()> {
        let start = usize::try_from(offset).ok();
        let bytes = start.and_then(|s| s.checked_add(buffer.len()).and_then(|e| self.get(s..e)));
        let bytes = bytes.ok_or_else(|| std::io::Error::from(std::io::ErrorKind::UnexpectedEof))?;
        buffer.copy_from_slice(bytes);
        Ok(())
    }
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    MissingLabel,
    Checksum(&'static str),
    Invalid(&'static str),
    Limit(&'static str),
    Unsupported(String),
    NotFound(String),
    ConflictingMetadata,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "LVM I/O: {e}"),
            Self::MissingLabel => f.write_str("no LVM2 label in sectors 0..4"),
            Self::Checksum(s) => write!(f, "invalid {s} checksum"),
            Self::Invalid(s) => write!(f, "invalid LVM {s}"),
            Self::Limit(s) => write!(f, "LVM {s} exceeds reader limit"),
            Self::Unsupported(s) => write!(f, "unsupported LVM feature: {s}"),
            Self::NotFound(s) => write!(f, "LVM reference not found: {s}"),
            Self::ConflictingMetadata => f.write_str("conflicting LVM metadata copies"),
        }
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

fn add(a: u64, b: u64) -> Result<u64, Error> {
    a.checked_add(b).ok_or(Error::Invalid("integer overflow"))
}
fn mul(a: u64, b: u64) -> Result<u64, Error> {
    a.checked_mul(b).ok_or(Error::Invalid("integer overflow"))
}

// LVM's CRC-32 uses the standard reflected polynomial, a format-specific seed,
// and no final xor. The table is computed at compile time, not on each read.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut value = i as u32;
        let mut bit = 0;
        while bit < 8 {
            value = (value >> 1) ^ (0xedb88320 & (0u32.wrapping_sub(value & 1)));
            bit += 1;
        }
        table[i] = value;
        i += 1;
    }
    table
};
fn checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0xf597a6cf, |crc, byte| {
        (crc >> 8) ^ CRC_TABLE[((crc ^ u32::from(*byte)) & 255) as usize]
    })
}
