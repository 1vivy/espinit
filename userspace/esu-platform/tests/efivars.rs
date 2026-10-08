// SPDX-License-Identifier: GPL-3.0-only
//! The host contract for the shared efivarfs API: a directory laid out like an
//! efivarfs mount, four attribute bytes then the variable payload.

use esu_platform::efivars::{self, Error, MAX_ROM_NUMBER, PROJECT_GUID};
use esu_platform::stage::StageState;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Vars(PathBuf);

impl Vars {
    fn new() -> Self {
        let root = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "esu-efivars-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn root(&self) -> &Path {
        &self.0
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(format!("{name}-{PROJECT_GUID}"))
    }

    fn raw(&self, name: &str, bytes: &[u8]) {
        fs::write(self.path(name), bytes).unwrap();
    }

    fn put(&self, name: &str, attributes: u32, payload: &[u8]) {
        let mut bytes = attributes.to_le_bytes().to_vec();
        bytes.extend_from_slice(payload);
        self.raw(name, &bytes);
    }

    fn slot(&self, id: &str, attributes: u32, magic: &[u8], number: u32) {
        let mut payload = magic.to_vec();
        payload.extend_from_slice(&number.to_le_bytes());
        self.put(&format!("Slot-{id}"), attributes, &payload);
    }

    /// A complete 24-byte GBS1 record, byte 8 being the selected slot.
    fn gbs1(&self, id: &str, attributes: u32, selected: u8) {
        let mut payload = b"GBS1".to_vec();
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(selected);
        payload.push(0xff);
        payload.extend_from_slice(&[0; 14]);
        assert_eq!(payload.len(), 24);
        self.put(&format!("Slot-{id}"), attributes, &payload);
    }

    fn bytes(&self, name: &str) -> Vec<u8> {
        fs::read(self.path(name)).unwrap()
    }
}

