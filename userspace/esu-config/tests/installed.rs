//! Installed-boot admission tests for the C1 contract: which kernel images a
//! ROM boots and where they come from.
//!
//! Every test names the contract it protects and the wrong behaviour it catches.

mod fixtures;

use esu_config::{
    InstalledConfig, KERNEL_SET_BASES, KernelImage, KernelImages, Slot, parse_installed,
};

/// The seven ESP kernel images admitted for one slot.
fn esp_images<'a>(installed: &'a InstalledConfig, slot: Slot) -> [KernelImage<'a>; 7] {
    match installed.kernel_images(slot) {
        KernelImages::Esp(images) => images,
        KernelImages::Physical => panic!("expected an ESP kernel set"),
    }
}

/// C1 installed admission: a ROM 1 boot reads the physical kernel partitions of
/// the current slot, so no `<base>_a`/`<base>_b` partition may be an ESP file
/// and the physical set is reported for both slots; a non-kernel ESP file
/// projection stays allowed.
#[test]
fn rom_one_is_physical_and_rejects_esp_file_kernel_partitions() {
    let installed = parse_installed(fixtures::MANIFEST, fixtures::ROM1, "rom1", 1).unwrap();
    assert_eq!(installed.rom_number(), 1);
    assert_eq!(installed.rom().id, "rom1");
    assert_eq!(installed.manifest().rom, "roms");
    assert_eq!(installed.kernel_images(Slot::A), KernelImages::Physical);
    assert_eq!(installed.kernel_images(Slot::B), KernelImages::Physical);

    let text = fixtures::rom_with("rom1", [("system", "esp-file:esu/rom1/system.img")]);
    let installed = parse_installed(fixtures::MANIFEST, &text, "rom1", 1).unwrap();
    assert_eq!(installed.kernel_images(Slot::A), KernelImages::Physical);

    for partition in ["boot_a", "boot_b", "vbmeta_vendor_a"] {
        let backend = format!("esp-file:esu/rom1/{partition}.img");
        let text = fixtures::rom_with("rom1", [(partition, backend.as_str())]);
        let error = parse_installed(fixtures::MANIFEST, &text, "rom1", 1).unwrap_err();
        assert_eq!(error.code, "KernelSetBackend", "{partition}");
        assert_eq!(error.component, partition, "{partition}");
    }
}

/// C1 installed admission: a ROM `>= 2` boots its own seven AVB kernel images
/// from the ESP, so all fourteen `<base>_a`/`<base>_b` partitions must be
/// admitted and `kernel_images` must report each slot in `KERNEL_SET_BASES`
/// order with the path that follows `esp-file:`.
#[test]
fn rom_two_admits_the_complete_esp_kernel_set() {
    let text = fixtures::rom2();
    let installed = parse_installed(fixtures::MANIFEST, &text, "rom2", 2).unwrap();
    assert_eq!(installed.rom_number(), 2);
    assert_eq!(installed.rom().id, "rom2");

    for slot in [Slot::A, Slot::B] {
        let images = esp_images(&installed, slot);

        assert_eq!(images.len(), KERNEL_SET_BASES.len());

        for (image, base) in images.iter().zip(KERNEL_SET_BASES) {
            let partition = format!("{base}{}", slot.suffix());
            assert_eq!(image.base, base);
            assert_eq!(image.partition, partition);
            assert_eq!(image.path, fixtures::kernel_path(&partition));
        }
    }

    assert_eq!(esp_images(&installed, Slot::A)[0].partition, "boot_a");
    assert_eq!(
        esp_images(&installed, Slot::B)[6].partition,
        "vbmeta_vendor_b"
    );
    assert_eq!(esp_images(&installed, Slot::B)[6].base, KERNEL_SET_BASES[6]);
}

/// C1 slot contract: the slot suffix, index and index lookup are the AOSP
/// conventions, so an executor decoding a slot index cannot pick the wrong
/// slot's images.
#[test]
fn slot_suffixes_and_indices_are_the_aosp_convention() {
    assert_eq!(Slot::A.suffix(), "_a");
    assert_eq!(Slot::B.suffix(), "_b");
    assert_eq!(Slot::A.index(), 0);
    assert_eq!(Slot::B.index(), 1);
    assert_eq!(Slot::from_index(0), Some(Slot::A));
    assert_eq!(Slot::from_index(1), Some(Slot::B));
    assert_eq!(Slot::from_index(2), None);
}

/// C1 installed admission: every one of the fourteen kernel partitions is
/// required, and the missing partition is named, so a partial ESP kernel set can
/// never boot with a stale image.
#[test]
fn kernel_set_requires_every_partition_of_both_slots() {
    for missing in fixtures::KERNEL_PARTITIONS {
        let text = fixtures::rom2_without(missing);
        let error = parse_installed(fixtures::MANIFEST, &text, "rom2", 2).unwrap_err();
        assert_eq!(error.code, "KernelSetIncomplete", "{missing}");
        assert_eq!(error.component, missing, "{missing}");
    }
}

/// C1 installed admission: every kernel partition of a ROM `>= 2` must be an
/// `esp-file:` image, because the executor reads the AVB images from the managed
/// ESP and never from a physical partition.
#[test]
fn kernel_set_partitions_must_be_esp_files() {
    for (partition, backend) in [
        ("boot_a", "/dev/block/by-name/boot_a"),
        ("vbmeta_system_b", "/dev/mapper/rom2-fw-vbmeta_system_b"),
    ] {
        let text = fixtures::rom2_replacing(partition, backend);
        let error = parse_installed(fixtures::MANIFEST, &text, "rom2", 2).unwrap_err();
        assert_eq!(error.code, "KernelSetBackend", "{partition}");
        assert_eq!(error.component, partition, "{partition}");
    }
}

