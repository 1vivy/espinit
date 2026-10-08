// SPDX-License-Identifier: GPL-3.0-only
//! Anti-rollback metadata of an `xbl_config` image.
//!
//! Port of the MIT-licensed `arbscan` (`github.com/syedinsaf/arbscan`,
//! `src/main.rs` on master, same author as the vendored `abl-image`): the
//! command-line shell, the JSON report and the interactive prompts are gone, and
//! what remains is the pure byte scan as one function over one image.
//!
//! An `xbl_config` carries a Qualcomm HASH segment whose OEM metadata block
//! holds three little-endian `u32`s: major version, minor version and the ARB
//! index. Refusing an update whose ARB is higher than the running one is what
//! keeps a firmware downgrade from being offered at all, so the read has to be
//! exact and has to fail closed: anything the port cannot vouch for is `None`,
//! never a zeroed triple.

/// Largest program segment considered, matching arbscan's safety cap.
pub const MAX_SEGMENT: u64 = 20 * 1024 * 1024;
/// The HASH header: five `u32`s.
const HASH_HEADER: usize = 36;
/// Bytes of the segment start scanned for a HASH header.
const SCAN_BYTES: usize = 0x1000;
/// Minimum ELF64 program header size.
const PHENT: usize = 56;
/// Largest program-header table considered.
const MAX_PHENTS: usize = 65536;

/// OEM metadata of one `xbl_config`: two version numbers and the ARB index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arb {
    pub major: u32,
    pub minor: u32,
    pub arb: u32,
}

/// Read the OEM anti-rollback metadata out of an `xbl_config` image.
///
/// The image must be a little-endian ELF64 whose program headers describe a
/// non-executable segment of at least one HASH header and at most
/// [`MAX_SEGMENT`] bytes. The first such segment whose first structurally valid
/// HASH header is followed by a sane metadata block wins; every other outcome is
/// `None`.
pub fn scan(image: &[u8]) -> Option<Arb> {
    if image.len() < 64 || image[..4] != [0x7f, b'E', b'L', b'F'] || image[4] != 2 || image[5] != 1
    {
        return None;
    }
    let phoff = u64_at(image, 0x20)? as usize;
    let phentsize = u16_at(image, 0x36)? as usize;
    let phnum = u16_at(image, 0x38)? as usize;
    if phentsize < PHENT || phnum == 0 {
        return None;
    }
    let table = phnum.checked_mul(phentsize)?;
    if table > MAX_PHENTS {
        return None;
    }
    let headers = image.get(phoff..phoff.checked_add(table)?)?;

    for index in 0..phnum {
        let record = headers.get(index * phentsize..(index + 1) * phentsize)?;
        let (Some(flags), Some(offset), Some(size)) =
            (u32_at(record, 4), u64_at(record, 8), u64_at(record, 32))
        else {
            continue;
        };
        if size == 0 {
            continue;
        }
        // A segment that claims to reach past the file is skipped, exactly as
        // arbscan skips it, rather than ending the scan.
        let Some(end) = offset.checked_add(size) else {
            continue;
        };
        if end > image.len() as u64 {
            continue;
        }
        // Executable segments are code, not the HASH segment.
        if flags & 0x1 != 0 || size < HASH_HEADER as u64 || size > MAX_SEGMENT {
            continue;
        }
        if let Some(arb) = segment_arb(&image[offset as usize..end as usize]) {
            return Some(arb);
        }
    }
    None
}

/// The ARB of one segment, or `None` when it has no structurally valid HASH
/// header or that header's metadata is not sane.
///
/// arbscan reads the metadata of the *first* structurally valid header only and
/// moves on to the next segment when it is not sane; this keeps that behaviour
/// rather than scanning further offsets of the same segment.
fn segment_arb(segment: &[u8]) -> Option<Arb> {
    let header = find_hash_header(segment)?;
    let common = u32_at(segment, header + 4)? as usize;
    let qti = u32_at(segment, header + 8)? as usize;
    let metadata = header
        .checked_add(HASH_HEADER)?
        .checked_add(common)?
        .checked_add(qti)?;
    if metadata.checked_add(12)? > segment.len() {
        return None;
    }
    let arb = Arb {
        major: u32_at(segment, metadata)?,
        minor: u32_at(segment, metadata + 4)?,
        arb: u32_at(segment, metadata + 8)?,
    };
    // ARB 0 is valid (OnePlus out-of-spec images); versions below 1000 are sane.
    (arb.major < 1000 && arb.minor < 1000 && arb.arb < 128).then_some(arb)
}

