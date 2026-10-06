// SPDX-License-Identifier: Apache-2.0
use lvm2_meta::{
    DeviceNumber, Devices, Error, Layer, MAX_DEPTH, MAX_NODES, MAX_TEXT_BYTES, MAX_TOKEN_BYTES,
    SegmentType, TableOptions, VolumeGroup, read, text,
};
use std::{collections::BTreeMap, path::PathBuf, process::Command};

fn fixture(variant: &str, name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(variant)
        .join(name)
}
fn image(variant: &str) -> Vec<u8> {
    let result = Command::new("gzip")
        .arg("-dc")
        .arg(fixture(variant, "pv-prefix.img.gz"))
        .output()
        .expect("gzip is a test prerequisite");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    result.stdout
}
fn number(s: &str) -> DeviceNumber {
    let (major, minor) = s.split_once(':').unwrap();
    DeviceNumber {
        major: major.parse().unwrap(),
        minor: minor.parse().unwrap(),
    }
}
fn devices(variant: &str, vg: &VolumeGroup) -> Devices {
    let mut devices = Devices::default();
    for line in std::fs::read_to_string(fixture(variant, "devices.txt"))
        .unwrap()
        .lines()
    {
        let (name, device) = line.split_once(' ').unwrap();
        if let Some(pv) = vg.physical_volumes().get(name) {
            devices
                .physical_volumes
                .insert(pv.id.clone(), number(device));
        } else {
            let (name, layer) = name
                .strip_suffix("-tpool")
                .map_or((name, Layer::Volume), |s| (s, Layer::ThinPool));
            devices
                .logical_volumes
                .entry(name.into())
                .or_default()
                .insert(layer, number(device));
        }
    }
    devices
}
fn oracle(variant: &str) -> BTreeMap<String, Vec<String>> {
    let mut tables = BTreeMap::<String, Vec<String>>::new();
    for line in std::fs::read_to_string(fixture(variant, "dmsetup.txt"))
        .unwrap()
        .lines()
    {
        let (name, table) = line.split_once(": ").unwrap();
        tables.entry(name.into()).or_default().push(table.into());
    }
    tables
}
#[test]
fn derives_exact_active_tables_from_real_lvm2_metadata() {
    for variant in ["", "zero-ignore", "zero-passdown"] {
        let raw = image(variant);
        let metadata = read(&mut raw.as_slice()).unwrap();
        let vg = &metadata.vg;
        let devices = devices(variant, vg);
        let mut expected = oracle(variant);
        for name in vg.logical_volumes().keys() {
            for layer in [Layer::Volume, Layer::ThinPool] {
                let Ok(dm_name) = vg.dm_name(name, layer) else {
                    continue;
                };
                if let Some(table) = expected.remove(&dm_name) {
                    assert_eq!(
                        vg.dm_table(name, layer, &devices, TableOptions::default())
                            .unwrap(),
                        table,
                        "{variant}/{dm_name}"
                    );
                }
            }
        }
        assert!(
            expected.is_empty(),
            "unaccounted oracle tables: {expected:?}"
        );
        // Firmware byte mapping is compared with the independent kernel table,
        // not reconstructed from our own model fields.
        let table = oracle(variant).remove("rom-winhost").unwrap();
        let spans = vg.physical_extents("winhost").unwrap();
        let expected: Vec<_> = table
            .iter()
            .map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                (
                    fields[0].parse::<u64>().unwrap() * 512,
                    fields[1].parse::<u64>().unwrap() * 512,
                    fields[4].parse::<u64>().unwrap() * 512,
                )
            })
            .collect();
        assert_eq!(
            spans
                .iter()
                .map(|s| (s.logical_offset, s.length, s.physical_offset))
                .collect::<Vec<_>>(),
            expected
        );
        assert!(
            spans
                .iter()
                .all(|s| devices.physical_volumes.contains_key(s.pv_id))
        );
        assert!(matches!(
            vg.physical_extents("userdata_1"),
            Err(Error::Unsupported(_))
        ));
        let SegmentType::Thin { origin, .. } = &vg.lv("linux_snapshot").unwrap().segments[0].kind
        else {
            panic!("snapshot is not thin");
        };
        assert_eq!(origin.as_deref(), Some("userdata_1"));
    }
}