/// C1 installed admission: the ESP is FAT, which UEFI and vfat resolve ASCII
/// case-insensitively, so two kernel paths differing only by case alias one
/// image file and must be rejected as duplicates, naming the later partition.
/// Case alone never fabricates a duplicate: a case-only variant of a kernel's
/// own (replaced) path stays admitted when no other kernel path matches it.
#[test]
fn kernel_set_paths_are_compared_case_insensitively() {
    for (partition, aliased, aliasing) in [
        ("boot_b", "boot_a", "ESU/ROM2/BOOT_A.IMG"),
        (
            "vbmeta_vendor_b",
            "vbmeta_vendor_a",
            "Esu/Rom2/Vbmeta_Vendor_A.img",
        ),
    ] {
        let aliased = fixtures::kernel_path(aliased);
        assert!(
            aliased.eq_ignore_ascii_case(aliasing),
            "{aliased} vs {aliasing}"
        );

        let text = fixtures::rom2_replacing_path(partition, aliasing);
        let error = parse_installed(fixtures::MANIFEST, &text, "rom2", 2).unwrap_err();
        assert_eq!(error.code, "KernelSetDuplicatePath", "{partition}");
        assert_eq!(error.component, partition, "{partition}");
    }

    let text = fixtures::rom2_replacing_path("boot_b", "ESU/ROM2/Boot_B.img");
    let installed = parse_installed(fixtures::MANIFEST, &text, "rom2", 2).unwrap();
    assert_eq!(installed.rom_number(), 2);
}

/// C1 installed admission: the fourteen kernel image paths must be distinct, so
/// a ROM can never alias two kernel bases (or both slots of one base) to a
/// single image file. The rejection names the partition that repeats a path an
/// earlier kernel partition already admitted.
#[test]
fn kernel_set_paths_must_be_distinct() {
    for (partition, aliased) in [
        ("boot_b", "boot_a"),
        ("vbmeta_vendor_b", "init_boot_a"),
        ("dtbo_b", "dtbo_a"),
    ] {
        let path = fixtures::kernel_path(aliased);
        let text = fixtures::rom2_replacing_path(partition, &path);
        let error = parse_installed(fixtures::MANIFEST, &text, "rom2", 2).unwrap_err();
        assert_eq!(error.code, "KernelSetDuplicatePath", "{partition}");
        assert_eq!(error.component, partition, "{partition}");
    }
}

/// C1 installed admission: the installed manifest must agree with the cpio
/// identity pair the loader already inserted, so firmware refuses at admission
/// time exactly what esu PID 1 refuses at boot; a missing or altered
/// `kernelesp`/`efivarfs` entry is named.
#[test]
fn installed_admission_refuses_a_manifest_without_the_identity_pair() {
    let missing =
        parse_installed(fixtures::MANIFEST_NO_EFIVARFS, fixtures::ROM1, "rom1", 1).unwrap_err();
    assert_eq!(missing.code, "IdentityModuleMissing");
    assert_eq!(missing.component, "efivarfs");

    for (entry, replacement, component) in [
        ("dev=by-name:bdsvars", "dev=8:16", "efivarfs"),
        ("lib/kernelesp.ko", "lib/other.ko", "kernelesp"),
    ] {
        let text = fixtures::MANIFEST.replace(entry, replacement);
        let error = parse_installed(&text, fixtures::ROM1, "rom1", 1).unwrap_err();
        assert_eq!(error.code, "IdentityModuleMismatch", "{entry}");
        assert_eq!(error.component, component, "{entry}");
    }
}

/// C1 installed admission order: the manifest, its cpio identity pair, the
/// selected ROM text and id, the ROM number, the managed module rule and only
/// then the kernel set decide the rejection, so a stale or mismatched
/// configuration is reported by its real cause.
#[test]
fn installed_admission_reports_the_first_failure_in_order() {
    let error =
        parse_installed(fixtures::MANIFEST_NO_EFIVARFS, fixtures::ROM1, "rom1", 1).unwrap_err();
    assert_eq!(error.code, "IdentityModuleMissing");

    let error = parse_installed(fixtures::MANIFEST, "not toml", "rom1", 1).unwrap_err();
    assert_eq!(error.code, "RomParse");

    let error = parse_installed(fixtures::MANIFEST, fixtures::ROM1, "rom2", 1).unwrap_err();
    assert_eq!(error.code, "RomIdMismatch");

    for number in [0, 6] {
        let error =
            parse_installed(fixtures::MANIFEST, fixtures::ROM1, "rom1", number).unwrap_err();
        assert_eq!(error.code, "RomNumberInvalid", "{number}");
    }

    let no_gpt = fixtures::manifest_without_gpt();
    let error = parse_installed(&no_gpt, &fixtures::rom2(), "rom2", 2).unwrap_err();
    assert_eq!(error.code, "ManifestManagedGptMissing");

    let text = fixtures::rom2_without("dtbo_a");
    let error = parse_installed(fixtures::MANIFEST, &text, "rom2", 2).unwrap_err();
    assert_eq!(error.code, "KernelSetIncomplete");
    assert_eq!(error.component, "dtbo_a");
}
