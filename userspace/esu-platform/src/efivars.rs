// SPDX-License-Identifier: GPL-3.0-only
//! Project bdsvars through efivarfs. Files contain four little-endian attribute
//! bytes followed by the variable payload; writes must be a single operation.

use crate::stage::StageState;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const PROJECT_GUID: &str = "7a5e4b1c-0d3f-4e62-9b8a-1c2d3e4f5a6b";
/// The TEE's per-ROM rollback windows support ROM numbers 1 through 5.
pub const MAX_ROM_NUMBER: u32 = 5;
const ATTRIBUTES: u32 = 7;

/// Parsing failures remain distinct from transient filesystem failures.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    BootedRomInvalid,
    RomRecordMissing,
    RomNumberInvalid,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "efivarfs: {error}"),
            Self::BootedRomInvalid => f.write_str("BootedRomInvalid"),
            Self::RomRecordMissing => f.write_str("RomRecordMissing"),
            Self::RomNumberInvalid => f.write_str("RomNumberInvalid"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

fn safe_component(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn variable_path(root: &Path, name: &str) -> io::Result<PathBuf> {
    if !safe_component(name, 255 - 1 - PROJECT_GUID.len()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid efivar name",
        ));
    }
    Ok(root.join(format!("{name}-{PROJECT_GUID}")))
}

/// Read attributes and payload. Only a missing variable returns `None`.
/// An incomplete attribute header is `InvalidData`.
pub fn read(root: &Path, name: &str) -> io::Result<Option<(u32, Vec<u8>)>> {
    let path = variable_path(root, name)?;
    let mut bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if bytes.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated efivar attribute header",
        ));
    }
    let attributes = u32::from_le_bytes(bytes[..4].try_into().unwrap());
    bytes.drain(..4);
    Ok(Some((attributes, bytes)))
}

/// Replace a variable with one write syscall containing attributes and payload.
/// Interrupted or partial writes fail without retrying: retrying would perform
/// another firmware variable transaction, not complete the original one.
pub fn write(root: &Path, name: &str, attributes: u32, data: &[u8]) -> io::Result<()> {
    let path = variable_path(root, name)?;
    let mut bytes = Vec::with_capacity(4 + data.len());
    bytes.extend_from_slice(&attributes.to_le_bytes());
    bytes.extend_from_slice(data);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    let written = file.write(&bytes)?;
    if written != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "partial efivar write",
        ));
    }
    Ok(())
}

/// Resolve the dispatched ROM. Missing BootedRom and `direct` are unmanaged;
/// malformed records and I/O failures never become unmanaged boots.
pub fn booted_rom(root: &Path) -> Result<Option<String>> {
    let record = read(root, "BootedRom").map_err(|error| {
        if error.kind() == io::ErrorKind::InvalidData {
            Error::BootedRomInvalid
        } else {
            Error::Io(error)
        }
    })?;
    let Some((attributes, data)) = record else {
        return Ok(None);
    };
    if attributes != ATTRIBUTES {
        return Err(Error::BootedRomInvalid);
    }
    let nul = data
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(Error::BootedRomInvalid)?;
    let id = std::str::from_utf8(&data[..nul])
        .ok()
        .filter(|id| safe_component(id, 59))
        .ok_or(Error::BootedRomInvalid)?;
    Ok((id != "direct").then(|| id.to_owned()))
}

/// Reject a ROM id that cannot name a variable. The bound matches
/// [`booted_rom`]'s, so every id the dispatcher can produce is accepted here and
/// nothing else is.
fn check_id(id: &str) -> io::Result<()> {
    if !safe_component(id, 59) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid efivar ROM id",
        ));
    }
    Ok(())
}

/// Read the OTA transaction record of a managed ROM. An absent variable is
/// [`StageState::None`]; a present record with wrong attributes, length, magic
/// or reserved bytes is `InvalidData` and never a silent `None`.
///
/// Project bdsvars are marked removable by the `efivarfs` project-GUID patch,
/// which is what makes the shared [`write`] path usable for them: no immutable
/// flag has to be cleared before a write, exactly as for `Slot-<id>`.
pub fn stage(root: &Path, id: &str) -> io::Result<StageState> {
    check_id(id)?;
    let record = read(root, &format!("Stage-{id}"))?;
    let Some((attributes, data)) = record else {
        return Ok(StageState::None);
    };
    if attributes != ATTRIBUTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Stage record"));
    }
    StageState::decode(&data)
}

/// Replace the OTA transaction record of a managed ROM. Like every other
/// project variable this is one write syscall containing the attribute prefix
/// followed by the 8-byte record.
pub fn write_stage(root: &Path, id: &str, state: StageState) -> io::Result<()> {
    check_id(id)?;
    write(root, &format!("Stage-{id}"), ATTRIBUTES, &state.encode())
}

/// The selected HLOS slot of a managed ROM: byte 8 of the already-written GBS1
/// `Slot-<id>` record. The record itself stays the authority for the number and
/// the per-slot health fields; this reads one byte out of it.
pub fn selected_slot(root: &Path, id: &str) -> io::Result<u8> {
    check_id(id)?;
    let record = read(root, &format!("Slot-{id}"))?;
    let Some((attributes, data)) = record else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Slot record missing",
        ));
    };
    if attributes != ATTRIBUTES || data.len() < 9 || &data[..4] != b"GBS1" || data[8] > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Slot record invalid",
        ));
    }
    Ok(data[8])
}

/// Read a managed ROM's GBS1 record. The number is payload bytes 4..8, hence
/// on-disk bytes 8..12 after the efivarfs attribute header.
pub fn rom_number(root: &Path, id: &str) -> Result<u32> {
    if !safe_component(id, 59) {
        return Err(Error::RomNumberInvalid);
    }
    let record = read(root, &format!("Slot-{id}")).map_err(|error| {
        if error.kind() == io::ErrorKind::InvalidData {
            Error::RomNumberInvalid
        } else {
            Error::Io(error)
        }
    })?;
    let (attributes, data) = record.ok_or(Error::RomRecordMissing)?;
    if attributes != ATTRIBUTES || data.len() < 8 || &data[..4] != b"GBS1" {
        return Err(Error::RomNumberInvalid);
    }
    let number = u32::from_le_bytes(data[4..8].try_into().unwrap());
    if !(1..=MAX_ROM_NUMBER).contains(&number) {
        return Err(Error::RomNumberInvalid);
    }
    Ok(number)
}
