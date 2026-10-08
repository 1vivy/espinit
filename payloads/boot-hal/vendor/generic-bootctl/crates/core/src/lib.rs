//! Transport-independent Android boot-control semantics. No Android dependencies.
use std::ffi::CStr;
use std::io;

pub const INVALID_SLOT: i32 = -1;
pub const COMMAND_FAILED: i32 = -2;
pub const WRITE_PROPERTY: &str = "persist.generic_bootctl.rw";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Slot {
    pub priority: u8,
    pub tries: u8,
    pub successful: bool,
    pub verity_corrupted: bool,
}
impl Slot {
    pub fn bootable(self) -> bool {
        self.tries != 0
    }
}
/// Native metadata policy when the current slot is marked successful.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HealthOnSuccess {
    #[default]
    ResetToOne,
    PreserveNonZero,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub slots: Vec<Slot>,
    pub max_priority: u8,
    pub max_tries: u8,
    pub clear_successful_on_activate: bool,
    pub successful_requires_bootable: bool,
    pub health_on_success: HealthOnSuccess,
}
impl State {
    pub fn active(&self, current: u32) -> Option<u32> {
        let mut active = current as usize;
        self.slots.get(active)?;
        for (i, slot) in self.slots.iter().enumerate() {
            if slot.priority > self.slots[active].priority {
                active = i;
            }
        }
        Some(active as u32)
    }
}
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeStatus {
    None = 0,
    Unknown = 1,
    Snapshotted = 2,
    Merging = 3,
    Cancelled = 4,
}
impl TryFrom<i32> for MergeStatus {
    type Error = i32;
    fn try_from(value: i32) -> Result<Self, i32> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Unknown),
            2 => Ok(Self::Snapshotted),
            3 => Ok(Self::Merging),
            4 => Ok(Self::Cancelled),
            _ => Err(COMMAND_FAILED),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Merge {
    pub status: MergeStatus,
    pub source: u32,
}
impl Merge {
    pub fn visible(self, current: u32) -> MergeStatus {
        if self.status == MergeStatus::Snapshotted && self.source == current {
            MergeStatus::None
        } else {
            self.status
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    MarkSuccessful(u32),
    SetActive(u32),
    SetUnbootable(u32),
}

/// Implementations must not initialize, repair, or write storage in read methods.
/// Writes must preserve unrelated bytes and verify persisted data by readback.
/// `commit` receives the entire validated next state and the operation for native
/// storage side effects (notably the UFS boot-LUN selection on SetActive).
pub trait Backend: Send {
    /// Immutable slot count discovered at construction; must not access storage.
    fn slot_count(&self) -> u32;
    /// Prepare state-dependent operations, never metadata/count/suffix queries.
    /// Implementations must retain the read-only guarantee of read operations.
    fn prepare(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn read_state(&mut self) -> io::Result<State>;
    fn commit(&mut self, state: &State, operation: Operation) -> io::Result<()>;
    fn read_merge(&mut self) -> io::Result<Merge>;
    fn write_merge(&mut self, merge: Merge) -> io::Result<()>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    Int(i32),
    Bool(bool),
    Text(&'static CStr),
    Void,
}

/// A single instance must be serialized by its transport. The gate is evaluated
/// afresh before EVERY mutation; only the exact property value "1" enables it.
pub struct Service {
    backend: Box<dyn Backend>,
    current: u32,
    gate: Box<dyn Fn() -> bool + Send>,
}
impl Service {
    pub fn new(
        backend: Box<dyn Backend>,
        current: u32,
        gate: Box<dyn Fn() -> bool + Send>,
    ) -> Self {
        Self {
            backend,
            current,
            gate,
        }
    }
    pub fn current_slot(&self) -> u32 {
        self.current
    }
    pub fn number_slots(&mut self) -> Result<u32, i32> {
        let count = self.backend.slot_count();
        if !(1..=4).contains(&count) || self.current >= count {
            return Err(COMMAND_FAILED);
        }
        Ok(count)
    }
    pub fn suffix(&mut self, slot: u32) -> Result<&'static CStr, i32> {
        if slot >= self.number_slots()? {
            return Ok(c"");
        }
        Ok(match slot {
            0 => c"_a",
            1 => c"_b",
            2 => c"_c",
            3 => c"_d",
            _ => c"",
        })
    }
    pub fn active_slot(&mut self) -> Result<u32, i32> {
        self.state()?.active(self.current).ok_or(COMMAND_FAILED)
    }
    pub fn is_bootable(&mut self, slot: u32) -> Result<bool, i32> {
        Ok(self.slot(slot)?.bootable())
    }
    pub fn is_successful(&mut self, slot: u32) -> Result<bool, i32> {
        self.validate_slot(slot)?;
        let state = self.state()?;
        let s = state.slots.get(slot as usize).ok_or(INVALID_SLOT)?;
        Ok(s.successful && (!state.successful_requires_bootable || s.bootable()))
    }
    pub fn mark_successful(&mut self) -> Result<(), i32> {
        self.mutate(Operation::MarkSuccessful(self.current))
    }
    pub fn set_active(&mut self, slot: u32) -> Result<(), i32> {
        self.mutate(Operation::SetActive(slot))
    }
    pub fn set_unbootable(&mut self, slot: u32) -> Result<(), i32> {
        self.mutate(Operation::SetUnbootable(slot))
    }
    pub fn merge_status(&mut self) -> Result<MergeStatus, i32> {
        self.backend.prepare().map_err(|_| COMMAND_FAILED)?;
        Ok(self
            .backend
            .read_merge()
            .map_err(|_| COMMAND_FAILED)?
            .visible(self.current))
    }
    pub fn set_merge_status(&mut self, status: MergeStatus) -> Result<(), i32> {
        self.allow_write()?;
        self.backend.prepare().map_err(|_| COMMAND_FAILED)?;
        self.backend
            .write_merge(Merge {
                status,
                source: self.current,
            })
            .map_err(|_| COMMAND_FAILED)
    }
    /// Frozen AIDL V1 transaction numbering; HIDL should use the named methods.
    pub fn execute(&mut self, code: u32, input: i32) -> Result<Reply, i32> {
        match code {
            1 => Ok(Reply::Int(self.active_slot()? as i32)),
            2 => Ok(Reply::Int(self.current_slot() as i32)),
            3 => Ok(Reply::Int(self.number_slots()? as i32)),
            4 => Ok(Reply::Int(self.merge_status()? as i32)),
            5 => Ok(Reply::Text(self.suffix(input as u32)?)),
            6 => Ok(Reply::Bool(self.is_bootable(input as u32)?)),
            7 => Ok(Reply::Bool(self.is_successful(input as u32)?)),
            8 => {
                self.mark_successful()?;
                Ok(Reply::Void)
            }
            9 => {
                self.set_active(input as u32)?;
                Ok(Reply::Void)
            }
            10 => {
                self.set_unbootable(input as u32)?;
                Ok(Reply::Void)
            }
            11 => {
                self.set_merge_status(input.try_into()?)?;
                Ok(Reply::Void)
            }
            16_777_214 => Ok(Reply::Text(c"2400346954240a5de495a1debc81429dd012d7b7")),
            16_777_215 => Ok(Reply::Int(1)),
            _ => Err(COMMAND_FAILED),
        }
    }
    fn state(&mut self) -> Result<State, i32> {
        self.backend.prepare().map_err(|_| COMMAND_FAILED)?;
        let s = self.backend.read_state().map_err(|_| COMMAND_FAILED)?;
        if s.slots.len() != self.number_slots()? as usize
            || self.current as usize >= s.slots.len()
            || s.max_priority == 0
        {
            return Err(COMMAND_FAILED);
        }
        Ok(s)
    }
    fn slot(&mut self, slot: u32) -> Result<Slot, i32> {
        self.validate_slot(slot)?;
        self.state()?
            .slots
            .get(slot as usize)
            .copied()
            .ok_or(INVALID_SLOT)
    }
    fn validate_slot(&self, slot: u32) -> Result<(), i32> {
        if slot >= self.backend.slot_count() {
            Err(INVALID_SLOT)
        } else {
            Ok(())
        }
    }
    fn allow_write(&self) -> Result<(), i32> {
        if (self.gate)() {
            Ok(())
        } else {
            Err(COMMAND_FAILED)
        }
    }
    fn mutate(&mut self, op: Operation) -> Result<(), i32> {
        let index = match op {
            Operation::MarkSuccessful(s)
            | Operation::SetActive(s)
            | Operation::SetUnbootable(s) => s as usize,
        };
        self.validate_slot(index as u32)?;
        let mut state = self.state()?;
        self.allow_write()?;
        match op {
            Operation::MarkSuccessful(_) => {
                state.slots[index].successful = true;
                match state.health_on_success {
                    HealthOnSuccess::ResetToOne => state.slots[index].tries = 1,
                    HealthOnSuccess::PreserveNonZero => {
                        state.slots[index].tries = state.slots[index].tries.max(1);
                        state.slots[index].priority = state.slots[index].priority.max(1);
                    }
                }
            }
            Operation::SetActive(_) => {
                for s in &mut state.slots {
                    if s.priority >= state.max_priority {
                        s.priority = state.max_priority - 1;
                    }
                }
                let s = &mut state.slots[index];
                s.priority = state.max_priority;
                s.tries = state.max_tries;
                if state.clear_successful_on_activate {
                    s.successful = false;
                }
                if index as u32 != self.current {
                    s.verity_corrupted = false;
                }
            }
            Operation::SetUnbootable(_) => {
                state.slots[index].successful = false;
                state.slots[index].tries = 0;
            }
        }
        self.backend.commit(&state, op).map_err(|_| COMMAND_FAILED)
    }
}
