//! Per-ROM boot authority. No GPT or UFS operations are present in this crate.
pub mod service;
pub mod storage;

pub const INVALID_SLOT: i32 = -1;
pub const COMMAND_FAILED: i32 = -2;
pub const NO_PENDING: u8 = 0xff;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    pub priority: u8,
    pub tries: u8,
    pub successful: bool,
}
impl Slot {
    pub fn bootable(self) -> bool {
        self.priority != 0 && (self.successful || self.tries != 0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub rom_number: u32,
    /// Confirmed firmware slot for ROM 1; selected HLOS slot for other ROMs.
    pub selected: u8,
    pub pending: u8,
    pub slots: [Slot; 2],
}
impl State {
    pub fn initial(rom_number: u32, current: u8) -> Result<Self, i32> {
        if rom_number == 0 || current > 1 {
            return Err(COMMAND_FAILED);
        }
        let mut state = Self {
            rom_number,
            selected: current,
            pending: NO_PENDING,
            slots: [Slot {
                priority: 0,
                tries: 0,
                successful: false,
            }; 2],
        };
        state.slots[usize::from(current)] = Slot {
            priority: 15,
            tries: 7,
            successful: true,
        };
        Ok(state)
    }

    pub fn decode(data: &[u8]) -> Result<Self, i32> {
        if data.len() != 24
            || &data[..4] != b"GBS1"
            || data[10..12] != [0, 0]
            || data[15] != 0
            || data[19..24] != [0; 5]
        {
            return Err(COMMAND_FAILED);
        }
        let rom_number = u32::from_le_bytes(data[4..8].try_into().map_err(|_| COMMAND_FAILED)?);
        let mut state = Self::initial(rom_number, data[8])?;
        state.pending = data[9];
        if state.pending != NO_PENDING && (state.pending > 1 || rom_number != 1) {
            return Err(COMMAND_FAILED);
        }
        for (slot, offset) in state.slots.iter_mut().zip([12, 16]) {
            if data[offset] > 15 || data[offset + 1] > 7 || data[offset + 2] > 1 {
                return Err(COMMAND_FAILED);
            }
            *slot = Slot {
                priority: data[offset],
                tries: data[offset + 1],
                successful: data[offset + 2] != 0,
            };
        }
        Ok(state)
    }

    pub fn encode(self) -> [u8; 24] {
        let mut bytes = [0; 24];
        bytes[..4].copy_from_slice(b"GBS1");
        bytes[4..8].copy_from_slice(&self.rom_number.to_le_bytes());
        bytes[8] = self.selected;
        bytes[9] = self.pending;
        for (slot, offset) in self.slots.iter().zip([12, 16]) {
            bytes[offset] = slot.priority;
            bytes[offset + 1] = slot.tries;
            bytes[offset + 2] = u8::from(slot.successful);
        }
        bytes
    }

    pub fn active(self) -> u8 {
        if self.pending == NO_PENDING {
            self.selected
        } else {
            self.pending
        }
    }

    pub fn set_active(&mut self, slot: i32) -> Result<(), i32> {
        let index = slot_index(slot)?;
        // Repeated activation is the AOSP retry operation, not an idempotent no-op.
        self.slots[index] = Slot {
            priority: 15,
            tries: 7,
            successful: false,
        };
        self.slots[1 - index].priority = self.slots[1 - index].priority.min(14);
        if self.rom_number == 1 {
            // Requesting the confirmed slot cancels a previous pending switch.
            self.pending = if slot as u8 == self.selected {
                NO_PENDING
            } else {
                slot as u8
            };
        } else {
            self.selected = slot as u8;
        }
        Ok(())
    }

    pub fn mark_successful(&mut self, current: u8) -> Result<(), i32> {
        let slot = &mut self.slots[slot_index(i32::from(current))?];
        slot.successful = true;
        slot.tries = slot.tries.max(1);
        slot.priority = slot.priority.max(1);
        // A success report must never confirm or erase a pending firmware switch.
        Ok(())
    }

    pub fn set_unbootable(&mut self, slot: i32) -> Result<(), i32> {
        let index = slot_index(slot)?;
        self.slots[index] = Slot {
            priority: 0,
            tries: 0,
            successful: false,
        };
        if self.pending == slot as u8 {
            self.pending = NO_PENDING;
        }
        // Do not silently select an alternate firmware slot or HLOS slot.
        Ok(())
    }
}

pub fn slot_index(slot: i32) -> Result<usize, i32> {
    match slot {
        0 => Ok(0),
        1 => Ok(1),
        _ => Err(INVALID_SLOT),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Merge {
    pub status: u8,
    pub source: u8,
}
impl Merge {
    pub fn decode(data: &[u8]) -> Result<Self, i32> {
        if data.len() != 8
            || &data[..4] != b"GBM1"
            || data[4] > 4
            || data[5] > 1
            || data[6..] != [0, 0]
        {
            return Err(COMMAND_FAILED);
        }
        Ok(Self {
            status: data[4],
            source: data[5],
        })
    }
    pub fn encode(self) -> [u8; 8] {
        [b'G', b'B', b'M', b'1', self.status, self.source, 0, 0]
    }
    pub fn visible(self, current: u8) -> u8 {
        // AOSP GetMiscVirtualAbMergeStatus: source-slot reversion discards snapshots.
        if self.status == 2 && self.source == current {
            0
        } else {
            self.status
        }
    }
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

    #[test]
    fn rom_one_records_without_confirming_and_cancel_keeps_firmware() {
        let mut s = State::initial(1, 1).unwrap();
        s.set_active(0).unwrap();
        assert_eq!((s.selected, s.pending, s.active()), (1, 0, 0));
        s.mark_successful(1).unwrap();
        assert_eq!(s.pending, 0);
        s.set_unbootable(0).unwrap();
        assert_eq!((s.selected, s.pending), (1, NO_PENDING));
        s.set_active(0).unwrap();
        s.set_active(1).unwrap();
        assert_eq!((s.selected, s.pending), (1, NO_PENDING));
    }

    #[test]
    fn custom_rom_selection_is_independent_and_retry_resets_health() {
        let mut a = State::initial(2, 0).unwrap();
        let b = State::initial(3, 1).unwrap();
        a.set_active(1).unwrap();
        assert_eq!((a.selected, a.pending), (1, NO_PENDING));
        assert_eq!(b, State::initial(3, 1).unwrap());
        a.mark_successful(1).unwrap();
        a.set_active(1).unwrap();
        assert_eq!(
            a.slots[1],
            Slot {
                priority: 15,
                tries: 7,
                successful: false
            }
        );
        assert_eq!(State::decode(&a.encode()), Ok(a));
    }

    #[test]
    fn invalid_slots_and_corrupt_state_fail_closed() {
        let mut s = State::initial(1, 0).unwrap();
        let before = s;
        for slot in [-1, 2, i32::MAX] {
            assert_eq!(s.set_active(slot), Err(INVALID_SLOT));
            assert_eq!(s.set_unbootable(slot), Err(INVALID_SLOT));
            assert_eq!(s, before);
        }
        let mut bytes = s.encode();
        bytes[9] = 2;
        assert_eq!(State::decode(&bytes), Err(COMMAND_FAILED));
        bytes = s.encode();
        bytes[12] = 16;
        assert!(State::decode(&bytes).is_err());
    }

    #[test]
    fn source_reversion_and_misc_layout_match_vab_contract() {
        let merge = Merge {
            status: 2,
            source: 0,
        };
        assert_eq!(merge.visible(0), 0);
        assert_eq!(merge.visible(1), 2);
        assert_eq!(Merge::decode(&merge.encode()), Ok(merge));
        assert_eq!(&merge.message()[..7], &[2, 0xb0, 0x0a, 0x74, 0x56, 2, 0]);
        for status in 0..=4 {
            let m = Merge { status, source: 1 };
            assert_eq!(m.visible(0), status);
        }
    }
}
