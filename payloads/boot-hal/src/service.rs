//! Host-testable frozen AIDL V1 operation dispatch; Binder only serializes replies.
use crate::{COMMAND_FAILED, Merge, slot_index, storage::Storage};
use esu_platform::efivars;
use std::ffi::CStr;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    Int(i32),
    Bool(bool),
    Text(&'static CStr),
    Void,
}

pub struct Hal {
    root: PathBuf,
    misc: PathBuf,
    storage: Option<Storage>,
    current: u8,
    mirrored: bool,
}

impl Hal {
    /// Construction never reads storage or ROM identity.
    pub fn new(root: &Path, misc: &Path, current: u8) -> Self {
        Self {
            root: root.to_owned(),
            misc: misc.to_owned(),
            storage: None,
            current,
            mirrored: false,
        }
    }

    fn storage(&mut self) -> Result<&mut Storage, i32> {
        if self.storage.is_none() {
            let id = efivars::booted_rom(&self.root)
                .map_err(|_| COMMAND_FAILED)?
                .ok_or(COMMAND_FAILED)?;
            self.storage =
                Some(Storage::open(&self.root, &self.misc, &id).map_err(|_| COMMAND_FAILED)?);
        }
        self.storage.as_mut().ok_or(COMMAND_FAILED)
    }

    /// Best-effort before registration; retried by state-dependent operations.
    pub fn reconcile(&mut self) -> Result<(), i32> {
        let current = self.current;
        self.storage()?
            .transaction(|s| {
                s.state()?;
                s.mirror(s.merge(current)?)
            })
            .map_err(|error| {
                eprintln!("boot-hal misc reconciliation: {error}");
                COMMAND_FAILED
            })?;
        self.mirrored = true;
        Ok(())
    }

    pub fn execute(&mut self, code: u32, input: i32) -> Result<Reply, i32> {
        if code == 16_777_214 {
            return Ok(Reply::Text(c"2400346954240a5de495a1debc81429dd012d7b7"));
        }
        if code == 16_777_215 {
            return Ok(Reply::Int(1));
        }
        let current = self.current;
        match code {
            2 => return Ok(Reply::Int(i32::from(current))),
            3 => return Ok(Reply::Int(2)),
            5 => {
                return Ok(Reply::Text(match input {
                    0 => c"_a",
                    1 => c"_b",
                    _ => c"",
                }));
            }
            6 | 7 | 9 | 10 => {
                slot_index(input)?;
            }
            11 if !(0..=4).contains(&input) => return Err(COMMAND_FAILED),
            _ => {}
        }
        if !self.mirrored {
            self.reconcile()?;
        }
        let result = self
            .storage()?
            .transaction(|storage| {
                match code {
                    4 => {
                        return Ok(Reply::Int(i32::from(
                            storage.merge(current)?.visible(current),
                        )));
                    }
                    11 => {
                        storage.save_merge(Merge {
                            status: input as u8,
                            source: current,
                        })?;
                        return Ok(Reply::Void);
                    }
                    _ => {}
                }
                let mut state = storage.state()?;
                let result = match code {
                    1 => return Ok(Reply::Int(i32::from(state.active()))),
                    6 => return Ok(Reply::Bool(state.slots[input as usize].bootable())),
                    7 => return Ok(Reply::Bool(state.slots[input as usize].successful)),
                    8 => state.mark_successful(current),
                    9 => state.set_active(input),
                    10 => state.set_unbootable(input),
                    _ => return Err(std::io::Error::other("unsupported transaction")),
                };
                result.map_err(|error| std::io::Error::other(error.to_string()))?;
                storage.save_state(state)?;
                Ok(Reply::Void)
            })
            .map_err(|error| {
                eprintln!("boot-hal transaction {code}: {error}");
                COMMAND_FAILED
            });
        if result.is_err() {
            self.mirrored = false;
        }
        result
    }
}
