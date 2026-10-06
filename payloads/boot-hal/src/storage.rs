//! efivarfs persistence; serialization is owned by the HAL's in-process Mutex.
use crate::{Merge, State};
use esu_platform::efivars;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

const VAB_OFFSET: u64 = 32 * 1024;

fn invalid(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

pub struct Storage {
    root: PathBuf,
    misc: PathBuf,
    slot_name: String,
    merge_name: String,
}

impl Storage {
    /// Does not open devices: unavailable storage must not gate Binder registration.
    pub fn open(root: &Path, misc: &Path, id: &str) -> io::Result<Self> {
        if id.is_empty()
            || id.len() > 59
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return Err(invalid("invalid catalogue id"));
        }
        Ok(Self {
            root: root.to_owned(),
            misc: misc.to_owned(),
            slot_name: format!("Slot-{id}"),
            merge_name: format!("MergeStatus-{id}"),
        })
    }

    /// The caller holds the HAL Mutex; EFI serializes individual variable operations.
    pub fn transaction<T>(&mut self, f: impl FnOnce(&mut Self) -> io::Result<T>) -> io::Result<T> {
        f(self)
    }

    pub fn state(&self) -> io::Result<State> {
        let (attributes, data) = efivars::read(&self.root, &self.slot_name)?
            .ok_or_else(|| invalid("Slot variable missing: provision first"))?;
        if attributes != 7 {
            return Err(invalid("Slot variable attributes"));
        }
        let state = State::decode(&data).map_err(invalid)?;
        if state.rom_number > efivars::MAX_ROM_NUMBER {
            return Err(invalid("invalid ROM number"));
        }
        Ok(state)
    }

    pub fn merge(&self, current: u8) -> io::Result<Merge> {
        let Some((attributes, data)) = efivars::read(&self.root, &self.merge_name)? else {
            return Ok(Merge {
                status: 0,
                source: current,
            });
        };
        if attributes != 7 {
            return Err(invalid("MergeStatus variable attributes"));
        }
        Merge::decode(&data).map_err(invalid)
    }

    pub fn save_state(&mut self, state: State) -> io::Result<()> {
        efivars::write(&self.root, &self.slot_name, 7, &state.encode())
    }

    pub fn save_merge(&mut self, merge: Merge) -> io::Result<()> {
        // Authority first. Failed mirroring returns failure, leaving the durable
        // authority intact; a retry/startup repairs only this booted ROM's mirror.
        efivars::write(&self.root, &self.merge_name, 7, &merge.encode())?;
        self.mirror(merge)
    }

    pub fn mirror(&self, merge: Merge) -> io::Result<()> {
        let misc = OpenOptions::new().read(true).write(true).open(&self.misc)?;
        let mut before = [0; 64];
        misc.read_exact_at(&mut before, VAB_OFFSET)?;
        let mut message = merge.message();
        // Preserve reserved bytes of a valid V2 VAB message, not stale foreign formats.
        if before[..5] == message[..5] {
            message[7..].copy_from_slice(&before[7..]);
        }
        if message == before {
            return Ok(());
        }
        misc.write_all_at(&message, VAB_OFFSET)?;
        misc.sync_all()?;
        misc.read_exact_at(&mut before, VAB_OFFSET)?;
        if before != message {
            return Err(invalid("misc VAB readback mismatch"));
        }
        Ok(())
    }
}