/// Offset of the first structurally valid HASH header in the first
/// [`SCAN_BYTES`] bytes of `segment`.
fn find_hash_header(segment: &[u8]) -> Option<usize> {
    for offset in (0..SCAN_BYTES.min(segment.len())).step_by(4) {
        if offset + HASH_HEADER > segment.len() {
            break;
        }
        let version = u32_at(segment, offset)?;
        let common = u32_at(segment, offset + 4)? as usize;
        let qti = u32_at(segment, offset + 8)? as usize;
        let oem = u32_at(segment, offset + 12)? as usize;
        let hash_table = u32_at(segment, offset + 16)? as usize;
        if !(1..=10).contains(&version) {
            continue;
        }
        if common > 0x1000 || qti > 0x1000 || oem > 0x4000 {
            continue;
        }
        if hash_table == 0 || !hash_table.is_multiple_of(32) {
            continue;
        }
        if offset + HASH_HEADER + common + qti + oem > segment.len() {
            continue;
        }
        return Some(offset);
    }
    None
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        data.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn u64_at(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        data.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic little-endian ELF64 whose program-header table describes the
    /// given segments as `(flags, bytes)`.
    fn elf64(segments: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let phoff = 64usize;
        let phentsize = 56usize;
        let table = phoff + phentsize * segments.len();
        let mut image = vec![0u8; table];
        image[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        image[4] = 2;
        image[5] = 1;
        image[0x20..0x28].copy_from_slice(&(phoff as u64).to_le_bytes());
        image[0x36..0x38].copy_from_slice(&(phentsize as u16).to_le_bytes());
        image[0x38..0x3a].copy_from_slice(&(segments.len() as u16).to_le_bytes());
        for (index, (flags, bytes)) in segments.iter().enumerate() {
            let offset = image.len();
            let record = phoff + index * phentsize;
            image[record + 4..record + 8].copy_from_slice(&flags.to_le_bytes());
            image[record + 8..record + 16].copy_from_slice(&(offset as u64).to_le_bytes());
            image[record + 32..record + 40].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
            image.extend_from_slice(bytes);
        }
        image
    }

    /// A HASH segment holding one header at `header_offset` and the given
    /// metadata triple after it.
    fn hash_segment(
        header_offset: usize,
        version: u32,
        oem_size: u32,
        metadata: [u32; 3],
    ) -> Vec<u8> {
        let mut segment = vec![0u8; header_offset + HASH_HEADER + 12];
        let header = header_offset;
        segment[header..header + 4].copy_from_slice(&version.to_le_bytes());
        segment[header + 4..header + 8].copy_from_slice(&0u32.to_le_bytes());
        segment[header + 8..header + 12].copy_from_slice(&0u32.to_le_bytes());
        segment[header + 12..header + 16].copy_from_slice(&oem_size.to_le_bytes());
        segment[header + 16..header + 20].copy_from_slice(&32u32.to_le_bytes());
        let at = header + HASH_HEADER;
        for (index, value) in metadata.iter().enumerate() {
            segment[at + index * 4..at + index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        segment
    }

    #[test]
    fn a_synthetic_hash_segment_yields_its_metadata() {
        let segment = hash_segment(0, 1, 12, [1, 2, 5]);
        let image = elf64(&[(4, segment)]);
        assert_eq!(
            scan(&image),
            Some(Arb {
                major: 1,
                minor: 2,
                arb: 5
            })
        );
    }

    #[test]
    fn the_header_may_be_aligned_inside_the_segment() {
        let segment = hash_segment(16, 10, 12, [7, 0, 0]);
        let image = elf64(&[(6, segment)]);
        assert_eq!(
            scan(&image),
            Some(Arb {
                major: 7,
                minor: 0,
                arb: 0
            })
        );
    }

    #[test]
    fn only_non_executable_segments_are_considered() {
        let segment = hash_segment(0, 1, 12, [1, 2, 5]);
        assert_eq!(scan(&elf64(&[(5, segment)])), None);
    }

    #[test]
    fn the_first_sane_segment_wins_and_an_insane_one_falls_through() {
        let insane = hash_segment(0, 1, 12, [1, 2, 200]);
        let sane = hash_segment(0, 1, 12, [3, 4, 6]);
        let image = elf64(&[(4, insane), (4, sane)]);
        assert_eq!(
            scan(&image),
            Some(Arb {
                major: 3,
                minor: 4,
                arb: 6
            })
        );

        // The same segment scanned alone is refused rather than reported zeroed.
        assert_eq!(
            scan(&elf64(&[(4, hash_segment(0, 1, 12, [1, 2, 200]))])),
            None
        );
    }

    #[test]
    fn structurally_invalid_headers_are_refused() {
        let cases: &[(u32, u32, [u32; 3])] = &[
            (0, 12, [1, 2, 5]),
            (11, 12, [1, 2, 5]),
            (1, 0x4001, [1, 2, 5]),
            (1, 12, [1000, 2, 5]),
            (1, 12, [1, 1000, 5]),
            (1, 12, [1, 2, 128]),
        ];
        for (version, oem_size, metadata) in cases {
            let image = elf64(&[(4, hash_segment(0, *version, *oem_size, *metadata))]);
            assert_eq!(scan(&image), None, "{version} {oem_size} {metadata:?}");
        }

        // A hash table size of zero or a non-multiple of 32 is refused too.
        for hash_table in [0u32, 33, 1] {
            let mut segment = hash_segment(0, 1, 12, [1, 2, 5]);
            segment[16..20].copy_from_slice(&hash_table.to_le_bytes());
            assert_eq!(scan(&elf64(&[(4, segment)])), None, "{hash_table}");
        }

        // A segment shorter than one header, or a header past its end.
        assert_eq!(scan(&elf64(&[(4, vec![0u8; HASH_HEADER - 1])])), None);
        assert_eq!(
            scan(&elf64(&[(
                4,
                hash_segment(0, 1, 12, [1, 2, 5])[..40].to_vec()
            )])),
            None
        );
    }

    #[test]
    fn non_elf_and_broken_tables_are_refused() {
        assert_eq!(scan(b""), None);
        assert_eq!(scan(b"not an elf at all, not even close to 64 bytes"), None);

        let mut big_endian = elf64(&[(4, hash_segment(0, 1, 12, [1, 2, 5]))]);
        big_endian[5] = 2;
        assert_eq!(scan(&big_endian), None);

        let mut not_64 = elf64(&[(4, hash_segment(0, 1, 12, [1, 2, 5]))]);
        not_64[4] = 1;
        assert_eq!(scan(&not_64), None);

        let mut no_headers = elf64(&[]);
        no_headers[0x38..0x3a].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(scan(&no_headers), None);

        let mut short_header = elf64(&[(4, hash_segment(0, 1, 12, [1, 2, 5]))]);
        short_header[0x36..0x38].copy_from_slice(&32u16.to_le_bytes());
        assert_eq!(scan(&short_header), None);

        // A segment that claims to end past the file is skipped, not read.
        let mut past_end = elf64(&[(4, hash_segment(0, 1, 12, [1, 2, 5]))]);
        let size = past_end.len() as u64 + 1;
        past_end[64 + 32..64 + 40].copy_from_slice(&size.to_le_bytes());
        assert_eq!(scan(&past_end), None);

        // A table larger than the 64 KiB cap is refused.
        let mut huge_table = elf64(&[(4, hash_segment(0, 1, 12, [1, 2, 5]))]);
        huge_table[0x38..0x3a].copy_from_slice(&1200u16.to_le_bytes());
        assert_eq!(scan(&huge_table), None);
    }
}
