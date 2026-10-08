// SPDX-License-Identifier: GPL-3.0-only
//! The KMI read on a real boot image: the banner is inside the kernel block, so
//! the probe has to parse the image and decompress the kernel exactly as the
//! bootloader would.

use ota_core::kmi::{Kmi, kmi_from_boot};

/// An Android boot image header v4 with one uncompressed kernel block.
///
/// Layout (AOSP `boot_img_hdr_v4`): magic at 0, `kernel_size` at 8,
/// `ramdisk_size` at 12, `os_version` at 16, `header_size` at 20, `reserved` at
/// 24, `header_version` at 40, `cmdline` at 44, `signature_size` at 1580; the
/// header occupies one 4096-byte page and blocks are page aligned.
fn boot_image(kernel: &[u8]) -> Vec<u8> {
    let mut image = vec![0u8; 4096];
    image[..8].copy_from_slice(b"ANDROID!");
    image[8..12].copy_from_slice(&(kernel.len() as u32).to_le_bytes());
    image[12..16].copy_from_slice(&0u32.to_le_bytes());
    image[16..20].copy_from_slice(&0u32.to_le_bytes());
    image[20..24].copy_from_slice(&1584u32.to_le_bytes());
    image[40..44].copy_from_slice(&4u32.to_le_bytes());
    image[44..44 + 8].copy_from_slice(b"console=");
    image.extend_from_slice(kernel);
    // Blocks are page aligned, so the file is at least one full page past the
    // header.
    image.resize(4096 + kernel.len().next_multiple_of(4096), 0);
    image
}

#[test]
fn the_kernel_banner_names_the_kmi() {
    let mut kernel = b"plain kernel bytes, deliberately not a compression magic ".to_vec();
    kernel.extend_from_slice(b"Linux version 6.12.23-android16-6-g1a2b3c4d (build@host) #1 SMP");
    kernel.extend_from_slice(&[0u8; 64]);

    let kmi = kmi_from_boot(&boot_image(&kernel)).unwrap();
    assert_eq!(
        kmi,
        Kmi {
            branch: "android16-6.12".into(),
            generation: 6
        }
    );
}

#[test]
fn an_image_without_a_kernel_or_a_banner_is_refused() {
    // A valid v4 header with no kernel block at all.
    let mut empty = boot_image(b"");
    assert!(kmi_from_boot(&empty).is_err());

    // A kernel block whose bytes carry no version string.
    let mut kernel = vec![0x41u8; 256];
    kernel[..4].copy_from_slice(b"junk");
    assert!(kmi_from_boot(&boot_image(&kernel)).is_err());

    // Not a boot image.
    assert!(kmi_from_boot(b"ANDROID!").is_err());

    // A truncated header is not silently read as a raw ramdisk either.
    empty.truncate(64);
    assert!(kmi_from_boot(&empty).is_err());

    // A v2 header has no KMI this build can read.
    let mut v2 = boot_image(&vec![0x41u8; 256]);
    v2[40..44].copy_from_slice(&2u32.to_le_bytes());
    let error = kmi_from_boot(&v2).unwrap_err();
    assert!(error.to_string().contains("v3/v4"), "{error}");

    // A header size that does not match its version is refused.
    let mut wrong_size = boot_image(&vec![0x41u8; 256]);
    wrong_size[20..24].copy_from_slice(&1580u32.to_le_bytes());
    let error = kmi_from_boot(&wrong_size).unwrap_err();
    assert!(error.to_string().contains("header size"), "{error}");

    // A kernel block that claims to be longer than the image is truncated.
    let mut truncated = boot_image(&vec![0x41u8; 256]);
    truncated[8..12].copy_from_slice(&8192u32.to_le_bytes());
    let error = kmi_from_boot(&truncated).unwrap_err();
    assert!(error.to_string().contains("truncated"), "{error}");
}
