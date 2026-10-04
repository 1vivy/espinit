//! Pure GPT parsing used for ESP discovery.
//!
//! Discovery is driven by the GPT type GUID on whole block disks enumerated
//! through sysfs, never by a device-name allowlist. The functions here operate
//! on raw bytes so they can be exercised without a block device.

/// EFI System Partition type GUID `C12A7328-F81F-11D2-BA4B-00A0C93EC93B` in
/// on-disk mixed-endian byte order.
pub const ESP_TYPE_GUID: [u8; 16] = [
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
];

/// GPT header signature at the start of LBA 1.
pub const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

/// Minimum GPT header size accepted.
const MIN_HEADER_SIZE: u32 = 92;

/// Largest partition entry size accepted, bounding the read.
const MAX_ENTRY_SIZE: u32 = 4096;

/// Largest partition entry count accepted, bounding the read.
const MAX_ENTRY_COUNT: u32 = 4096;

/// Parsed GPT header fields required for partition discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub entries_lba: u64,
    pub entry_count: u32,
    pub entry_size: u32,
}

/// One matching partition entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub index: u32,
    pub first_lba: u64,
    pub last_lba: u64,
}

/// Parse the GPT header from the LBA 1 sector contents. Returns `None` when the
/// signature or the structural fields are invalid, which marks the disk as
/// "not a usable GPT" rather than a hard boot failure.
pub fn parse_header(sector: &[u8]) -> Option<Header> {
    if sector.len() < MIN_HEADER_SIZE as usize {
        return None;
    }

    if !sector.starts_with(GPT_SIGNATURE) {
        return None;
    }

    let header_size = read_u32(sector, 12)?;
    if header_size < MIN_HEADER_SIZE {
        return None;
    }

    let entries_lba = read_u64(sector, 72)?;
    let entry_count = read_u32(sector, 80)?;
    let entry_size = read_u32(sector, 84)?;

    if entries_lba == 0 {
        return None;
    }

    if entry_count == 0 || entry_count > MAX_ENTRY_COUNT {
        return None;
    }

    if (entry_size as usize) < 128 || entry_size > MAX_ENTRY_SIZE || entry_size % 8 != 0 {
        return None;
    }

    Some(Header {
        entries_lba,
        entry_count,
        entry_size,
    })
}

/// Find every entry whose type GUID equals `type_guid`, in table order.
pub fn find_entries(entries: &[u8], entry_size: usize, type_guid: &[u8; 16]) -> Vec<Entry> {
    let mut found = Vec::new();

    if entry_size < 128 {
        return found;
    }

    for index in 0..(entries.len() / entry_size) {
        let base = index * entry_size;
        let entry = &entries[base..base + entry_size];

        if !entry.starts_with(type_guid) {
            continue;
        }

        let Some(first_lba) = read_u64(entry, 32) else {
            continue;
        };
        let Some(last_lba) = read_u64(entry, 40) else {
            continue;
        };

        if first_lba == 0 || last_lba < first_lba {
            continue;
        }

        found.push(Entry {
            index: index as u32,
            first_lba,
            last_lba,
        });
    }

    found
}

