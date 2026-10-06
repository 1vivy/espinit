use esu_platform::efivars::{self, PROJECT_GUID};
use gblbds_boot_hal::{
    COMMAND_FAILED, Merge, State,
    service::{Hal, Reply},
    storage::Storage,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
            &State::initial(1, 1).unwrap().encode(),
        )
        .unwrap();
    }
    fn storage(&self) -> Storage {
        Storage::open(&self.root, &self.misc, "stock").unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).unwrap();
    }
}

#[test]
fn missing_merge_defaults_but_missing_slot_cannot_invent_rom_number() {
    let f = Fixture::new();
    let s = f.storage();
    assert_eq!(
        s.merge(1).unwrap(),
        Merge {
            status: 0,
            source: 1
        }
    );
    assert!(s.state().is_err());
    // Construction is safe even when neither path exists.
    Storage::open(
        Path::new("/nonexistent/efivars"),
        Path::new("/nonexistent/misc"),
        "stock",
    )
    .unwrap();
}

#[test]
fn unavailable_or_unmanaged_identity_does_not_gate_hal_and_is_retried() {
    let f = Fixture::new();
    let mut hal = Hal::new(&f.root, &f.misc, 1);
    for identity in [
        None,
        Some(b"direct\0".as_slice()),
        Some(b"malformed".as_slice()),
    ] {
        if let Some(data) = identity {
            efivars::write(&f.root, "BootedRom", 7, data).unwrap();
        }
        assert_eq!(hal.reconcile(), Err(COMMAND_FAILED));
        assert_eq!(hal.execute(2, 0), Ok(Reply::Int(1)));
        assert_eq!(hal.execute(3, 0), Ok(Reply::Int(2)));
        assert_eq!(hal.execute(5, 0), Ok(Reply::Text(c"_a")));
        assert_eq!(hal.execute(1, 0), Err(COMMAND_FAILED));
    }
    f.managed();
    assert_eq!(hal.execute(1, 0), Ok(Reply::Int(1)));
}

#[test]
fn persistent_read_and_write_errors_are_aidl_command_failures() {
    let f = Fixture::new();
    f.managed();
    let mut hal = Hal::new(&f.root, &f.misc, 1);
    hal.reconcile().unwrap();
    std::fs::remove_file(f.variable("Slot-stock")).unwrap();
    for _ in 0..2 {
        assert_eq!(hal.execute(1, 0), Err(COMMAND_FAILED));
    }
    std::fs::create_dir(f.variable("Slot-stock")).unwrap();
    assert_eq!(hal.execute(8, 0), Err(COMMAND_FAILED));
    std::fs::remove_dir(f.variable("Slot-stock")).unwrap();
    f.managed();
    hal.reconcile().unwrap();
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
    let other = State::initial(2, 0).unwrap().encode();
    efivars::write(&f.root, "Slot-other", 7, &other).unwrap();
    let mut before = std::fs::read(&f.misc).unwrap();
    let old = Merge {
        status: 2,
        source: 1,
    }
    .message();
    before[32768..32775].copy_from_slice(&old[..7]);
    std::fs::write(&f.misc, &before).unwrap();
    let mut hal = Hal::new(&f.root, &f.misc, 1);
    hal.reconcile().unwrap();
    assert_eq!(hal.execute(9, 0), Ok(Reply::Void));
    assert_eq!(hal.execute(11, 3), Ok(Reply::Void));
    let mut expected = State::initial(1, 1).unwrap();
    expected.set_active(0).unwrap();
    let wire = [7u32.to_le_bytes().as_slice(), expected.encode().as_slice()].concat();
    assert_eq!(std::fs::read(f.variable("Slot-stock")).unwrap(), wire);
    assert_eq!(
        efivars::read(&f.root, "Slot-other").unwrap(),
        Some((7, other.to_vec()))
    );
    let merge = Merge {
        status: 3,
        source: 1,
    };
    assert_eq!(
        std::fs::read(f.variable("MergeStatus-stock")).unwrap(),
        [7u32.to_le_bytes().as_slice(), merge.encode().as_slice()].concat()
    );
    let after = std::fs::read(&f.misc).unwrap();
    assert_eq!(&after[..32768], &before[..32768]);
    assert_eq!(&after[32775..], &before[32775..]);
    assert_eq!(&after[32768..32775], &merge.message()[..7]);
}

#[test]
fn failed_misc_mirror_leaves_authority_committed_and_reconciliation_retries() {
    let f = Fixture::new();
    f.managed();
    std::fs::remove_file(&f.misc).unwrap();
    let mut hal = Hal::new(&f.root, &f.misc, 1);
    assert_eq!(hal.reconcile(), Err(COMMAND_FAILED));
    assert_eq!(hal.execute(2, 0), Ok(Reply::Int(1)));
    let merge = Merge {
        status: 3,
        source: 1,
    };
    assert!(f.storage().save_merge(merge).is_err());
    assert_eq!(f.storage().merge(1).unwrap(), merge);
    std::fs::write(&f.misc, vec![0xa5; 65536]).unwrap();
    assert_eq!(hal.execute(4, 0), Ok(Reply::Int(3)));
    let after = std::fs::read(&f.misc).unwrap();
    assert_eq!(&after[32768..32832], &merge.message());
}
