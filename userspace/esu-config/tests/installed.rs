//! Installed-boot admission tests for the C1 contract: which base images a ROM
//! boots and where they come from.
//!
//! Every test names the contract it protects and the wrong behaviour it catches.

mod fixtures;

use esu_config::{IMAGE_BASES, InstalledConfig, KernelImage, KernelImages, Slot, parse_installed};

/// The ESP base images admitted for ROM 2's declared bases.
fn esp_images(installed: &InstalledConfig) -> Vec<KernelImage> {
    match installed.kernel_images() {
        KernelImages::Esp(images) => images,
        KernelImages::Physical => panic!("expected an ESP image set"),
    }
}

/// C1 installed admission: a ROM 1 boot reads the physical kernel partitions of
/// the booted slot, so no partition may carry an image backend — an
/// `esp-file:` image partition or a `rom-image:` role — and the physical set is
/// reported. A non-image read-only ESP-file projection stays allowed.
#[test]
fn rom_one_is_physical_and_rejects_image_roles() {
    let installed = parse_installed(fixtures::MANIFEST, fixtures::ROM1, "rom1", 1).unwrap();
    assert_eq!(installed.rom_number(), 1);
    assert_eq!(installed.rom().id, "rom1");
    assert_eq!(installed.manifest().rom, "roms");
    assert_eq!(installed.kernel_images(), KernelImages::Physical);

    let text = fixtures::rom_with("rom1", [("system", "esp-file:esu/rom1/system.img")]);
    let installed = parse_installed(fixtures::MANIFEST, &text, "rom1", 1).unwrap();
    assert_eq!(installed.kernel_images(), KernelImages::Physical);

    for partition in [
        "boot_a",
        "boot_b",
        "vbmeta_vendor_a",
        "vendor_kernel_boot_b",
        "pvmfw_a",
    ] {
        let backend = format!("esp-file:esu/rom1/{partition}.img");
        let text = fixtures::rom_with("rom1", [(partition, backend.as_str())]);
        let error = parse_installed(fixtures::MANIFEST, &text, "rom1", 1).unwrap_err();
        assert_eq!(error.code, "KernelSetBackend", "{partition}");
        assert_eq!(error.component, partition, "{partition}");
    }

    // A base role is refused wherever it appears, including on a partition
    // whose name carries no slot suffix.
    for (partition, backend) in [
        ("boot_a", "rom-image:boot"),
        ("vbmeta_b", "rom-image:vbmeta"),
        ("boot", "rom-image:boot"),
    ] {
        let text = fixtures::rom_with("rom1", [(partition, backend)]);
        let error = parse_installed(fixtures::MANIFEST, &text, "rom1", 1).unwrap_err();
        assert_eq!(error.code, "KernelSetBackend", "{backend}");
        assert_eq!(error.component, partition, "{backend}");
    }
}

