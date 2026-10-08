// SPDX-License-Identifier: GPL-3.0-only
//! Kernel-module-interface identity of a boot image.
//!
//! An update is only admitted when the payload carries a module set for the KMI
//! the *target* kernel reports, because a set built for another branch or
//! generation would either refuse to load or load with the wrong ABI. The KMI is
//! read from the kernel's own version banner, the same fact
//! `scripts/kmi_modules.py` checks on the host, so the device and the host agree
//! by construction.

use android_bootimg::parser::BootImage;
use anyhow::{Context, Result, ensure};
use esu_platform::identifier;

/// One module-set identity: the kernel branch and generation a set was built
/// for, as `esu/kmi/<branch>-<generation>/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kmi {
    /// For example `android16-6.12`: the Android release and the kernel's
    /// `major.minor`.
    pub branch: String,
    /// The KMI generation, for example `6`.
    pub generation: u32,
}

/// Read the KMI from an Android boot image.
///
/// The kernel block is decompressed exactly as the bootloader would read it and
/// the first `<major>.<minor>.<patch>-<android><n>-<generation>` version string
/// wins, which is the `Linux version 6.12.23-android16-6-…` banner. A raw
/// (uncompressed) kernel is read as is.
pub fn kmi_from_boot(image: &[u8]) -> Result<Kmi> {
    guard(image)?;
    let boot = BootImage::parse(image).context("parse boot image")?;
    let kernel = boot
        .get_blocks()
        .get_kernel()
        .context("boot image has no kernel block")?;
    let mut decompressed = Vec::new();
    kernel
        .dump(&mut decompressed, false)
        .context("decompress kernel block")?;
    let kmi = parse_banner(&decompressed).with_context(|| {
        format!(
            "no KMI version string in a {} byte kernel",
            decompressed.len()
        )
    })?;
    validate(&kmi)?;
    Ok(kmi)
}

/// Reject an image the upstream parser would slice blindly.
///
/// `android_bootimg` trusts the header's fixed offsets and its own size fields,
/// so a truncated or non-v3/v4 image is refused here before it is parsed: a
/// payload's base image is operator-supplied data, not a file this build wrote.
/// The checks mirror `esud`'s `patch_boot` guard for the same reason.
fn guard(image: &[u8]) -> Result<()> {
    ensure!(
        image.len() >= 4096 && image.starts_with(b"ANDROID!"),
        "expected an Android boot image"
    );
    let version = u32::from_le_bytes(image[40..44].try_into().expect("checked length"));
    ensure!(
        matches!(version, 3 | 4),
        "only Android boot/init_boot v3/v4 carries a KMI"
    );
    let header_size = u32::from_le_bytes(image[20..24].try_into().expect("checked length"));
    ensure!(
        header_size == if version == 3 { 1580 } else { 1584 },
        "invalid boot header size"
    );
    let kernel_size = u32::from_le_bytes(image[8..12].try_into().expect("checked length")) as usize;
    let ramdisk_size =
        u32::from_le_bytes(image[12..16].try_into().expect("checked length")) as usize;
    let needed = 4096usize
        .checked_add(kernel_size.next_multiple_of(4096))
        .and_then(|size| size.checked_add(ramdisk_size.next_multiple_of(4096)))
        .context("boot image size overflow")?;
    ensure!(needed <= image.len(), "truncated boot image");
    Ok(())
}

/// The first `(\d+\.\d+)\.\d+-(android\d+)-(\d+)` in `bytes`, as a [`Kmi`].
///
/// Hand-rolled instead of pulling a regex engine into the payload: the pattern
/// is fixed, and the scan is the same "first match anywhere" the host verifier
/// applies.
pub fn parse_banner(bytes: &[u8]) -> Option<Kmi> {
    for start in 0..bytes.len() {
        if let Some(kmi) = banner_at(bytes, start) {
            return Some(kmi);
        }
    }
    None
}