// Independent bitwise CRC only repairs mutated test headers; the positive
// checksums and every expected table come from the real-tool fixtures.
fn crc(bytes: &[u8]) -> u32 {
    let mut value = 0xf597a6cf;
    for byte in bytes {
        value ^= u32::from(*byte);
        for _ in 0..8 {
            value = if value & 1 != 0 {
                (value >> 1) ^ 0xedb88320
            } else {
                value >> 1
            };
        }
    }
    value
}
fn put64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn get64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn fix_label(bytes: &mut [u8], offset: usize) {
    put32(bytes, offset + 16, crc(&bytes[offset + 20..offset + 512]));
}
fn fix_mda(bytes: &mut [u8]) {
    put32(bytes, 4096, crc(&bytes[4100..4608]));
}
fn original_text() -> String {
    read(&mut image("").as_slice()).unwrap().text
}

#[test]
fn refuses_corruption_at_every_checksum_boundary() {
    for (offset, expected) in [(512 + 100, "label"), (4096 + 30, "MDA")] {
        let mut bytes = image("");
        bytes[offset] ^= 1;
        assert!(
            matches!(read(&mut bytes.as_slice()), Err(Error::Checksum(kind)) if kind == expected)
        );
    }
    let mut bytes = image("");
    let start = 4096 + get64(&bytes, 4136) as usize;
    bytes[start + 10] ^= 1;
    assert!(matches!(
        read(&mut bytes.as_slice()),
        Err(Error::Checksum("metadata text"))
    ));
}
#[test]
fn scans_all_four_label_sectors_and_rejects_ambiguous_identity() {
    for sector in [0, 2, 3] {
        let mut bytes = image("");
        bytes.copy_within(512..1024, sector * 512);
        bytes[512..1024].fill(0);
        put64(&mut bytes, sector * 512 + 8, sector as u64);
        fix_label(&mut bytes, sector * 512);
        assert_eq!(
            read(&mut bytes.as_slice()).unwrap().pv.label_sector,
            sector as u64
        );
    }
    let mut bytes = image("");
    bytes.copy_within(512..1024, 1024);
    put64(&mut bytes, 1032, 2);
    fix_label(&mut bytes, 1024);
    assert!(matches!(
        read(&mut bytes.as_slice()),
        Err(Error::Invalid(_))
    ));
}
#[test]
fn refuses_truncated_headers_text_and_unbounded_raw_locations() {
    let bytes = image("");
    let end = 4096 + get64(&bytes, 4136) + get64(&bytes, 4144);
    for length in [511, 2047, 4607, end as usize - 1] {
        assert!(matches!(read(&mut &bytes[..length]), Err(Error::Io(_))));
    }
    for (offset, value) in [(4136, 0), (4136, 256), (4136, u64::MAX), (4144, u64::MAX)] {
        let mut bytes = image("");
        put64(&mut bytes, offset, value);
        fix_mda(&mut bytes);
        assert!(matches!(
            read(&mut bytes.as_slice()),
            Err(Error::Invalid(_)) | Err(Error::Limit(_))
        ));
    }
}
#[test]
fn unwraps_the_circular_text_buffer_without_reading_the_mda_header() {
    let mut bytes = image("");
    let expected = read(&mut bytes.as_slice()).unwrap().text;
    let old = get64(&bytes, 4136) as usize;
    let length = get64(&bytes, 4144) as usize;
    let text = bytes[4096 + old..4096 + old + length].to_vec();
    let area_size = get64(&bytes, 4096 + 32) as usize;
    let offset = area_size - 512;
    bytes[4096 + offset..4096 + area_size].copy_from_slice(&text[..512]);
    bytes[4608..4608 + length - 512].copy_from_slice(&text[512..]);
    put64(&mut bytes, 4136, offset as u64);
    fix_mda(&mut bytes);
    assert_eq!(read(&mut bytes.as_slice()).unwrap().text, expected);
    bytes[4608] ^= 1;
    assert!(matches!(
        read(&mut bytes.as_slice()),
        Err(Error::Checksum("metadata text"))
    ));
}
#[test]
fn does_not_promote_precommit_or_ignored_metadata() {
    let mut bytes = image("");
    let original = read(&mut bytes.as_slice()).unwrap();
    // A second raw location is a precommit, even when its bytes are invalid.
    bytes.copy_within(4136..4160, 4160);
    put32(&mut bytes, 4176, 0);
    fix_mda(&mut bytes);
    assert_eq!(read(&mut bytes.as_slice()).unwrap().text, original.text);
    put32(&mut bytes, 4156, 1);
    fix_mda(&mut bytes);
    assert!(matches!(
        read(&mut bytes.as_slice()),
        Err(Error::Invalid(_))
    ));
}
#[test]
fn rejects_unsupported_or_unsafe_volume_graphs() {
    let source = original_text();
    for (from, to) in [
        ("type = \"striped\"", "type = \"raid1\""),
        ("stripe_count = 1", "stripe_count = 2"),
        ("thin_pool = \"pool\"", "thin_pool = \"missing\""),
        ("\"pv0\", 0", "\"pv0\", 127"),
        ("start_extent = 4", "start_extent = 3"),
        ("extent_count = 4", "extent_count = 18446744073709551615"),
        ("origin = \"userdata_1\"", "origin = \"linux_snapshot\""),
        ("device_id = 2", "device_id = 1"),
        ("device_id = 2", "device_id = 65536"),
        ("device_id = 2", "device_id = 16777216"),
        ("pe_count = 127", "pe_count = 18446744073709551615"),
        ("chunk_size = 128", "chunk_size = 127"),
        ("discards = \"nopassdown\"", "discards = \"future\""),
        (
            "type = \"thin\"",
            "type = \"thin\" external_origin = \"separator\"",
        ),
        (
            "type = \"thin-pool\"",
            "type = \"thin-pool\" message1 { delete = 2 }",
        ),
    ] {
        assert!(source.contains(from), "mutation not applied: {from}");
        assert!(
            VolumeGroup::parse(&source.replacen(from, to, 1)).is_err(),
            "accepted {to}"
        );
    }
    assert!(matches!(
        VolumeGroup::parse(&source.replace("type = \"striped\"", "type = \"raid1\"")),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn thin_device_ids_below_the_reserved_range_are_accepted() {
    let source = original_text();
    // 0xFFFF is the largest id LVM2 metadata may own.
    let boundary = source.replacen("device_id = 2", "device_id = 65535", 1);
    VolumeGroup::parse(&boundary).unwrap();
    // 0x10000 starts the reserved per-ROM firmware-view range.
    let reserved = source.replacen("device_id = 2", "device_id = 65536", 1);
    assert!(matches!(
        VolumeGroup::parse(&reserved),
        Err(Error::Invalid(_))
    ));
}
#[test]
fn text_bounds_prevent_ambiguous_or_unbounded_parsing() {
    for input in [
        "a {",
        "a = [1",
        "a = \"unterminated",
        "a = 1 a = 2",
        "a { a=1 } a=2",
        "a=18446744073709551616",
        "a=-9223372036854775809",
        "a=1\0",
        "a=1.5",
    ] {
        assert!(text::parse(input).is_err(), "accepted {input:?}");
    }
    for input in [
        format!(
            "{}{}",
            "a {".repeat(MAX_DEPTH + 1),
            "}".repeat(MAX_DEPTH + 1)
        ),
        format!(
            "a={}0{}",
            "[".repeat(MAX_DEPTH + 1),
            "]".repeat(MAX_DEPTH + 1)
        ),
        " ".repeat(MAX_TEXT_BYTES + 1),
        format!("a=\"{}\"", "x".repeat(MAX_TOKEN_BYTES + 1)),
        format!("a=[{}]", "1,".repeat(MAX_NODES + 1)),
    ] {
        assert!(matches!(text::parse(&input), Err(Error::Limit(_))));
    }
}
#[test]
fn preserves_lvm_literal_escapes_comments_and_signed_integers() {
    let config = text::parse(
        r#"# comment
"quoted\"key" { values = ["quote\" slash\\ literal\n #", 'single\n', -17, 077, [], bare] }
"#,
    )
    .unwrap();
    let section = config["quoted\"key"].section().unwrap();
    let values = section["values"].array().unwrap();
    assert_eq!(values[0].string().unwrap(), "quote\" slash\\ literal\\n #");
    assert_eq!(values[1].string().unwrap(), "single\\n");
    assert_eq!(values[2], text::Value::Integer(-17));
    assert_eq!(values[3].unsigned().unwrap(), 63);
    assert_eq!(values[4].array().unwrap(), &[]);
    assert_eq!(values[5].string().unwrap(), "bare");
}

#[test]
fn redundant_mdas_select_committed_seqno_and_refuse_split_brain() {
    struct Sparse {
        prefix: Vec<u8>,
        tail_start: u64,
        tail: Vec<u8>,
    }
    impl lvm2_meta::ReadAt for Sparse {
        fn read_exact_at(&mut self, offset: u64, buffer: &mut [u8]) -> std::io::Result<()> {
            if offset >= self.tail_start {
                self.tail
                    .as_slice()
                    .read_exact_at(offset - self.tail_start, buffer)
            } else {
                self.prefix.as_slice().read_exact_at(offset, buffer)
            }
        }
    }
    let prefix = image("");
    let original = read(&mut prefix.as_slice()).unwrap();
    let area = &original.pv.metadata_areas[0];
    let tail_start = original.pv.device_size - area.size;
    let mut disk = Sparse {
        tail: prefix[area.offset as usize..(area.offset + area.size) as usize].to_vec(),
        prefix,
        tail_start,
    };
    // Add a trailing MDA to the label; the data-area size remains unspecified.
    put64(&mut disk.prefix, 512 + 120, tail_start);
    put64(&mut disk.prefix, 512 + 128, area.size);
    disk.prefix[512 + 136..512 + 152].fill(0);
    fix_label(&mut disk.prefix, 512);
    put64(&mut disk.tail, 24, tail_start);
    let text_start = get64(&disk.tail, 40) as usize;
    for (seqno, expected) in [(12, 13), (14, 14)] {
        let text = original
            .text
            .replace("seqno = 13", &format!("seqno = {seqno}"));
        disk.tail[text_start..text_start + text.len()].copy_from_slice(text.as_bytes());
        put64(&mut disk.tail, 48, text.len() as u64);
        put32(&mut disk.tail, 56, crc(text.as_bytes()));
        let checksum = crc(&disk.tail[4..512]);
        put32(&mut disk.tail, 0, checksum);
        assert_eq!(read(&mut disk).unwrap().vg.seqno(), expected);
    }
    let conflicting = original.text.replacen("creation_host", "creation_hint", 1);
    disk.tail[text_start..text_start + conflicting.len()].copy_from_slice(conflicting.as_bytes());
    put64(&mut disk.tail, 48, conflicting.len() as u64);
    put32(&mut disk.tail, 56, crc(conflicting.as_bytes()));
    let checksum = crc(&disk.tail[4..512]);
    put32(&mut disk.tail, 0, checksum);
    assert!(matches!(read(&mut disk), Err(Error::ConflictingMetadata)));
    disk.tail[20] ^= 1;
    assert!(matches!(read(&mut disk), Err(Error::Checksum("MDA"))));
}