fn read_u32(buffer: &[u8], offset: usize) -> Option<u32> {
    let bytes: [u8; 4] = buffer.get(offset..offset + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn read_u64(buffer: &[u8], offset: usize) -> Option<u64> {
    let bytes: [u8; 8] = buffer.get(offset..offset + 8)?.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOREIGN_GUID: [u8; 16] = [0xaf; 16];

    /// A well formed LBA 1 sector: signature, header size 92, table at LBA 2
    /// with 128 entries of 128 bytes.
    fn header_sector() -> [u8; 512] {
        let mut sector = [0u8; 512];
        sector[..8].copy_from_slice(GPT_SIGNATURE);
        sector[12..16].copy_from_slice(&92u32.to_le_bytes());
        sector[72..80].copy_from_slice(&2u64.to_le_bytes());
        sector[80..84].copy_from_slice(&128u32.to_le_bytes());
        sector[84..88].copy_from_slice(&128u32.to_le_bytes());
        sector
    }

    fn sector_with(entry_count: u32, entry_size: u32, entries_lba: u64) -> [u8; 512] {
        let mut sector = header_sector();
        sector[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        sector[80..84].copy_from_slice(&entry_count.to_le_bytes());
        sector[84..88].copy_from_slice(&entry_size.to_le_bytes());
        sector
    }

    fn entry(type_guid: &[u8; 16], first_lba: u64, last_lba: u64) -> [u8; 128] {
        let mut entry = [0u8; 128];
        entry[..16].copy_from_slice(type_guid);
        entry[32..40].copy_from_slice(&first_lba.to_le_bytes());
        entry[40..48].copy_from_slice(&last_lba.to_le_bytes());
        entry
    }

    #[test]
    fn header_reports_the_table_location_and_entry_shape() {
        let sector = sector_with(128, 128, 2);
        assert_eq!(
            parse_header(&sector),
            Some(Header {
                entries_lba: 2,
                entry_count: 128,
                entry_size: 128,
            })
        );

        let tail_sector = sector_with(MAX_ENTRY_COUNT, MAX_ENTRY_SIZE, u64::MAX);
        assert_eq!(
            parse_header(&tail_sector),
            Some(Header {
                entries_lba: u64::MAX,
                entry_count: MAX_ENTRY_COUNT,
                entry_size: MAX_ENTRY_SIZE,
            })
        );
    }

    #[test]
    fn header_rejects_a_missing_signature_or_a_short_read() {
        let sector = header_sector();

        let mut wrong = sector;
        wrong[0] = b'X';
        assert_eq!(parse_header(&wrong), None);

        let mut only_signature = [0u8; MIN_HEADER_SIZE as usize];
        only_signature[..8].copy_from_slice(GPT_SIGNATURE);
        assert_eq!(
            parse_header(&only_signature[..MIN_HEADER_SIZE as usize - 1]),
            None
        );
        assert_eq!(parse_header(&[]), None);
    }

    #[test]
    fn header_rejects_a_declared_size_below_the_minimum() {
        let mut sector = header_sector();
        sector[12..16].copy_from_slice(&(MIN_HEADER_SIZE - 1).to_le_bytes());
        assert_eq!(parse_header(&sector), None);

        sector[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            parse_header(&sector),
            Some(Header {
                entries_lba: 2,
                entry_count: 128,
                entry_size: 128,
            })
        );
    }

    #[test]
    fn header_rejects_out_of_range_table_and_entry_bounds() {
        assert_eq!(
            parse_header(&sector_with(128, 128, 0)),
            None,
            "zero table LBA"
        );
        assert_eq!(parse_header(&sector_with(0, 128, 2)), None, "zero entries");
        assert_eq!(
            parse_header(&sector_with(MAX_ENTRY_COUNT + 1, 128, 2)),
            None,
            "entry count above the read bound"
        );

        assert_eq!(
            parse_header(&sector_with(128, 127, 2)),
            None,
            "entry below 128"
        );
        assert_eq!(
            parse_header(&sector_with(128, MAX_ENTRY_SIZE + 1, 2)),
            None,
            "entry above the read bound"
        );
        assert_eq!(
            parse_header(&sector_with(128, 130, 2)),
            None,
            "entry size not a multiple of 8"
        );
        assert_eq!(
            parse_header(&sector_with(128, 8, 2)),
            None,
            "entry size far below the header"
        );
    }

    #[test]
    fn find_entries_returns_esp_entries_in_table_order_with_their_indices() {
        let mut table = Vec::new();
        table.extend_from_slice(&entry(&FOREIGN_GUID, 2048, 4095));
        table.extend_from_slice(&entry(&ESP_TYPE_GUID, 4096, 8191));
        table.extend_from_slice(&entry(&FOREIGN_GUID, 8192, 12287));
        table.extend_from_slice(&entry(&ESP_TYPE_GUID, 12288, 16383));

        assert_eq!(
            find_entries(&table, 128, &ESP_TYPE_GUID),
            vec![
                Entry {
                    index: 1,
                    first_lba: 4096,
                    last_lba: 8191
                },
                Entry {
                    index: 3,
                    first_lba: 12288,
                    last_lba: 16383
                },
            ]
        );
        assert_eq!(find_entries(&table, 128, &FOREIGN_GUID).len(), 2);
    }

    #[test]
    fn find_entries_skips_malformed_bounds_and_ignores_a_partial_entry() {
        let mut table = Vec::new();
        table.extend_from_slice(&entry(&ESP_TYPE_GUID, 0, 4095));
        table.extend_from_slice(&entry(&ESP_TYPE_GUID, 8192, 4096));
        table.extend_from_slice(&entry(&ESP_TYPE_GUID, 4096, 4096));
        table.extend_from_slice(&entry(&ESP_TYPE_GUID, 1, u64::MAX));
        table.extend_from_slice(&[0xff; 64]);

        assert_eq!(
            find_entries(&table, 128, &ESP_TYPE_GUID),
            vec![
                Entry {
                    index: 2,
                    first_lba: 4096,
                    last_lba: 4096
                },
                Entry {
                    index: 3,
                    first_lba: 1,
                    last_lba: u64::MAX
                },
            ]
        );

        assert!(find_entries(&table[..127], 128, &ESP_TYPE_GUID).is_empty());
        assert!(find_entries(&table, 64, &ESP_TYPE_GUID).is_empty());
        assert!(find_entries(&[], 128, &ESP_TYPE_GUID).is_empty());
    }
}
