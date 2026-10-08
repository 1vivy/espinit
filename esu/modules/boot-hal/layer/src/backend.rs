//! esu backend: project efivarfs `Slot-<id>`/`MergeStatus-<id>` records plus the
//! misc VAB mirror onto the shared `generic_bootctl_core::Backend` contract.
//!
//! Everything esu-specific lives here: the `BootedRom` catalogue identity, the
//! 24/8-byte project records, the 64-byte misc mirror at 32 KiB and the
//! retry-on-next-operation behaviour. Slot health, AIDL semantics and the
//! AIDL transport belong to the vendored shared crates.
use crate::wire::{Gbm1, Gbs1, VAB_OFFSET, invalid};
use esu_platform::efivars;
use generic_bootctl_core::{Backend, HealthOnSuccess, Merge, MergeStatus, Operation, Slot, State};
use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

const ATTRIBUTES: u32 = 7;

/// Variable names resolved from the booted catalogue id once and reused.
#[derive(Clone)]
struct Names {
    slot: String,
    merge: String,
}

/// The ROM number comes from the provisioned GBS1 record, never from the id; the
/// catalogue id only names the variables.
pub struct EsuBackend {
    root: PathBuf,
    misc: PathBuf,
    current: u8,
    names: Option<Names>,
    record: Option<Gbs1>,
    mirrored: bool,
}

impl EsuBackend {
    /// Never opens devices: unavailable storage must not gate Binder registration,
    /// and every storage-dependent operation reports failure on its own.
    pub fn open(root: &Path, misc: &Path, current: u8) -> Self {
        Self {
            root: root.to_owned(),
            misc: misc.to_owned(),
            current,
            names: None,
            record: None,
            mirrored: false,
        }
    }

    /// Best-effort startup repair: validate this ROM's record, then mirror its
    /// stored merge state into misc. The next state-dependent operation retries
    /// either step after a failure.
    pub fn reconcile(&mut self) -> io::Result<()> {
        match self.reconcile_inner() {
            Ok(()) => {
                self.mirrored = true;
                Ok(())
            }
            Err(error) => {
                eprintln!("boot-hal misc reconciliation: {error}");
                self.mirrored = false;
                Err(error)
            }
        }
    }

    fn reconcile_inner(&mut self) -> io::Result<()> {
        self.read_record()?;
        let merge = self.read_merge_raw()?;
        self.mirror(merge)
    }

    /// Resolve the booted ROM identity on demand so a failed or unmanaged
    /// identity is retried by every state-dependent operation.
    fn names(&mut self) -> io::Result<Names> {
        if let Some(names) = &self.names {
            return Ok(names.clone());
        }
        let id = efivars::booted_rom(&self.root)
            .map_err(|error| invalid(&format!("BootedRom: {error}")))?
            .ok_or_else(|| invalid("unmanaged booted ROM"))?;
        let names = Names {
            slot: format!("Slot-{id}"),
            merge: format!("MergeStatus-{id}"),
        };
        self.names = Some(names.clone());
        Ok(names)
    }

    fn read_record(&mut self) -> io::Result<Gbs1> {
        let names = self.names()?;
        let (attributes, data) = efivars::read(&self.root, &names.slot)?
            .ok_or_else(|| invalid("Slot variable missing: provision first"))?;
        if attributes != ATTRIBUTES {
            return Err(invalid("Slot variable attributes"));
        }
        let record = Gbs1::decode(&data)?;
        if record.rom_number > efivars::MAX_ROM_NUMBER {
            return Err(invalid("invalid ROM number"));
        }
        self.record = Some(record);
        Ok(record)
    }

    /// Missing `MergeStatus-<id>` is NONE sourced from the current slot.
    fn read_merge_raw(&mut self) -> io::Result<Gbm1> {
        let names = self.names()?;
        let Some((attributes, data)) = efivars::read(&self.root, &names.merge)? else {
            return Ok(Gbm1 {
                status: MergeStatus::None as u8,
                source: self.current,
            });
        };
        if attributes != ATTRIBUTES {
            return Err(invalid("MergeStatus variable attributes"));
        }
        Gbm1::decode(&data)
    }

    /// Ordered write, sync and readback of the 64-byte VAB message; no BCB or
    /// stock `bootloader_control` byte is touched.
    fn mirror(&self, merge: Gbm1) -> io::Result<()> {
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

    fn commit_inner(&mut self, state: &State, operation: Operation) -> io::Result<()> {
        let record = self
            .record
            .ok_or_else(|| invalid("no Slot record was read before commit"))?;
        let names = self.names()?;
        let slots: [Slot; 2] = state
            .slots
            .as_slice()
            .try_into()
            .map_err(|_| invalid("slot count changed"))?;
        let next = Gbs1 { slots, ..record }.with_operation(operation)?;
        self.record = Some(next);
        efivars::write(&self.root, &names.slot, ATTRIBUTES, &next.encode())
    }

    fn write_merge_inner(&mut self, merge: Merge) -> io::Result<()> {
        let record = Gbm1 {
            status: merge.status as u8,
            source: u8::try_from(merge.source).map_err(|_| invalid("invalid merge source"))?,
        };
        let names = self.names()?;
        // Authority first. A failed mirror returns failure while the durable
        // variable stays committed; the next operation repairs this ROM's mirror.
        efivars::write(&self.root, &names.merge, ATTRIBUTES, &record.encode())?;
        self.mirror(record)
    }

    fn failed<T>(&mut self, error: io::Error) -> io::Result<T> {
        eprintln!("boot-hal transaction: {error}");
        self.mirrored = false;
        Err(error)
    }

    /// Normalized view for the shared service. `max_tries` 7 and
    /// `PreserveNonZero` are the GBS1 health policy; bootability is the priority
    /// byte, which `wire` projects as `tries != 0`.
    fn project(record: Gbs1) -> State {
        State {
            slots: record.slots.to_vec(),
            max_priority: 15,
            max_tries: 7,
            clear_successful_on_activate: true,
            successful_requires_bootable: false,
            health_on_success: HealthOnSuccess::PreserveNonZero,
        }
    }
}

impl Backend for EsuBackend {
    /// GBS1 manages exactly two slots, and the count is storage-independent so
    /// slot-count/suffix queries stay available without efivarfs or identity.
    fn slot_count(&self) -> u32 {
        2
    }

    fn prepare(&mut self) -> io::Result<()> {
        if self.mirrored {
            return Ok(());
        }
        self.reconcile()
    }

    fn read_state(&mut self) -> io::Result<State> {
        match self.read_record() {
            Ok(record) => Ok(Self::project(record)),
            Err(error) => self.failed(error),
        }
    }

    fn commit(&mut self, state: &State, operation: Operation) -> io::Result<()> {
        match self.commit_inner(state, operation) {
            Ok(()) => Ok(()),
            Err(error) => self.failed(error),
        }
    }

    fn read_merge(&mut self) -> io::Result<Merge> {
        match self.read_merge_raw() {
            Ok(record) => MergeStatus::try_from(i32::from(record.status))
                .map(|status| Merge {
                    status,
                    source: u32::from(record.source),
                })
                .map_err(|_| invalid("invalid merge status")),
            Err(error) => self.failed(error),
        }
    }

    fn write_merge(&mut self, merge: Merge) -> io::Result<()> {
        match self.write_merge_inner(merge) {
            Ok(()) => Ok(()),
            Err(error) => self.failed(error),
        }
    }
}
