// SPDX-License-Identifier: GPL-3.0-only
//! The takeover overlay's wire contract, checked by decoding it the way the
//! kernel does and parsing the newc members by hand.
//!
//! `build_overlay` is the only producer of the archive PID 1 is entered through,
//! so this test decodes it independently of `android-bootimg`: a legacy-LZ4
//! stream followed by one newc archive whose members are exactly `esuinit`,
//! `esu-build-id` and `lib/<name>.ko`.

use ota_core::overlay::{LZ4_BLOCK_SIZE, LZ4_LEGACY_MAGIC, build_overlay};
use std::collections::BTreeMap;

/// One decoded newc member: its name, mode and bytes.
struct Member {
    name: String,
    mode: u32,
    data: Vec<u8>,
}

/// Decode a legacy-LZ4 stream: magic, then `u32` block size and block, until the
/// input ends.
fn decode_legacy_lz4(encoded: &[u8]) -> Vec<u8> {
    assert!(encoded.starts_with(&LZ4_LEGACY_MAGIC));
    let mut decoded = Vec::new();
    let mut offset = LZ4_LEGACY_MAGIC.len();
    while offset < encoded.len() {
        let size = u32::from_le_bytes(encoded[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        let mut block = vec![0u8; LZ4_BLOCK_SIZE];
        let written = lz4::block::decompress_to_buffer(
            &encoded[offset..offset + size],
            Some(LZ4_BLOCK_SIZE as i32),
            &mut block,
        )
        .unwrap();
        decoded.extend_from_slice(&block[..written]);
        offset += size;
    }
    decoded
}

fn hex(field: &[u8]) -> u32 {
    u32::from_str_radix(std::str::from_utf8(field).unwrap(), 16).unwrap()
}

/// Parse a newc archive: 110-byte headers, 4-byte aligned names and data, up to
/// the `TRAILER!!!` member.
fn parse_newc(archive: &[u8]) -> Vec<Member> {
    let mut members = Vec::new();
    let mut offset = 0;
    loop {
        assert_eq!(&archive[offset..offset + 6], b"070701", "newc magic");
        let header = &archive[offset..offset + 110];
        let mode = hex(&header[14..22]);
        let size = hex(&header[54..62]) as usize;
        let name_length = hex(&header[94..102]) as usize;
        let name_start = offset + 110;
        let name = std::str::from_utf8(&archive[name_start..name_start + name_length - 1])
            .unwrap()
            .to_owned();
        assert_eq!(
            archive[name_start + name_length - 1],
            0,
            "NUL terminated name"
        );
        let data_start = (name_start + name_length).next_multiple_of(4);
        let data = archive[data_start..data_start + size].to_vec();
        offset = (data_start + size).next_multiple_of(4);
        if name == "TRAILER!!!" {
            return members;
        }
        members.push(Member { name, mode, data });
    }
}

#[test]
fn the_overlay_carries_exactly_esuinit_the_build_id_and_the_module_set() {
    let esuinit = b"\x7fELF esuinit".to_vec();
    let modules: BTreeMap<String, Vec<u8>> = [
        ("lib/kernelesp.ko".to_owned(), b"kernelesp".to_vec()),
        ("lib/thin.ko".to_owned(), b"thin".to_vec()),
        ("lib/efivar_store.ko".to_owned(), b"efivar_store".to_vec()),
    ]
    .into_iter()
    .collect();

    let archive = build_overlay(&esuinit, &modules, "0123456789ab").unwrap();
    let decoded = decode_legacy_lz4(&archive);
    // The archive itself is padded to 512 bytes before compression.
    assert_eq!(decoded.len() % 512, 0);
    let members = parse_newc(&decoded);

    let names: Vec<&str> = members.iter().map(|member| member.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "esu-build-id",
            "esuinit",
            "lib",
            "lib/efivar_store.ko",
            "lib/kernelesp.ko",
            "lib/thin.ko",
        ]
    );
    for member in &members {
        let mode = member.mode & 0o7777;
        match member.name.as_str() {
            "esuinit" => {
                assert_eq!(mode, 0o755);
                assert_eq!(member.data, esuinit);
            }
            "esu-build-id" => {
                assert_eq!(mode, 0o644);
                assert_eq!(member.data, b"0123456789ab\n");
            }
            "lib" => assert_eq!(mode, 0o755),
            name => {
                assert_eq!(mode, 0o644);
                assert_eq!(member.data, modules[name]);
            }
        }
        // No member may take over the first-stage entry: `rdinit=/esuinit` is
        // the only entry the launcher adds.
        assert_ne!(member.name, "init");
        assert_ne!(member.name, "init.esureal");
    }
}

#[test]
fn an_overlay_without_modules_is_still_a_valid_archive() {
    let archive = build_overlay(b"\x7fELF", &BTreeMap::new(), "id").unwrap();
    let members = parse_newc(&decode_legacy_lz4(&archive));
    let names: Vec<&str> = members.iter().map(|member| member.name.as_str()).collect();
    assert_eq!(names, ["esu-build-id", "esuinit"]);
}

#[test]
fn a_single_block_overlay_stays_one_legacy_lz4_stream() {
    // Every decoded block is length-prefixed by exactly one u32, and there is no
    // trailing marker: the kernel's decoder stops at the end of the input.
    let archive = build_overlay(&vec![0x41; 4096], &BTreeMap::new(), "id").unwrap();
    assert_eq!(&archive[..4], &LZ4_LEGACY_MAGIC);
    let mut offset = 4;
    let mut blocks = 0;
    while offset < archive.len() {
        let size = u32::from_le_bytes(archive[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4 + size;
        blocks += 1;
    }
    assert_eq!(offset, archive.len());
    assert_eq!(blocks, 1);
}
