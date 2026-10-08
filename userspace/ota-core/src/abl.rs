// SPDX-License-Identifier: GPL-3.0-only
//! The `efisp` check on a stock `abl` partition.
//!
//! ROM 1's Apply switches the physical A/B slot, and the new ABL must be able
//! to reach the `efisp` partition the way the old one does; when the stock
//! LinuxLoader has no such reference, the running ABL is copied over the target
//! ABL before the switch. The check is therefore a search for one needle inside
//! the extracted LinuxLoader PE: ASCII in a path string, or UTF-16LE in a
//! firmware volume variable name.

/// Needle searched for in the LinuxLoader PE. Surfacer duplicates these five
/// bytes; there is no shared crate between the UEFI and Linux sides.
pub const EFISP: &[u8] = b"efisp";

/// Whether the LinuxLoader extracted from `partition` references `efisp`.
///
/// An image that cannot be extracted (too small, not a firmware volume, no
/// complete ARM64 EFI application) is reported as `false`: the carry-over copy
/// is the safe outcome, and a copy that was not needed costs one partition
/// write, while a missing one bricks the slot's firmware.
pub fn abl_has_efisp(partition: &[u8]) -> bool {
    match abl_image::extract_linuxloader(partition) {
        Ok(loader) => loader_has_efisp(&loader),
        Err(_) => false,
    }
}

/// Whether an already extracted LinuxLoader PE references `efisp`, as ASCII or
/// as UTF-16LE.
pub fn loader_has_efisp(loader: &[u8]) -> bool {
    let ascii = loader.windows(EFISP.len()).any(|window| window == EFISP);
    let utf16: Vec<u8> = EFISP.iter().flat_map(|byte| [*byte, 0]).collect();
    ascii || loader.windows(utf16.len()).any(|window| window == utf16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_encodings_are_found() {
        assert!(loader_has_efisp(b"prefix\0efisp\0suffix"));
        assert!(loader_has_efisp(&[
            0, 1, b'e', 0, b'f', 0, b'i', 0, b's', 0, b'p', 0, 2
        ]));
        assert!(!loader_has_efisp(b"efis"));
        assert!(!loader_has_efisp(b""));
        assert!(!loader_has_efisp(b"e\0f\0i\0s\0p")); // one byte short of UTF-16
    }

    #[test]
    fn an_extraction_failure_is_reported_as_missing() {
        assert!(!abl_has_efisp(b""));
        assert!(!abl_has_efisp(&[0u8; 128]));
    }
}
