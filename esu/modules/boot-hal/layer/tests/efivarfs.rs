//! Host behaviour tests for the esu records, the misc mirror, and the shared
//! generic-bootctl service that now drives them.
use esu_platform::efivars::{self, PROJECT_GUID};
use esu_platform::stage::StageState;
use generic_bootctl_core::{
    Backend, COMMAND_FAILED, INVALID_SLOT, Merge, MergeStatus, Reply, Service, Slot,
};
use gobbl_boot_hal::backend::EsuBackend;
use gobbl_boot_hal::txn::{Class, Env, Rom};
use gobbl_boot_hal::wire::{Gbm1, Gbs1, NO_PENDING, VAB_OFFSET};
use ota_core::Kmi;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

/// An Android boot image v4 with one uncompressed kernel block carrying a KMI
/// banner: the transaction reads the target's kernel identity out of it.
fn boot_image() -> Vec<u8> {
    let mut kernel = b"plain kernel bytes".to_vec();
    kernel.extend_from_slice(b"Linux version 6.12.23-android16-6-g1a2b3c4d (x) #1 SMP");
    kernel.extend_from_slice(&[0u8; 64]);

    let mut image = vec![0u8; 4096];
    image[..8].copy_from_slice(b"ANDROID!");
    image[8..12].copy_from_slice(&(kernel.len() as u32).to_le_bytes());
    image[20..24].copy_from_slice(&1584u32.to_le_bytes());
    image[40..44].copy_from_slice(&4u32.to_le_bytes());
    image.extend_from_slice(&kernel);
    image.resize(4096 + kernel.len().next_multiple_of(4096), 0);
    image
}

/// The transaction's platform for these record tests: the payload tree and the
/// storage classes always succeed and the stage record is kept in memory. The
/// transaction's own behaviour is covered by `txn.rs`'s tests.
#[derive(Default)]
struct TestEnv {
    stage: AtomicU8,
}

impl TestEnv {
    fn state(&self) -> StageState {
        match self.stage.load(Ordering::Relaxed) {
            1 => StageState::Staging,
            2 => StageState::Sealed,
            3 => StageState::Promote,
            _ => StageState::None,
        }
    }
}

impl Env for TestEnv {
    fn stage(&self, _id: &str) -> io::Result<StageState> {
        Ok(self.state())
    }

    fn write_stage(&self, _id: &str, state: StageState) -> io::Result<()> {
        self.stage.store(state as u8, Ordering::Relaxed);
        Ok(())
    }

    fn selected(&self, _id: &str) -> io::Result<u8> {
        Ok(0)
    }

    fn payload_file(&self, relative: &str) -> anyhow::Result<Vec<u8>> {
        Ok(match relative {
            "bin/esuinit" => b"\x7fELF esuinit".to_vec(),
            "build-id" => b"0123456789ab\n".to_vec(),
            other => anyhow::bail!("unexpected payload file {other}"),
        })
    }