fn banner_at(bytes: &[u8], start: usize) -> Option<Kmi> {
    let (major, at) = digits(bytes, start)?;
    if bytes.get(at) != Some(&b'.') {
        return None;
    }
    let (minor, at) = digits(bytes, at + 1)?;
    if bytes.get(at) != Some(&b'.') {
        return None;
    }
    let (_, at) = digits(bytes, at + 1)?;
    if bytes.get(at) != Some(&b'-') {
        return None;
    }
    let at = at + 1;
    if bytes.get(at..at.checked_add(7)?)? != b"android" {
        return None;
    }
    let (android, at) = digits(bytes, at + 7)?;
    if bytes.get(at) != Some(&b'-') {
        return None;
    }
    let (generation, _) = digits(bytes, at + 1)?;
    Some(Kmi {
        branch: format!("android{android}-{major}.{minor}"),
        generation,
    })
}

/// One maximal digit run starting exactly at `start`, and the offset after it.
/// A run is not accepted when a digit overflows the parse.
fn digits(bytes: &[u8], start: usize) -> Option<(u32, usize)> {
    let mut value: u32 = 0;
    let mut at = start;
    while let Some(byte) = bytes.get(at) {
        if !byte.is_ascii_digit() {
            break;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
        at += 1;
    }
    (at > start).then_some((value, at))
}

/// Reject a KMI that cannot name a module-set directory: the branch becomes one
/// path component, so it must be a safe identifier and not `.` or `..`.
pub fn validate(kmi: &Kmi) -> Result<()> {
    identifier(&kmi.branch).with_context(|| format!("invalid KMI branch {:?}", kmi.branch))?;
    ensure!(
        kmi.generation > 0 && kmi.generation < 1000,
        "invalid KMI generation {}",
        kmi.generation
    );
    Ok(())
}

/// Parse and validate a `branch`/`generation` pair from a `set.json` document.
pub fn from_parts(branch: &str, generation: u32) -> Result<Kmi> {
    let kmi = Kmi {
        branch: branch.to_owned(),
        generation,
    };
    validate(&kmi)?;
    Ok(kmi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_version_string_wins() {
        let bytes = b"junk Linux version 6.12.23-android16-6-g1234abcd (build@x) #1 SMP";
        assert_eq!(
            parse_banner(bytes).unwrap(),
            Kmi {
                branch: "android16-6.12".into(),
                generation: 6
            }
        );
    }

    #[test]
    fn only_the_full_pattern_matches() {
        for bytes in [
            b"Linux version 6.12-android16-6".as_slice(),
            b"Linux version 6.12.23-android16".as_slice(),
            b"Linux version 6.12.23-android16-".as_slice(),
            b"Linux version 6.12.23-6".as_slice(),
            b"Linux version 6.12.23-androidx-6".as_slice(),
            b"Linux version 6.12.23-android16".as_slice(),
            b"Linux version a.b.c-android16-6".as_slice(),
            b"".as_slice(),
        ] {
            assert!(
                parse_banner(bytes).is_none(),
                "{:?}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    #[test]
    fn a_longer_or_shorter_banner_still_parses() {
        assert_eq!(
            parse_banner(b"6.1.99-android14-11").unwrap(),
            Kmi {
                branch: "android14-6.1".into(),
                generation: 11
            }
        );
        assert_eq!(
            parse_banner(b"7.0.1-android99-123").unwrap(),
            Kmi {
                branch: "android99-7.0".into(),
                generation: 123
            }
        );
    }

    #[test]
    fn kmi_parts_are_validated() {
        assert!(from_parts("android16-6.12", 6).is_ok());
        for (branch, generation) in [
            ("", 6),
            ("android16/6.12", 6),
            ("android16 6.12", 6),
            ("..", 6),
            ("android16-6.12", 0),
            ("android16-6.12", 1000),
        ] {
            assert!(
                from_parts(branch, generation).is_err(),
                "{branch:?} {generation}"
            );
        }
    }

    #[test]
    fn an_unparseable_generation_is_not_a_match() {
        assert!(parse_banner(b"6.12.23-android16-99999999999").is_none());
    }

    #[test]
    fn a_non_boot_image_is_refused_before_parsing() {
        let error = kmi_from_boot(b"not a boot image").unwrap_err();
        assert_eq!(error.to_string(), "expected an Android boot image");
    }
}
