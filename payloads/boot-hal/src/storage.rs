//! Linux persistence adapter: ordered edk2 append, never whole-store rewrite/reclaim.
use crate::{Merge, State, VENDOR_GUID};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use varstore::{Layout, Store, StoreMut};

const STORE_BYTES: usize = 1024 * 1024;
const VAB_OFFSET: u64 = 32 * 1024;

unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

fn invalid(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

pub struct Storage {
    vars: File,
    misc: File,
    slot_name: Vec<u16>,
    merge_name: Vec<u16>,
    image: Vec<u8>,
    updated: Vec<u8>,
}

impl Storage {
    pub fn open(vars: &Path, misc: &Path, id: &str) -> io::Result<Self> {
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return Err(invalid("invalid catalogue id"));
        }
        Ok(Self {
            vars: OpenOptions::new().read(true).write(true).open(vars)?,
            misc: OpenOptions::new().read(true).write(true).open(misc)?,
            slot_name: format!("Slot-{id}").encode_utf16().collect(),
            merge_name: format!("MergeStatus-{id}").encode_utf16().collect(),
            image: vec![0; STORE_BYTES],
            updated: vec![0; STORE_BYTES],
        })
    }

    /// Serializes cooperative Linux writers; firmware cannot execute concurrently.
    pub fn transaction<T>(&mut self, f: impl FnOnce(&mut Self) -> io::Result<T>) -> io::Result<T> {
        // SAFETY: vars owns a valid descriptor; flock takes no pointers.
        if unsafe { flock(self.vars.as_raw_fd(), 2) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let result = self
            .vars
            .read_exact_at(&mut self.image, 0)
            .and_then(|()| Store::parse(&self.image).map(|_| ()).map_err(invalid))
            .and_then(|()| f(self));
        // SAFETY: the file remains open until after the lock is released.
        let unlocked = unsafe { flock(self.vars.as_raw_fd(), 8) };
        if unlocked != 0 {
            return Err(io::Error::last_os_error());
        }
        result
    }

    pub fn state(&self) -> io::Result<State> {
        let store = Store::parse(&self.image).map_err(invalid)?;
        let value = store
            .get(&self.slot_name, &VENDOR_GUID)
            .ok_or_else(|| invalid("Slot variable missing: provision first"))?;
        if value.attributes != 7 {
            return Err(invalid("Slot variable attributes"));
        }
        State::decode(value.data).map_err(invalid)
    }

    pub fn merge(&self) -> io::Result<Merge> {
        let store = Store::parse(&self.image).map_err(invalid)?;
        let value = store
            .get(&self.merge_name, &VENDOR_GUID)
            .ok_or_else(|| invalid("MergeStatus variable missing: provision first"))?;
        if value.attributes != 7 {
            return Err(invalid("MergeStatus variable attributes"));
        }
        Merge::decode(value.data).map_err(invalid)
    }

    pub fn save_state(&mut self, state: State) -> io::Result<()> {
        self.save(false, &state.encode())
    }

    pub fn save_merge(&mut self, merge: Merge) -> io::Result<()> {
        // Authority first. Failed mirroring returns failure, leaving the durable
        // authority intact; a retry/startup repairs only this booted ROM's mirror.
        self.save(true, &merge.encode())?;
        self.mirror(merge)
    }

    pub fn mirror(&self, merge: Merge) -> io::Result<()> {
        let mut before = [0; 64];
        self.misc.read_exact_at(&mut before, VAB_OFFSET)?;
        let mut message = merge.message();
        // Preserve reserved bytes of a valid V2 VAB message, not stale foreign formats.
        if before[..5] == message[..5] {
            message[7..].copy_from_slice(&before[7..]);
        }
        if message == before {
            return Ok(());
        }
        self.misc.write_all_at(&message, VAB_OFFSET)?;
        self.misc.sync_all()?;
        self.misc.read_exact_at(&mut before, VAB_OFFSET)?;
        if before != message {
            return Err(invalid("misc VAB readback mismatch"));
        }
        Ok(())
    }

    fn save(&mut self, merge: bool, data: &[u8]) -> io::Result<()> {
        let name = if merge {
            &self.merge_name
        } else {
            &self.slot_name
        };
        let original = Store::parse(&self.image).map_err(invalid)?;
        if original
            .get(name, &VENDOR_GUID)
            .is_some_and(|v| v.attributes == 7 && v.data == data)
        {
            return Ok(());
        }
        self.updated.copy_from_slice(&self.image);
        let mut writer = StoreMut::parse(&mut self.updated).map_err(invalid)?;
        writer.set(name, &VENDOR_GUID, 7, data).map_err(invalid)?;
        let new_store = writer.as_store();
        let new_value = new_store
            .get(name, &VENDOR_GUID)
            .ok_or_else(|| invalid("missing appended value"))?;
        // The shared parser supplies slices inside the validated image. These
        // two edk2 header sizes are used only to delimit persistence phases;
        // record encoding and validation remain entirely in crates/varstore.
        let header_len = match new_store.layout() {
            Layout::Normal => 32,
            Layout::Authenticated => 60,
        };
        let data_start = new_value.data.as_ptr() as usize - writer.image().as_ptr() as usize;
        let start = new_value.name.as_bytes().as_ptr() as usize
            - writer.image().as_ptr() as usize
            - header_len;
        let end = (data_start + data.len() + 3) & !3;
        if self.image[start..end].iter().any(|b| *b != 0xff) {
            return Err(invalid("append is not erased"));
        }
        // Validate that all pre-append edits are edk2 old-record state changes.
        for (offset, (old, new)) in self.image[..start]
            .iter()
            .zip(&self.updated[..start])
            .enumerate()
        {
            if old != new && !matches!((*old, *new), (0x3f, 0x3c) | (0x3e, 0x3c)) {
                return Err(invalid(format!("unexpected pre-append change at {offset}")));
            }
        }
        for (offset, (old, new)) in self.image[..start]
            .iter()
            .zip(&self.updated[..start])
            .enumerate()
        {
            if old != new {
                self.vars.write_all_at(&[old & 0xfe], offset as u64)?;
            }
        }
        self.vars.sync_all()?;
        let mut header = [0xff; 60];
        header[..header_len].copy_from_slice(&self.updated[start..start + header_len]);
        header[2] = 0xff;
        self.vars
            .write_all_at(&header[..header_len], start as u64)?;
        self.vars.sync_all()?;
        self.vars.write_all_at(&[0x7f], (start + 2) as u64)?;
        self.vars.sync_all()?;
        self.vars.write_all_at(
            &self.updated[start + header_len..end],
            (start + header_len) as u64,
        )?;
        self.vars.sync_all()?;
        self.vars.write_all_at(&[0x3f], (start + 2) as u64)?;
        self.vars.sync_all()?;
        for (offset, (old, new)) in self.image[..start]
            .iter()
            .zip(&self.updated[..start])
            .enumerate()
        {
            if old != new {
                self.vars.write_all_at(&[*new], offset as u64)?;
            }
        }
        self.vars.sync_all()?;
        self.vars.read_exact_at(&mut self.image, 0)?;
        if self.image != self.updated {
            return Err(invalid("bdsvars readback mismatch"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn durable_updates_preserve_other_roms_bcb_and_bootloader_control() {
        let dir = std::env::temp_dir().join(format!(
            "gblbds-hal-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let vars = dir.join("vars");
        let misc = dir.join("misc");
        let mut image = vec![0xff; STORE_BYTES];
        let mut store = StoreMut::format(&mut image, Layout::Authenticated, 4096).unwrap();
        for (name, data) in [
            (
                "Slot-stock",
                State::initial(1, 1).unwrap().encode().to_vec(),
            ),
            (
                "Slot-other",
                State::initial(2, 0).unwrap().encode().to_vec(),
            ),
            (
                "MergeStatus-stock",
                Merge {
                    status: 0,
                    source: 1,
                }
                .encode()
                .to_vec(),
            ),
        ] {
            store
                .set(
                    &name.encode_utf16().collect::<Vec<_>>(),
                    &VENDOR_GUID,
                    7,
                    &data,
                )
                .unwrap();
        }
        std::fs::write(&vars, &image).unwrap();
        let original_misc = vec![0xa5; 65536];
        std::fs::write(&misc, &original_misc).unwrap();
        {
            let mut storage = Storage::open(&vars, &misc, "stock").unwrap();
            storage
                .transaction(|s| {
                    let mut state = s.state()?;
                    state.set_active(0).unwrap();
                    s.save_state(state)?;
                    s.save_merge(Merge {
                        status: 3,
                        source: 1,
                    })
                })
                .unwrap();
        }
        let mut reopened = Storage::open(&vars, &misc, "stock").unwrap();
        reopened
            .transaction(|s| {
                assert_eq!(s.state()?.pending, 0);
                assert_eq!(s.state()?.selected, 1);
                assert_eq!(s.merge()?.status, 3);
                let other = Store::parse(&s.image)
                    .unwrap()
                    .get(
                        &"Slot-other".encode_utf16().collect::<Vec<_>>(),
                        &VENDOR_GUID,
                    )
                    .unwrap();
                assert_eq!(State::decode(other.data), State::initial(2, 0));
                Ok(())
            })
            .unwrap();
        let after = std::fs::read(&misc).unwrap();
        assert_eq!(&after[..32768], &original_misc[..32768]);
        assert_eq!(&after[32832..], &original_misc[32832..]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
