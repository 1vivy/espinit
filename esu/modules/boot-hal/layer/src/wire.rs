//! esu's native boot records: GBS1 `Slot-<id>`, GBM1 `MergeStatus-<id>` and the
//! 64-byte misc VAB message they are mirrored into.
//!
//! These bytes are read by the UEFI bootloader, by Surfacer and by the host
//! provisioners, so the layout is fixed and lives here alone. Slot health and
//! merge policy belong to `generic_bootctl_core`; this module only decodes,
//! encodes and projects the records.
use generic_bootctl_core::{Operation, Slot};
use std::io;

/// `Slot-<id>` byte 9: no pending firmware switch request.
pub const NO_PENDING: u8 = 0xff;
/// Offset of the virtual A/B message inside `/dev/block/by-name/misc`.
pub const VAB_OFFSET: u64 = 32 * 1024;

pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn slot_number(slot: u32) -> io::Result<u8> {
    match slot {
        0 => Ok(0),
        1 => Ok(1),
        _ => Err(invalid("slot index is not 0/1")),
    }
}

/// The 24-byte managed `Slot-<id>` record.
///
/// Natural ROM number 1 is stock/record-and-confirm: a request is recorded in
/// `pending` and the confirmed firmware slot stays in `selected`. ROM numbers
/// 2..=5 are record-only and select immediately, so they never carry a pending
/// request. Bytes 10-11, 15 and 19-23 are reserved zero; the shared
/// `Slot::verity_corrupted` field is not part of GBS1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gbs1 {
    pub rom_number: u32,
    /// Confirmed firmware slot for ROM 1; selected HLOS slot for other ROMs.
    pub selected: u8,
    pub pending: u8,
    pub slots: [Slot; 2],
}

impl Gbs1 {
    /// Fresh-install record: the current slot is healthy, the other is unbootable.
    pub fn initial(rom_number: u32, current: u8) -> io::Result<Self> {
        if rom_number == 0 || current > 1 {
            return Err(invalid("invalid ROM number or current slot"));
        }
        let mut record = Self {
            rom_number,
            selected: current,
            pending: NO_PENDING,
            slots: [Slot::default(); 2],
        };
        record.slots[usize::from(current)] = Slot {
            priority: 15,
            tries: 7,
            successful: true,
            verity_corrupted: false,
        };
        Ok(record)
    }

    pub fn decode(data: &[u8]) -> io::Result<Self> {
        if data.len() != 24
            || &data[..4] != b"GBS1"
            || data[10..12] != [0, 0]
            || data[15] != 0
            || data[19..24] != [0; 5]
        {
            return Err(invalid("invalid GBS1 record"));
        }
        let rom_number = u32::from_le_bytes(
            data[4..8]
                .try_into()
                .map_err(|_| invalid("truncated GBS1 ROM number"))?,
        );
        let mut record = Self::initial(rom_number, data[8])?;
        record.pending = data[9];
        if record.pending != NO_PENDING && (record.pending > 1 || rom_number != 1) {
            return Err(invalid("invalid GBS1 pending request"));
        }
        for (slot, offset) in record.slots.iter_mut().zip([12, 16]) {
            if data[offset] > 15 || data[offset + 1] > 7 || data[offset + 2] > 1 {
                return Err(invalid("invalid GBS1 slot fields"));
            }
            *slot = Slot {
                // GBS1 has no separate bootable bit: priority 0 *is* unbootable,
                // which the shared model expresses as `tries == 0`.
                tries: if data[offset] == 0 {
                    0
                } else {
                    data[offset + 1]
                },
                priority: data[offset],
                successful: data[offset + 2] != 0,
                verity_corrupted: false,
            };
        }
        Ok(record)
    }

    pub fn encode(self) -> [u8; 24] {
        let mut bytes = [0; 24];
        bytes[..4].copy_from_slice(b"GBS1");
        bytes[4..8].copy_from_slice(&self.rom_number.to_le_bytes());
        bytes[8] = self.selected;
        bytes[9] = self.pending;
        for (slot, offset) in self.slots.iter().zip([12, 16]) {
            // Bootability is the priority byte, so a slot with tries left is
            // always written bootable and an unbootable slot is written zeroed.
            bytes[offset] = if slot.tries == 0 {
                0
            } else {
                slot.priority.max(1)
            };
            bytes[offset + 1] = slot.tries;
            bytes[offset + 2] = u8::from(slot.successful);
        }
        bytes
    }