/// C1 installed admission: a ROM `>= 2` boots its own base images from the ESP,
/// so every declared base is admitted as both letters of one `rom-image:` role
/// and `kernel_images` reports one base-named file per base in [`IMAGE_BASES`]
/// order.
#[test]
fn rom_two_admits_the_declared_base_image_set() {
    let text = fixtures::rom2();
    let installed = parse_installed(fixtures::MANIFEST, &text, "rom2", 2).unwrap();
    assert_eq!(installed.rom_number(), 2);
    assert_eq!(installed.rom().id, "rom2");

    let images = esp_images(&installed);
    assert_eq!(images.len(), fixtures::DECLARED_BASES.len());

    for (image, base) in images.iter().zip(fixtures::DECLARED_BASES) {
        assert_eq!(image.base, base);
        assert_eq!(image.path, fixtures::image_path(base));
    }

    // The declared subset is expanded in IMAGE_BASES order, never declaration
    // order: vbmeta follows boot there.
    let declared: Vec<&str> = IMAGE_BASES
        .into_iter()
        .filter(|base| fixtures::DECLARED_BASES.contains(base))
        .collect();
    assert_eq!(
        images.iter().map(|image| image.base).collect::<Vec<_>>(),
        declared
    );

    // A base outside the declared subset is absent, and one base file serves
    // both letters by construction: the same path is reported once.
    assert!(!images.iter().any(|image| image.base == "dtbo"));
    assert_eq!(
        images
            .iter()
            .filter(|image| image.base == "boot")
            .map(|image| image.path.as_str())
            .collect::<Vec<_>>(),
        [fixtures::image_path("boot").as_str()]
    );
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

/// C1 installed admission: a declared base requires both of its slot
/// partitions, and the missing one is named, so a half-admitted base can never
/// boot with a stale letter.
#[test]
fn declared_bases_require_both_letters() {
    for missing in fixtures::IMAGE_PARTITIONS {
        let text = fixtures::rom2_without(missing);
        let error = parse_installed(fixtures::MANIFEST, &text, "rom2", 2).unwrap_err();
        assert_eq!(error.code, "KernelSetIncomplete", "{missing}");
        assert_eq!(error.component, missing, "{missing}");
    }
}

/// C1 installed admission: every declared image partition must carry its base's
/// `rom-image:` role, because the executor resolves the base file and never
/// reads an arbitrary physical partition or ESP file for a base.
#[test]
fn image_partitions_must_carry_their_base_role() {
    for (partition, backend, code) in [
        ("boot_a", "/dev/block/by-name/boot_a", "KernelSetBackend"),
        (
            "vbmeta_b",
            "/dev/mapper/rom2-fw-vbmeta_b",
            "KernelSetBackend",
        ),
        ("boot_b", "esp-file:rom/rom2/boot.img", "KernelSetBackend"),
        ("vbmeta_a", "rom-image:dtbo", "KernelSetBackend"),
        ("vbmeta_b", "rom-image:pvmfw", "KernelSetBackend"),
    ] {
        let text = fixtures::rom2_replacing(partition, backend);
        let error = parse_installed(fixtures::MANIFEST, &text, "rom2", 2).unwrap_err();
        assert_eq!(error.code, code, "{partition} = {backend}");
        assert_eq!(error.component, partition, "{partition} = {backend}");
    }
}

/// C1 installed admission: a declared base is written in place by promote and
/// stays writable, so a read-only image projection is refused and the partition
/// is named.
#[test]
fn image_partitions_must_be_writable() {
    let text = fixtures::rom2().replace(
        "name = \"boot_a\"\nbackend = \"rom-image:boot\"\nread_only = false",
        "name = \"boot_a\"\nbackend = \"rom-image:boot\"\nread_only = true",
    );
    assert_ne!(text, fixtures::rom2());
    let error = parse_installed(fixtures::MANIFEST, &text, "rom2", 2).unwrap_err();
    assert_eq!(error.code, "KernelSetReadOnly");
    assert_eq!(error.component, "boot_a");
}

/// C1 installed admission: a ROM `>= 2` without a single declared base has no
/// image to boot and is refused as empty, naming no component.
#[test]
fn rom_two_without_a_declared_base_is_refused() {
    let text = fixtures::rom_with("rom2", [("metadata", "/dev/mapper/rom-metadata_2")]);
    let error = parse_installed(fixtures::MANIFEST, &text, "rom2", 2).unwrap_err();
    assert_eq!(error.code, "KernelSetEmpty");
    assert_eq!(error.component, "");
}

/// C1 installed admission: an unknown image base is a schema error wherever it
/// appears, so a typo can never name a role the executor cannot serve.
#[test]
fn unknown_image_bases_are_refused() {
    for (partition, backend) in [
        ("system_a", "rom-image:xbl"),
        ("boot_a", "rom-image:boot_a"),
        ("boot_a", "rom-image:"),
    ] {
        let text = fixtures::rom_with("rom1", [(partition, backend)]);
        let error = parse_installed(fixtures::MANIFEST, &text, "rom1", 1).unwrap_err();
        assert_eq!(error.code, "BackendRomImageBase", "{backend}");
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
        ("dev=by-name:bdsvars", "dev=8:16", "efivar_store"),
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
/// then the image set decide the rejection, so a stale or mismatched
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

    let text = fixtures::rom2_without("vbmeta_b");
    let error = parse_installed(fixtures::MANIFEST, &text, "rom2", 2).unwrap_err();
    assert_eq!(error.code, "KernelSetIncomplete");
    assert_eq!(error.component, "vbmeta_b");
}
