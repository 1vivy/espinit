// SPDX-License-Identifier: GPL-3.0-only
//! The esu takeover overlay: a legacy-LZ4 framed newc archive that PID 1 enters
//! through `rdinit=/esuinit`.
//!
//! The archive carries `esuinit`, the build id and the KMI-selected kernel
//! modules, and nothing else: the stock `/init` is never renamed, copied,
//! inspected or vouched for, so there is no wrapper and no `init` member.
//!
//! Framing is deliberate. The kernel's in-memory legacy-LZ4 decoder
//! (`lib/decompress_unlz4.c`) has no end marker, so after a legacy-LZ4 segment
//! only another LZ4 magic continues unpacking and a raw newc segment aborts it;
//! the archive therefore stays one legacy-LZ4 stream, block-compressed in 8 MiB
//! chunks the way `android-bootimg` reads and writes it. GBL validates nothing
//! about ramdisk framing, so this is entirely the maker's responsibility.

use android_bootimg::cpio::{Cpio, CpioEntry};
use anyhow::{Context, Result, ensure};
use esu_platform::identifier;
use std::collections::BTreeMap;

/// Legacy LZ4 frame magic, little endian.
pub const LZ4_LEGACY_MAGIC: [u8; 4] = 0x184C_2102u32.to_le_bytes();
/// Uncompressed bytes per legacy-LZ4 block, as `android-bootimg` expects.
pub const LZ4_BLOCK_SIZE: usize = 8 * 1024 * 1024;
/// The archive is padded to this boundary before compression.
const ARCHIVE_BLOCK: usize = 512;
/// Member carrying the build id, at the archive root like `esuinit`.
pub const BUILD_ID: &str = "esu-build-id";
/// Directory holding the kernel modules, the only other member family.
pub const LIB: &str = "lib";

/// Build the takeover overlay from the PID-1 binary, the selected module set and
/// the build id.
///
/// `modules` maps a full member path (`lib/<name>.ko`) to its bytes, which is
/// exactly what [`crate::modules::ModuleSet::payload_members`] returns. The
/// archive root gets `esuinit` (0755) and `esu-build-id` (0644); every module is
/// a 0644 file under `lib/`, whose directory entry precedes them so the kernel's
/// extractor never has to create a parent directory.
pub fn build_overlay(
    esuinit: &[u8],
    modules: &BTreeMap<String, Vec<u8>>,
    build_id: &str,
) -> Result<Vec<u8>> {
    ensure!(!esuinit.is_empty(), "esuinit payload is empty");
    let mut cpio = Cpio::new();
    cpio.add(
        "esuinit",
        CpioEntry::regular(0o755, Box::new(esuinit.to_vec())),
    )?;
    cpio.add(
        BUILD_ID,
        CpioEntry::regular(0o644, Box::new(format!("{build_id}\n").into_bytes())),
    )?;
    if !modules.is_empty() {
        cpio.add(LIB, CpioEntry::dir(0o755))?;
    }
    for (path, bytes) in modules {
        member(path)?;
        ensure!(!bytes.is_empty(), "module {path} is empty");
        cpio.add(path, CpioEntry::regular(0o644, Box::new(bytes.clone())))?;
    }

    let mut archive = Vec::new();
    cpio.dump(&mut archive).context("write newc archive")?;
    archive.resize(archive.len().next_multiple_of(ARCHIVE_BLOCK), 0);
    legacy_lz4(&archive)
}

/// Check one module member path: `lib/<name>.ko`, one safe path component.
fn member(path: &str) -> Result<()> {
    let name = path
        .strip_prefix("lib/")
        .with_context(|| format!("module {path} is not under lib/"))?;
    let stem = name
        .strip_suffix(".ko")
        .with_context(|| format!("module {path} does not end in .ko"))?;
    identifier(stem).with_context(|| format!("invalid module name in {path}"))?;
    Ok(())
}

/// Compress an already padded archive into one legacy-LZ4 stream.
///
/// This is the only framing the kernel's initrd path can continue through, so it
/// is not selectable: `android-bootimg`'s `compress` module is private, which is
/// why the encoder lives here and esud calls it.
pub fn legacy_lz4(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoded = Vec::with_capacity(data.len());
    encoded.extend_from_slice(&LZ4_LEGACY_MAGIC);
    for chunk in data.chunks(LZ4_BLOCK_SIZE) {
        let block = lz4::block::compress(
            chunk,
            Some(lz4::block::CompressionMode::HIGHCOMPRESSION(12)),
            false,
        )
        .context("compress overlay block")?;
        encoded.extend_from_slice(
            &u32::try_from(block.len())
                .context("overlay block is larger than 4 GiB")?
                .to_le_bytes(),
        );
        encoded.extend_from_slice(&block);
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modules(entries: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
        entries
            .iter()
            .map(|(path, bytes)| ((*path).to_owned(), bytes.to_vec()))
            .collect()
    }

    #[test]
    fn only_lib_module_paths_are_accepted() {
        for path in ["lib/kernelesp.ko", "lib/thin.ko", "lib/efivar_store.ko"] {
            assert!(member(path).is_ok(), "{path}");
        }
        for path in [
            "kernelesp.ko",
            "lib/kernelesp",
            "lib/.ko",
            "lib/../kernelesp.ko",
            "lib/a/b.ko",
            "lib/init",
            "init",
            "/lib/kernelesp.ko",
        ] {
            assert!(member(path).is_err(), "{path}");
        }
    }

    #[test]
    fn an_empty_esuinit_or_module_is_refused() {
        assert!(build_overlay(b"", &BTreeMap::new(), "id").is_err());
        assert!(build_overlay(b"\x7fELF", &modules(&[("lib/x.ko", b"")]), "id").is_err());
        assert!(build_overlay(b"\x7fELF", &modules(&[("lib/x", b"x")]), "id").is_err());
    }

    #[test]
    fn an_archive_without_modules_has_no_lib_member() {
        let archive = build_overlay(b"\x7fELF", &BTreeMap::new(), "id").unwrap();
        assert!(archive.starts_with(&LZ4_LEGACY_MAGIC));
    }
}
