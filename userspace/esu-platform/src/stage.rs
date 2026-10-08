// SPDX-License-Identifier: GPL-3.0-only
//! The OTA transaction record: bdsvars `Stage-<id>`.
//!
//! One 8-byte record per managed ROM holds the transaction state and nothing
//! else. The staging and selected *letters* are derived from the booted slot and
//! the `Slot-<id>` record, so a staged set can never disagree with the boot
//! state about which slot it belongs to. The wire layout is read by esuinit,
//! the boot HAL, `ota-stage` and Surfacer, so it is fixed here and mirrored
//! byte-for-byte by Surfacer's decoder.

use std::io;

/// Record magic: the four bytes before the state byte.
pub const MAGIC: &[u8; 4] = b"GBT1";
/// Encoded record length, including the magic.
pub const RECORD_BYTES: usize = 8;

/// The transaction state of one ROM. `None` is also what an absent variable
/// means; the two are indistinguishable on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum StageState {
    /// No transaction: no staged set exists for this ROM.
    None = 0,
    /// A staging set exists and holds the target of an in-progress update.
    Staging = 1,
    /// The target was sealed (payload and module set written); the staged set
    /// is what the next boot of the staged letter runs.
    Sealed = 2,
    /// The staged set booted and was marked successful; its bytes are being
    /// promoted into the ROM's base images.
    Promote = 3,
}

impl StageState {
    /// Encode the record. Every byte is defined: reserved bytes are zero.
    pub fn encode(self) -> [u8; RECORD_BYTES] {
        let mut bytes = [0; RECORD_BYTES];
        bytes[..4].copy_from_slice(MAGIC);
        bytes[4] = self as u8;
        bytes
    }

    /// Decode a record. Length, magic, state range and the reserved bytes are
    /// all checked; anything else is `InvalidData`.
    pub fn decode(data: &[u8]) -> io::Result<Self> {
        if data.len() != RECORD_BYTES || &data[..4] != MAGIC || data[5..] != [0, 0, 0] {
            return Err(invalid());
        }
        match data[4] {
            0 => Ok(Self::None),
            1 => Ok(Self::Staging),
            2 => Ok(Self::Sealed),
            3 => Ok(Self::Promote),
            _ => Err(invalid()),
        }
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Stage record")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_round_trips_through_its_exact_bytes() {
        for (state, byte) in [
            (StageState::None, 0x00_u8),
            (StageState::Staging, 0x01),
            (StageState::Sealed, 0x02),
            (StageState::Promote, 0x03),
        ] {
            assert_eq!(
                state.encode(),
                [b'G', b'B', b'T', b'1', byte, 0, 0, 0],
                "{state:?}"
            );
            assert_eq!(StageState::decode(&state.encode()).unwrap(), state);
        }
    }

    #[test]
    fn every_other_record_is_rejected() {
        let cases: &[&[u8]] = &[
            b"",
            b"GBT1",
            b"GBT1\x00\x00\x00",
            b"GBT1\x00\x00\x00\x00\x00",
            b"GBT1\x04\x00\x00\x00",
            b"GBT1\xff\x00\x00\x00",
            b"GBT0\x00\x00\x00\x00",
            b"gbt1\x00\x00\x00\x00",
            b"GBT1\x00\x00\x00\x01",
            b"GBT1\x02\x01\x00\x00",
            b"GBS1\x00\x00\x00\x00",
        ];
        for case in cases {
            let error = StageState::decode(case).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{case:?}");
            assert_eq!(error.to_string(), "Stage record", "{case:?}");
        }
    }
}