    /// The record's own boot target: the pending request for ROM 1, otherwise
    /// the selected slot. The shared service derives the same answer from
    /// slot priority for every record this HAL, Surfacer or the provisioners
    /// write: the boot target always holds the strict maximum priority.
    pub fn booted_slot(self) -> u8 {
        if self.pending == NO_PENDING {
            self.selected
        } else {
            self.pending
        }
    }

    /// AIDL operation projected onto GBS1 bytes 8/9.
    pub fn with_operation(mut self, operation: Operation) -> io::Result<Self> {
        match operation {
            Operation::SetActive(target) => {
                let target = slot_number(target)?;
                if self.rom_number == 1 {
                    // Requesting the confirmed slot cancels a pending switch.
                    self.pending = if target == self.selected {
                        NO_PENDING
                    } else {
                        target
                    };
                } else {
                    self.selected = target;
                    self.pending = NO_PENDING;
                }
            }
            Operation::SetUnbootable(target) => {
                if self.pending == slot_number(target)? {
                    self.pending = NO_PENDING;
                }
            }
            Operation::MarkSuccessful(_) => {}
        }
        Ok(self)
    }
}

/// The 8-byte managed `MergeStatus-<id>` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gbm1 {
    /// AIDL MergeStatus: NONE=0, UNKNOWN=1, SNAPSHOTTED=2, MERGING=3, CANCELLED=4.
    pub status: u8,
    pub source: u8,
}

impl Gbm1 {
    pub fn decode(data: &[u8]) -> io::Result<Self> {
        if data.len() != 8
            || &data[..4] != b"GBM1"
            || data[4] > 4
            || data[5] > 1
            || data[6..] != [0, 0]
        {
            return Err(invalid("invalid GBM1 record"));
        }
        Ok(Self {
            status: data[4],
            source: data[5],
        })
    }

    pub fn encode(self) -> [u8; 8] {
        [b'G', b'B', b'M', b'1', self.status, self.source, 0, 0]
    }