    fn modules(&self, _kmi: &Kmi) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
        Ok(BTreeMap::from([(
            "lib/kernelesp.ko".to_owned(),
            b"\x7fELF module".to_vec(),
        )]))
    }

    fn write_stage_payload(&self, _id: &str, _bytes: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }

    fn remove_stage_payload(&self, _id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn commit_payload(&self, _id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn deny(&self, _reason: &str) {}

    fn detach(&self, job: Box<dyn FnOnce() + Send + 'static>) {
        job();
    }

    fn class(&self, _rom: &Rom) -> anyhow::Result<Box<dyn Class>> {
        Ok(Box::new(NoopClass))
    }
}

/// A class whose images are the synthetic boot image and whose staging,
/// teardown and promote are no-ops.
struct NoopClass;

impl Class for NoopClass {
    fn read_target_image(&self, _base: &str) -> anyhow::Result<Vec<u8>> {
        Ok(boot_image())
    }

    fn read_current_image(&self, _base: &str) -> anyhow::Result<Vec<u8>> {
        Ok(boot_image())
    }

    fn prepare(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn teardown(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn promote(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    dir: PathBuf,
    root: PathBuf,
    misc: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "boot-hal-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let root = dir.join("efivars");
        std::fs::create_dir(&root).unwrap();
        let misc = dir.join("misc");
        std::fs::write(&misc, vec![0xa5; 65536]).unwrap();
        Self { dir, root, misc }
    }
    fn variable(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}-{PROJECT_GUID}"))
    }
    fn managed(&self) {
        efivars::write(&self.root, "BootedRom", 7, b"stock\0").unwrap();
        efivars::write(
            &self.root,
            "Slot-stock",
            7,
            &Gbs1::initial(1, 1).unwrap().encode(),
        )
        .unwrap();
    }
    fn backend(&self, current: u8) -> EsuBackend {
        EsuBackend::open(
            &self.root,
            &self.misc,
            current,
            Arc::new(TestEnv::default()),
        )
    }
    /// Production constructs the same service with an always-writable gate.
    fn service(&self, current: u8) -> Service {
        Service::new(
            Box::new(self.backend(current)),
            u32::from(current),
            Box::new(|| true),
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).unwrap();
    }
}
fn stored(f: &Fixture, name: &str) -> Vec<u8> {
    efivars::read(&f.root, name).unwrap().unwrap().1
}
fn vab() -> usize {
    usize::try_from(VAB_OFFSET).unwrap()
}

#[test]
fn missing_merge_defaults_but_missing_slot_cannot_invent_rom_number() {
    let f = Fixture::new();
    // The variable names come from BootedRom, so only identity precedes them.
    efivars::write(&f.root, "BootedRom", 7, b"stock\0").unwrap();
    let mut backend = f.backend(1);
    assert_eq!(
        backend.read_merge().unwrap(),
        Merge {
            status: MergeStatus::None,
            source: 1
        }
    );
    assert!(backend.read_state().is_err());
    // Construction is infallible and touches nothing; every storage-dependent
    // operation reports failure instead.
    let mut absent = EsuBackend::open(
        Path::new("/nonexistent/efivars"),
        Path::new("/nonexistent/misc"),
        1,
        Arc::new(TestEnv::default()),
    );
    assert!(absent.read_state().is_err());
    assert!(absent.prepare().is_err());
}

#[test]
fn unavailable_or_unmanaged_identity_does_not_gate_hal_and_is_retried() {
    let f = Fixture::new();
    let mut backend = f.backend(1);
    for identity in [
        None,
        Some(b"direct\0".as_slice()),
        Some(b"malformed".as_slice()),
    ] {
        if let Some(data) = identity {
            efivars::write(&f.root, "BootedRom", 7, data).unwrap();
        }
        // The same instance retries identity on every state-dependent call.
        assert!(backend.reconcile().is_err());
        assert!(backend.read_state().is_err());
        let mut hal = f.service(1);
        assert_eq!(hal.execute(2, 0), Ok(Reply::Int(1)));
        assert_eq!(hal.execute(3, 0), Ok(Reply::Int(2)));
        assert_eq!(hal.execute(5, 0), Ok(Reply::Text(c"_a")));
        assert_eq!(
            hal.execute(16_777_214, 0),
            Ok(Reply::Text(c"2400346954240a5de495a1debc81429dd012d7b7"))
        );
        assert_eq!(hal.execute(16_777_215, 0), Ok(Reply::Int(1)));
        assert_eq!(hal.execute(1, 0), Err(COMMAND_FAILED));
    }
    f.managed();
    assert!(backend.reconcile().is_ok());
    assert_eq!(backend.read_state().unwrap().slots.len(), 2);
    let mut hal = f.service(1);
    assert_eq!(hal.execute(1, 0), Ok(Reply::Int(1)));
}

#[test]
fn persistent_read_and_write_errors_are_aidl_command_failures() {
    let f = Fixture::new();
    f.managed();
    f.backend(1).reconcile().unwrap();
    let mut hal = f.service(1);
    std::fs::remove_file(f.variable("Slot-stock")).unwrap();
    for _ in 0..2 {
        assert_eq!(hal.execute(1, 0), Err(COMMAND_FAILED));
    }
    std::fs::create_dir(f.variable("Slot-stock")).unwrap();
    assert_eq!(hal.execute(8, 0), Err(COMMAND_FAILED));
    std::fs::remove_dir(f.variable("Slot-stock")).unwrap();
    f.managed();
    f.backend(1).reconcile().unwrap();
    std::fs::create_dir(f.variable("MergeStatus-stock")).unwrap();
    for _ in 0..2 {
        assert_eq!(hal.execute(11, 3), Err(COMMAND_FAILED));
    }
    assert_eq!(hal.execute(2, 0), Ok(Reply::Int(1)));
}

#[test]
fn writes_are_byte_exact_and_preserve_other_roms_and_misc_regions() {
    let f = Fixture::new();
    f.managed();
    let other = Gbs1::initial(2, 0).unwrap().encode();
    efivars::write(&f.root, "Slot-other", 7, &other).unwrap();
    let mut before = std::fs::read(&f.misc).unwrap();
    let old = Gbm1 {
        status: 2,
        source: 1,
    }
    .message();
    before[vab()..vab() + 7].copy_from_slice(&old[..7]);
    std::fs::write(&f.misc, &before).unwrap();
    let mut backend = f.backend(1);
    backend.reconcile().unwrap();
    let mut hal = Service::new(Box::new(backend), 1, Box::new(|| true));
    assert_eq!(hal.execute(9, 0), Ok(Reply::Void));
    assert_eq!(hal.execute(11, 3), Ok(Reply::Void));
    // Byte-exact records: the esu wire format is the on-disk contract.
    let expected = Gbs1 {
        rom_number: 1,
        selected: 1,
        pending: 0,
        slots: [
            Slot {
                priority: 15,
                tries: 7,
                successful: false,
                verity_corrupted: false,
            },
            Slot {
                priority: 14,
                tries: 7,
                successful: true,
                verity_corrupted: false,
            },
        ],
    };
    let wire = [7u32.to_le_bytes().as_slice(), expected.encode().as_slice()].concat();
    assert_eq!(std::fs::read(f.variable("Slot-stock")).unwrap(), wire);
    assert_eq!(
        efivars::read(&f.root, "Slot-other").unwrap(),
        Some((7, other.to_vec()))
    );
    let merge = Gbm1 {
        status: 3,
        source: 1,
    };
    assert_eq!(
        std::fs::read(f.variable("MergeStatus-stock")).unwrap(),
        [7u32.to_le_bytes().as_slice(), merge.encode().as_slice()].concat()
    );
    let after = std::fs::read(&f.misc).unwrap();
    assert_eq!(&after[..vab()], &before[..vab()]);
    assert_eq!(&after[vab() + 7..], &before[vab() + 7..]);
    assert_eq!(&after[vab()..vab() + 7], &merge.message()[..7]);
}

#[test]
fn failed_misc_mirror_leaves_authority_committed_and_reconciliation_retries() {
    let f = Fixture::new();
    f.managed();
    std::fs::remove_file(&f.misc).unwrap();
    let mut hal = f.service(1);
    assert!(f.backend(1).reconcile().is_err());
    assert_eq!(hal.execute(2, 0), Ok(Reply::Int(1)));
    let merge = Merge {
        status: MergeStatus::Merging,
        source: 1,
    };
    assert!(f.backend(1).write_merge(merge).is_err());
    assert_eq!(f.backend(1).read_merge().unwrap(), merge);
    std::fs::write(&f.misc, vec![0xa5; 65536]).unwrap();
    assert_eq!(hal.execute(4, 0), Ok(Reply::Int(3)));
    let after = std::fs::read(&f.misc).unwrap();
    assert_eq!(
        &after[vab()..vab() + 64],
        &Gbm1 {
            status: 3,
            source: 1
        }
        .message()
    );
}

#[test]
fn rom_one_records_a_pending_switch_and_rom_two_selects_immediately() {
    let f = Fixture::new();
    f.managed();
    let mut hal = f.service(1);
    assert_eq!(hal.execute(9, 0), Ok(Reply::Void));
    let record = Gbs1::decode(&stored(&f, "Slot-stock")).unwrap();
    assert_eq!(
        (record.rom_number, record.selected, record.pending),
        (1, 1, 0)
    );
    assert_eq!(record.booted_slot(), 0);
    assert_eq!(hal.execute(1, 0), Ok(Reply::Int(0)));
    // Requesting the confirmed slot cancels the pending switch.
    assert_eq!(hal.execute(9, 1), Ok(Reply::Void));
    let record = Gbs1::decode(&stored(&f, "Slot-stock")).unwrap();
    assert_eq!((record.selected, record.pending), (1, NO_PENDING));
    assert_eq!(hal.execute(1, 0), Ok(Reply::Int(1)));
    // ROM >= 2 selects immediately and never carries a pending request.
    efivars::write(
        &f.root,
        "Slot-stock",
        7,
        &Gbs1::initial(2, 0).unwrap().encode(),
    )
    .unwrap();
    let mut hal = f.service(1);
    assert_eq!(hal.execute(9, 1), Ok(Reply::Void));
    let record = Gbs1::decode(&stored(&f, "Slot-stock")).unwrap();
    assert_eq!(
        (record.rom_number, record.selected, record.pending),
        (2, 1, NO_PENDING)
    );
    assert_eq!(hal.execute(1, 0), Ok(Reply::Int(1)));
    assert_eq!(hal.execute(6, 1), Ok(Reply::Bool(true)));
}

#[test]
fn success_and_unbootable_keep_the_gbs1_health_policy() {
    let f = Fixture::new();
    f.managed();
    let mut hal = f.service(1);
    // markBootSuccessful keeps the existing nonzero try count and never confirms
    // or erases a pending firmware request.
    assert_eq!(hal.execute(8, 0), Ok(Reply::Void));
    let record = Gbs1::decode(&stored(&f, "Slot-stock")).unwrap();
    assert_eq!(
        record.slots[1],
        Slot {
            priority: 15,
            tries: 7,
            successful: true,
            verity_corrupted: false
        }
    );
    assert_eq!(record.pending, NO_PENDING);
    assert_eq!(hal.execute(7, 1), Ok(Reply::Bool(true)));
    // A fully zeroed slot is raised to one try, one priority and the success bit.
    efivars::write(
        &f.root,
        "Slot-stock",
        7,
        &Gbs1 {
            slots: [Slot::default(); 2],
            ..Gbs1::initial(1, 1).unwrap()
        }
        .encode(),
    )
    .unwrap();
    let mut hal = f.service(1);
    assert_eq!(hal.execute(6, 1), Ok(Reply::Bool(false)));
    assert_eq!(hal.execute(8, 0), Ok(Reply::Void));
    let record = Gbs1::decode(&stored(&f, "Slot-stock")).unwrap();
    assert_eq!(
        record.slots[1],
        Slot {
            priority: 1,
            tries: 1,
            successful: true,
            verity_corrupted: false
        }
    );
    assert_eq!(hal.execute(7, 1), Ok(Reply::Bool(true)));
    // setSlotAsUnbootable zeroes priority, tries and success, clears the matching
    // pending request and never selects an alternate slot.
    assert_eq!(hal.execute(9, 0), Ok(Reply::Void));
    let requested = Gbs1::decode(&stored(&f, "Slot-stock")).unwrap();
    assert_eq!(requested.pending, 0);
    assert_eq!(hal.execute(10, 0), Ok(Reply::Void));
    let record = Gbs1::decode(&stored(&f, "Slot-stock")).unwrap();
    assert_eq!(record.slots[0], Slot::default());
    assert_eq!((record.selected, record.pending), (1, NO_PENDING));
    assert_eq!(hal.execute(1, 0), Ok(Reply::Int(1)));
    assert_eq!(hal.execute(6, 0), Ok(Reply::Bool(false)));
}

#[test]
fn slot_queries_stay_available_and_invalid_slots_fail_first() {
    let f = Fixture::new();
    let mut hal = f.service(1);
    // No identity, no Slot record and no mirror reachable: these answers remain.
    assert_eq!(hal.execute(2, 0), Ok(Reply::Int(1)));
    assert_eq!(hal.execute(3, 0), Ok(Reply::Int(2)));
    assert_eq!(hal.execute(5, 0), Ok(Reply::Text(c"_a")));
    assert_eq!(hal.execute(5, 1), Ok(Reply::Text(c"_b")));
    assert_eq!(hal.execute(5, 2), Ok(Reply::Text(c"")));
    assert_eq!(hal.execute(5, -1), Ok(Reply::Text(c"")));
    // Index validation precedes storage access, exactly as AOSP does.
    for code in [6, 7, 9, 10] {
        for slot in [2, -1, i32::MAX] {
            assert_eq!(hal.execute(code, slot), Err(INVALID_SLOT));
        }
    }
    assert_eq!(hal.execute(8, 0), Err(COMMAND_FAILED));
    f.managed();
    std::fs::remove_file(f.variable("Slot-stock")).unwrap();
    std::fs::create_dir(f.variable("Slot-stock")).unwrap();
    let mut hal = f.service(1);
    assert_eq!(hal.execute(6, 5), Err(INVALID_SLOT));
    assert_eq!(hal.execute(6, 0), Err(COMMAND_FAILED));
}