impl Drop for Vars {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn missing_variables_are_absent_and_unmanaged() {
    let vars = Vars::new();
    assert!(efivars::read(vars.root(), "BootedRom").unwrap().is_none());
    assert!(efivars::booted_rom(vars.root()).unwrap().is_none());
    assert!(matches!(
        efivars::rom_number(vars.root(), "rom1"),
        Err(Error::RomRecordMissing)
    ));
}

#[test]
fn direct_dispatch_is_unmanaged() {
    let vars = Vars::new();
    vars.put("BootedRom", 7, b"direct\0");
    assert!(efivars::booted_rom(vars.root()).unwrap().is_none());
}

#[test]
fn managed_dispatch_resolves_the_rom_id() {
    let vars = Vars::new();
    vars.put("BootedRom", 7, b"rom1\0");
    assert_eq!(
        efivars::booted_rom(vars.root()).unwrap().as_deref(),
        Some("rom1")
    );
}

#[test]
fn booted_rom_decodes_utf8_up_to_the_first_nul() {
    let vars = Vars::new();
    vars.put("BootedRom", 7, b"android-a\0trailing");
    assert_eq!(
        efivars::booted_rom(vars.root()).unwrap().as_deref(),
        Some("android-a")
    );
}

#[test]
fn malformed_booted_rom_is_rejected() {
    let vars = Vars::new();
    let cases: &[(&[u8], u32)] = &[
        (b"rom1", 7),
        (b"\0", 7),
        (b"..\0", 7),
        (b"rom/1\0", 7),
        (b"rom 1\0", 7),
        (b"\xff\xfe\0", 7),
        (b"rom1\0", 0),
        (b"rom1\0", 6),
        (b"rom1\0", 0x8000_0007),
    ];
    for (payload, attributes) in cases {
        vars.put("BootedRom", *attributes, payload);
        assert!(
            matches!(
                efivars::booted_rom(vars.root()),
                Err(Error::BootedRomInvalid)
            ),
            "{payload:?} with attributes {attributes}"
        );
    }

    let id = "a".repeat(60);
    vars.put("BootedRom", 7, format!("{id}\0").as_bytes());
    assert!(matches!(
        efivars::booted_rom(vars.root()),
        Err(Error::BootedRomInvalid)
    ));

    vars.put("BootedRom", 7, &[7, 0, 0]);
    assert!(matches!(
        efivars::booted_rom(vars.root()),
        Err(Error::BootedRomInvalid)
    ));
}

#[test]
fn truncated_attribute_header_is_invalid_data() {
    let vars = Vars::new();
    vars.raw("BootedRom", &[7, 0, 0]);
    let error = efivars::read(vars.root(), "BootedRom").unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn rom_number_reads_the_gbs1_record() {
    let vars = Vars::new();
    for number in [1, 3, MAX_ROM_NUMBER] {
        vars.slot("rom1", 7, b"GBS1", number);
        assert_eq!(efivars::rom_number(vars.root(), "rom1").unwrap(), number);
    }

    // On disk: four attribute bytes, then `GBS1`, so the number occupies 8..12.
    vars.slot("rom1", 7, b"GBS1", 2);
    let bytes = vars.bytes("Slot-rom1");
    assert_eq!(bytes.len(), 12);
    assert_eq!(&bytes[..4], &7u32.to_le_bytes());
    assert_eq!(&bytes[4..8], b"GBS1");
    assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 2);
}

#[test]
fn invalid_slot_records_are_rejected() {
    let vars = Vars::new();
    vars.raw("Slot-rom1", b"\x07\0\0\0GBS1\0\0\0");
    assert!(matches!(
        efivars::rom_number(vars.root(), "rom1"),
        Err(Error::RomNumberInvalid)
    ));

    let cases: &[(&[u8], u32)] = &[
        (b"GBS1", 7),
        (b"GBS0\x01\0\0\0", 7),
        (b"gbs1\x01\0\0\0", 7),
        (b"GBS1\0\0\0\0", 7),
        (b"GBS1\x06\0\0\0", 7),
        (b"GBS1\xff\xff\xff\xff", 7),
        (b"GBS1\x01\0\0\0", 6),
    ];
    for (payload, attributes) in cases {
        vars.put("Slot-rom1", *attributes, payload);
        assert!(
            matches!(
                efivars::rom_number(vars.root(), "rom1"),
                Err(Error::RomNumberInvalid)
            ),
            "{payload:?} with attributes {attributes}"
        );
    }
}

#[test]
fn missing_managed_slot_differs_from_an_invalid_one() {
    let vars = Vars::new();
    vars.slot("rom2", 7, b"GBS1", 9);
    assert!(matches!(
        efivars::rom_number(vars.root(), "rom2"),
        Err(Error::RomNumberInvalid)
    ));
    assert!(matches!(
        efivars::rom_number(vars.root(), "rom3"),
        Err(Error::RomRecordMissing)
    ));
}

#[test]
fn write_emits_attributes_then_the_payload_once() {
    let vars = Vars::new();
    let data = [b'r', b'o', b'm', b'1', 0];
    efivars::write(vars.root(), "BootedRom", 7, &data).unwrap();
    assert_eq!(vars.bytes("BootedRom"), b"\x07\0\0\0rom1\0");
    assert_eq!(
        efivars::read(vars.root(), "BootedRom").unwrap(),
        Some((7, data.to_vec()))
    );

    efivars::write(vars.root(), "BootedRom", 7, b"rom2\0").unwrap();
    assert_eq!(vars.bytes("BootedRom"), b"\x07\0\0\0rom2\0");
}

#[test]
fn unsafe_names_and_ids_never_reach_the_filesystem() {
    let vars = Vars::new();
    for name in ["", ".", "..", "a/b", "Slot-../../etc/passwd", "BootedRom\0"] {
        assert!(
            matches!(
                efivars::read(vars.root(), name),
                Err(error) if error.kind() == io::ErrorKind::InvalidInput
            ),
            "{name:?}"
        );
        assert!(matches!(
            efivars::write(vars.root(), name, 7, b"x"),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput
        ));
    }
    for id in ["", ".", "..", "rom1/x", "rom1\0"] {
        assert!(
            matches!(
                efivars::rom_number(vars.root(), id),
                Err(Error::RomNumberInvalid)
            ),
            "{id:?}"
        );
    }
    assert_eq!(fs::read_dir(vars.root()).unwrap().count(), 0);
}

#[test]
fn transient_io_failures_are_not_absent_variables() {
    let vars = Vars::new();
    fs::create_dir(vars.path("BootedRom")).unwrap();
    assert!(matches!(
        efivars::read(vars.root(), "BootedRom"),
        Err(error) if error.kind() != io::ErrorKind::NotFound
    ));
    assert!(matches!(
        efivars::booted_rom(vars.root()),
        Err(Error::Io(error)) if error.kind() != io::ErrorKind::NotFound
    ));
}

#[test]
fn absent_stage_record_is_none_and_a_write_is_one_record() {
    let vars = Vars::new();
    assert_eq!(
        efivars::stage(vars.root(), "rom2").unwrap(),
        StageState::None
    );

    efivars::write_stage(vars.root(), "rom2", StageState::Sealed).unwrap();
    // On disk: four attribute bytes, then `GBT1`, the state, three zero bytes.
    assert_eq!(
        vars.bytes("Stage-rom2"),
        b"\x07\0\0\0GBT1\x02\0\0\0".as_slice()
    );
    assert_eq!(
        efivars::stage(vars.root(), "rom2").unwrap(),
        StageState::Sealed
    );

    for state in [StageState::Staging, StageState::Promote, StageState::None] {
        efivars::write_stage(vars.root(), "rom2", state).unwrap();
        assert_eq!(efivars::stage(vars.root(), "rom2").unwrap(), state);
        assert_eq!(
            efivars::read(vars.root(), "Stage-rom2").unwrap().unwrap().0,
            7
        );
    }
}

#[test]
fn malformed_stage_records_are_invalid_data() {
    let vars = Vars::new();
    for (payload, attributes) in [
        (b"GBT1\x00\x00\x00".as_slice(), 7),
        (b"GBT1\x00\x00\x00\x00\x00".as_slice(), 7),
        (b"GBT1\x04\x00\x00\x00".as_slice(), 7),
        (b"GBT1\x00\x00\x00\x01".as_slice(), 7),
        (b"GBT0\x00\x00\x00\x00".as_slice(), 7),
        (b"GBS1\x00\x00\x00\x00".as_slice(), 7),
        (b"GBT1\x02\x00\x00\x00".as_slice(), 6),
        (b"GBT1\x02\x00\x00\x00".as_slice(), 0x8000_0007),
    ] {
        vars.put("Stage-rom2", attributes, payload);
        let error = efivars::stage(vars.root(), "rom2").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{payload:?}");
        assert_eq!(error.to_string(), "Stage record");
    }

    // A truncated attribute header is a filesystem-shaped failure, not a state.
    vars.raw("Stage-rom2", &[7, 0, 0]);
    assert_eq!(
        efivars::stage(vars.root(), "rom2").unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );

    for id in ["", ".", "..", "rom2/x", "rom2\0"] {
        assert_eq!(
            efivars::stage(vars.root(), id).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
            "{id:?}"
        );
        assert_eq!(
            efivars::write_stage(vars.root(), id, StageState::Staging)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput,
            "{id:?}"
        );
        assert_eq!(
            efivars::selected_slot(vars.root(), id).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
            "{id:?}"
        );
    }
}

#[test]
fn selected_slot_is_gbs1_byte_eight() {
    let vars = Vars::new();
    for selected in [0, 1] {
        vars.gbs1("rom2", 7, selected);
        assert_eq!(
            efivars::selected_slot(vars.root(), "rom2").unwrap(),
            selected
        );
    }

    // Payload byte 8, hence on-disk byte 12 after the attribute header.
    vars.gbs1("rom2", 7, 1);
    let bytes = vars.bytes("Slot-rom2");
    assert_eq!(bytes.len(), 28);
    assert_eq!(&bytes[4..8], b"GBS1");
    assert_eq!(bytes[12], 1);

    // A shorter GBS1 prefix still carries the byte; only the magic and the
    // value range are required.
    vars.put("Slot-rom3", 7, b"GBS1\x01\0\0\0\0");
    assert_eq!(efivars::selected_slot(vars.root(), "rom3").unwrap(), 0);
}

#[test]
fn missing_and_invalid_slot_records_are_distinct() {
    let vars = Vars::new();
    assert_eq!(
        efivars::selected_slot(vars.root(), "rom2")
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );

    for (payload, attributes) in [
        (b"GBS1\x01\0\0\0\x02".as_slice(), 7),
        (b"GBS0\x01\0\0\0\x01".as_slice(), 7),
        (b"GBS1".as_slice(), 7),
        (b"GBS1\x01\0\0\0\x01".as_slice(), 6),
    ] {
        vars.put("Slot-rom2", attributes, payload);
        let error = efivars::selected_slot(vars.root(), "rom2").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{payload:?}");
    }
}