    /// The V2 virtual A/B message at misc byte 32768.
    pub fn message(self) -> [u8; 64] {
        let mut bytes = [0; 64];
        bytes[0] = 2;
        bytes[1..5].copy_from_slice(&0x5674_0ab0u32.to_le_bytes());
        bytes[5] = self.status;
        bytes[6] = self.source;
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(priority: u8, tries: u8, successful: bool) -> Slot {
        Slot {
            priority,
            tries,
            successful,
            verity_corrupted: false,
        }
    }

    #[test]
    fn rom_one_records_without_confirming_and_cancel_keeps_firmware() {
        let record = Gbs1::initial(1, 1).unwrap();
        assert_eq!(
            (record.selected, record.pending, record.booted_slot()),
            (1, NO_PENDING, 1)
        );
        let requested = record.with_operation(Operation::SetActive(0)).unwrap();
        assert_eq!(
            (
                requested.selected,
                requested.pending,
                requested.booted_slot()
            ),
            (1, 0, 0)
        );
        // A success report never confirms or erases a pending firmware switch.
        assert_eq!(requested.pending, 0);
        let failed = requested
            .with_operation(Operation::SetUnbootable(0))
            .unwrap();
        assert_eq!((failed.selected, failed.pending), (1, NO_PENDING));
        let cancelled = failed
            .with_operation(Operation::SetActive(0))
            .unwrap()
            .with_operation(Operation::SetActive(1))
            .unwrap();
        assert_eq!((cancelled.selected, cancelled.pending), (1, NO_PENDING));
        assert_eq!(Gbs1::decode(&cancelled.encode()).unwrap(), cancelled);
    }

    #[test]
    fn custom_rom_selection_is_independent_and_selects_immediately() {
        let a = Gbs1::initial(2, 0)
            .unwrap()
            .with_operation(Operation::SetActive(1))
            .unwrap();
        assert_eq!((a.selected, a.pending), (1, NO_PENDING));
        assert_eq!(Gbs1::decode(&a.encode()).unwrap(), a);
        let b = Gbs1::initial(3, 1).unwrap();
        assert_eq!(b, Gbs1::initial(3, 1).unwrap());
        // ROM >= 2 records never carry a pending firmware request.
        let invalid = Gbs1 {
            pending: 0,
            ..Gbs1::initial(2, 0).unwrap()
        };
        assert!(Gbs1::decode(&invalid.encode()).is_err());
    }

    #[test]
    fn invalid_slots_and_corrupt_state_fail_closed() {
        let record = Gbs1::initial(1, 0).unwrap();
        assert!(record.with_operation(Operation::SetActive(7)).is_err());
        assert!(record.with_operation(Operation::SetUnbootable(2)).is_err());
        for corrupt in [
            {
                let mut bytes = record.encode().to_vec();
                bytes[0] = b'X';
                bytes
            },
            {
                let bytes = record.encode();
                bytes[..23].to_vec()
            },
            {
                let mut bytes = record.encode().to_vec();
                bytes[9] = 2;
                bytes
            },
            {
                let mut bytes = record.encode().to_vec();
                bytes[4..8].copy_from_slice(&0u32.to_le_bytes());
                bytes
            },
            {
                let mut bytes = record.encode().to_vec();
                bytes[8] = 2;
                bytes
            },
            {
                let mut bytes = record.encode().to_vec();
                bytes[12] = 16;
                bytes
            },
            {
                let mut bytes = record.encode().to_vec();
                bytes[13] = 8;
                bytes
            },
            {
                let mut bytes = record.encode().to_vec();
                bytes[14] = 2;
                bytes
            },
            {
                let mut bytes = record.encode().to_vec();
                bytes[20] = 1;
                bytes
            },
        ] {
            assert!(Gbs1::decode(&corrupt).is_err(), "{corrupt:?}");
        }
        assert_eq!(Gbs1::decode(&record.encode()).unwrap(), record);
    }

    #[test]
    fn priority_is_the_bootable_byte_for_tries_bearing_slots() {
        // A record where priority and tries disagree: unbootable wins on read and
        // the next encode writes the zeroed pair, never a stale priority.
        let mut bytes = Gbs1::initial(1, 1).unwrap().encode();
        bytes[16] = 0;
        bytes[17] = 5;
        let record = Gbs1::decode(&bytes).unwrap();
        assert_eq!(record.slots[1], slot(0, 0, true));
        assert_eq!(record.encode()[16..18], [0, 0]);
        // Marking a zeroed slot successful raises both to at least one.
        let raised = Gbs1 {
            slots: [slot(0, 0, false), slot(1, 1, true)],
            ..Gbs1::initial(1, 1).unwrap()
        }
        .encode();
        assert_eq!(raised[12..15], [0, 0, 0]);
        assert_eq!(raised[16..19], [1, 1, 1]);
    }

    #[test]
    fn advance_and_misc_layout_match_the_vab_contract() {
        let merge = Gbm1 {
            status: 2,
            source: 0,
        };
        assert_eq!(&merge.message()[..7], &[2, 0xb0, 0x0a, 0x74, 0x56, 2, 0]);
        assert_eq!(Gbm1::decode(&merge.encode()).unwrap(), merge);
        for status in 0..=4 {
            let record = Gbm1 { status, source: 1 };
            assert_eq!(Gbm1::decode(&record.encode()).unwrap(), record);
        }
        for corrupt in [
            {
                let mut bytes = merge.encode().to_vec();
                bytes[5] = 2;
                bytes
            },
            {
                let mut bytes = merge.encode().to_vec();
                bytes[4] = 5;
                bytes
            },
            {
                let mut bytes = merge.encode().to_vec();
                bytes[6] = 1;
                bytes
            },
            {
                let mut bytes = merge.encode().to_vec();
                bytes[0] = b'X';
                bytes
            },
            {
                let bytes = merge.encode();
                bytes[..7].to_vec()
            },
        ] {
            assert!(Gbm1::decode(&corrupt).is_err(), "{corrupt:?}");
        }
    }
}
