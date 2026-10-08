//! Installed manifest and ROM admission tests for the C1 schema contract, the
//! behaviour before C1's consolidation (espinit PID-1 `esuinit/src/config.rs`).
//!
//! Every test names the contract it protects and the wrong behaviour it
//! catches.

mod fixtures;

use esu_config::{
    Backend, Error, MAX_IDENTIFIER_BYTES, MAX_PARAMS_BYTES, MAX_PARTITION_NAME_BYTES,
    MAX_PATH_BYTES, SCHEMA_VERSION, identifier, parse_backend, parse_manifest, parse_rom,
    parse_selected_rom, relative_path, rom_config_path, rom_id, validate_bootstrap,
    validate_managed, validate_manifest, validate_module_name, validate_module_params,
    validate_partition_name, validate_relative_path, validate_rom,
};

/// Parse a ROM text and validate its number, as the installed-boot path does.
fn numbered(text: &str, number: u32) -> Result<esu_config::RomConfig, Error> {
    let rom = parse_rom(text)?;
    validate_rom(&rom, number)?;
    Ok(rom)
}

#[test]
fn first_boot_number_is_bounded_optional_and_does_not_override_a_record() {
    assert_eq!(parse_rom(fixtures::ROM1).unwrap().number, None);
    let text = format!("number = 2\n{}", fixtures::ROM1);
    let rom = numbered(&text, 1).unwrap();
    assert_eq!(rom.number, Some(2));
    for value in [0, 6] {
        assert_eq!(
            parse_rom(&format!("number = {value}\n{}", fixtures::ROM1))
                .unwrap_err()
                .code,
            "RomNumberInvalid"
        );
    }
    for field in ["number = \"2\"", "number = -1", "number = 2\nnumber = 2"] {
        assert_eq!(
            parse_rom(&format!("{field}\n{}", fixtures::ROM1))
                .unwrap_err()
                .code,
            "RomParse"
        );
    }
}

/// C1 schema contract: `manifest.toml` accepts exactly the schema-1 fields, so a
/// document still carrying the removed `generation`, `[platform]` or
/// `recovery_packages` fields - or any unknown field or table - is rejected
/// instead of booting a stale configuration.
#[test]
fn manifest_rejects_unknown_and_removed_fields() {
    for field in [
        "generation = \"old\"\n",
        "recovery_packages = []\n",
        "unexpected = true\n",
        "[platform]\npackages = []\n",
    ] {
        let text = format!("{field}{}", fixtures::MANIFEST);
        assert_eq!(
            parse_manifest(&text).unwrap_err().code,
            "ManifestParse",
            "{field}"
        );

        let text = format!("{}{field}", fixtures::MANIFEST);
        assert_eq!(
            parse_manifest(&text).unwrap_err().code,
            "ManifestParse",
            "{field}"
        );
    }
}

/// C1 schema contract: `<id>.toml` accepts exactly the schema-1 fields, so the
/// removed `generation`/`rom_number` identity fallbacks and any unknown field
/// are rejected rather than accepted as a second ROM number source.
#[test]
fn rom_rejects_unknown_and_removed_fields() {
    for field in [
        "generation = \"old\"\n",
        "rom_number = 2\n",
        "unexpected = true\n",
    ] {
        let text = format!("{field}{}", fixtures::ROM1);
        assert_eq!(parse_rom(&text).unwrap_err().code, "RomParse", "{field}");

        let text = format!("{}{field}", fixtures::ROM1);
        assert_eq!(parse_rom(&text).unwrap_err().code, "RomParse", "{field}");
    }
}

/// C1 schema contract: duplicate, missing and wrongly typed fields are rejected
/// for the manifest, so a hand-edited ESP cannot silently change the boot.
#[test]
fn manifest_rejects_duplicate_missing_and_wrong_type_fields() {
    for text in [
        fixtures::MANIFEST.replace(
            "schema_version = 1",
            "schema_version = 1\nschema_version = 1",
        ),
        fixtures::MANIFEST.replace("params = \"\"\n", ""),
        fixtures::MANIFEST.replace("schema_version = 1", "schema_version = \"1\""),
        fixtures::MANIFEST.replace(
            "modules_order = [\"thin\", \"fw-views\"]",
            "modules_order = 1",
        ),
    ] {
        assert_eq!(
            parse_manifest(&text).unwrap_err().code,
            "ManifestParse",
            "{text}"
        );
    }
}

/// C1 schema contract: duplicate, missing and wrongly typed fields are rejected
/// for a ROM, so `read_only` can never default and `managed` can never be a
/// string.
#[test]
fn rom_rejects_duplicate_missing_and_wrong_type_fields() {
    for text in [
        fixtures::ROM1.replace("managed = true", "managed = true\nmanaged = false"),
        fixtures::ROM1.replace("read_only = true\n", ""),
        fixtures::ROM1.replace("id = \"rom1\"\n", ""),
        fixtures::ROM1.replace("managed = true", "managed = \"true\""),
    ] {
        assert_eq!(parse_rom(&text).unwrap_err().code, "RomParse", "{text}");
    }
}

/// C1 validation-order contract: the manifest schema version is checked before
/// structure, the ROM directory before the module lists, module-order ids
/// before the empty-module rule, the per-module name/path/params rules before
/// the duplicate rule, and the core-module rule last.
#[test]
fn manifest_validation_order_is_pinned() {
    let text = fixtures::MANIFEST
        .replace("schema_version = 1", "schema_version = 2")
        .replace("rom = \"roms\"", "rom = \"../roms\"");
    assert_eq!(
        parse_manifest(&text).unwrap_err().code,
        "ManifestSchemaVersion"
    );

    // The ROM directory precedes the module-order ids.
    let text = fixtures::MANIFEST
        .replace("rom = \"roms\"", "rom = \"../roms\"")
        .replace(
            "modules_order = [\"thin\", \"fw-views\"]",
            "modules_order = [\"thin\", \"thin\"]",
        );
    assert_eq!(parse_manifest(&text).unwrap_err().code, "PathTraversal");

    // Module-order ids precede the empty-module rule.
    let text = "schema_version = 1\nrom = \"roms\"\nmodules = []\nmodules_order = [\"a\", \"a\"]";
    assert_eq!(parse_manifest(text).unwrap_err().code, "ModuleIdDuplicate");

    // The empty-module rule precedes the core rule.
    let text = "schema_version = 1\nrom = \"roms\"\nmodules = []\nmodules_order = [\"a\"]";
    assert_eq!(
        parse_manifest(text).unwrap_err().code,
        "ManifestModulesEmpty"
    );

    // A module name precedes that module's path and the duplicate rule.
    let text = r#"
schema_version = 1
rom = "roms"
modules_order = []
[[modules]]
name = "bad/name"
path = "not-a-lib-path"
params = ""
"#;
    let error = parse_manifest(text).unwrap_err();
    assert_eq!(error.code, "ModuleNameInvalidCharacter");
    assert_eq!(error.component, "modules[0]");

    // The duplicate rule precedes the later module's path shape.
    let text = r#"
schema_version = 1
rom = "roms"
modules_order = []
[[modules]]
name = "kernelesp"
path = "lib/kernelesp.ko"
params = ""
[[modules]]
name = "kernelesp"
path = "not-a-lib-path"
params = ""
"#;
    let error = parse_manifest(text).unwrap_err();
    assert_eq!(error.code, "ManifestDuplicateModule");
    assert_eq!(error.component, "kernelesp");

    // The path shape precedes the core rule.
    let text = r#"
schema_version = 1
rom = "roms"
modules_order = []
[[modules]]
name = "gpt"
path = "lib/gpt.ko"
params = ""
[[modules]]
name = "kernelesp"
path = "lib/kernelesp.txt"
params = ""
"#;
    assert_eq!(parse_manifest(text).unwrap_err().code, "ModulePathInvalid");

    // The core rule is last.
    let text = r#"
schema_version = 1
rom = "roms"
modules_order = []
[[modules]]
name = "gpt"
path = "lib/gpt.ko"
params = ""
[[modules]]
name = "kernelesp"
path = "lib/kernelesp.ko"
params = ""
[[modules]]
name = "efivarfs"
path = "lib/efivarfs.ko"
params = "dev=by-name:bdsvars"
"#;
    let error = parse_manifest(text).unwrap_err();
    assert_eq!(error.code, "ManifestCoreFirst");
    assert_eq!(error.component, "gpt");
}

/// C1 validation-order contract for ROM admission: the schema version precedes
/// the id, the id precedes the managed/partitions pairing, the projection bound
/// precedes the partition names, a duplicate name precedes that partition's
/// backend, names and backends precede the ROM number, the ESP-file refusal
/// precedes the firmware views, and the managed rule precedes the view names.
#[test]
fn rom_validation_order_is_pinned() {
    let text = fixtures::ROM1
        .replace("schema_version = 1", "schema_version = 2")
        .replace("id = \"rom1\"", "id = \"../rom1\"");
    assert_eq!(parse_rom(&text).unwrap_err().code, "RomSchemaVersion");

    let text = fixtures::ROM1.replace("id = \"rom1\"", "id = \"../rom1\"");
    assert_eq!(parse_rom(&text).unwrap_err().code, "RomIdInvalid");

    // The managed/partitions pairing precedes the ROM number.
    let text = format!("rom_number = 9\n{}", fixtures::ROM1);
    assert_eq!(parse_rom(&text).unwrap_err().code, "RomParse");
    let empty = "schema_version = 1\nid = \"rom1\"\nmanaged = true\n";
    assert_eq!(numbered(empty, 9).unwrap_err().code, "RomPartitionsEmpty");
    let unmanaged =
        format!("{empty}[[partitions]]\nname=\"system\"\nbackend=\"/dev/loop0\"\nread_only=true\n")
            .replace("managed = true", "managed = false");
    assert_eq!(
        numbered(&unmanaged, 9).unwrap_err().code,
        "RomPartitionsNotAllowed"
    );

    // The projection bound precedes the partition names: the 129th entry is a
    // duplicate with an unusable backend.
    let mut partitions: Vec<(String, String)> = (0..128)
        .map(|index| (format!("p{index}"), "/dev/loop0".to_owned()))
        .collect();
    partitions.push(("p0".to_owned(), "not-a-backend".to_owned()));
    let text = fixtures::rom_with(
        "rom1",
        partitions
            .iter()
            .map(|(name, backend)| (name.as_str(), backend.as_str())),
    );
    assert_eq!(parse_rom(&text).unwrap_err().code, "RomPartitionsTooMany");

    // A duplicate name precedes that partition's backend spelling.
    let text = fixtures::rom_with(
        "rom1",
        [("system", "/dev/loop0"), ("system", "not-a-backend")],
    );
    let error = parse_rom(&text).unwrap_err();
    assert_eq!(error.code, "RomPartitionDuplicate");
    assert_eq!(error.component, "system");

    // The ESP-file refusal precedes the firmware views.
    let text = fixtures::rom_with("rom1", [("system", "esp-file:esu/backing.img")])
        .replace("read_only = true", "read_only = false")
        + "[[firmware_views]]\nname = \"xbl\"\nthin_id = 1\n";
    let error = numbered(&text, 1).unwrap_err();
    assert_eq!(error.code, "RomEspFileWritable");
    assert_eq!(error.component, "system");

    // The managed rule precedes the view name and thin-id rules.
    let unmanaged = "schema_version = 1\nid = \"android-b\"\nmanaged = false\n\
        [[firmware_views]]\nname = \"xbl\"\nthin_id = 1\n";
    assert_eq!(
        numbered(unmanaged, 2).unwrap_err().code,
        "RomFirmwareViewsUnmanaged"
    );

    // A view name precedes that view's thin id.
    let text = fixtures::FW_ROM.replacen(
        "name = \"xbl_a\"\nthin_id = 131073",
        "name = \"xbl\"\nthin_id = 1",
        1,
    );
    assert_eq!(numbered(&text, 2).unwrap_err().code, "RomFirmwareViewName");
}

/// C1 identity-bootstrap contract: an installed manifest must carry the fixed
/// cpio identity modules, loaded before the manifest was read.
/// The frontend/backend pair is admitted by `validate_bootstrap`, so
/// altered `kernelesp`/`efivarfs`/`efivar_store` entries are rejected rather than
/// accepting a manifest that disagrees with the running kernel.
#[test]
fn installed_manifest_identity_bootstrap_is_admitted() {
    // Bootstrap modules are not part of structural schema parsing; identity
    // admission separately rejects omissions or disagreement with running ops.
    let without = parse_manifest(fixtures::MANIFEST_NO_EFIVARFS).unwrap();
    let missing = validate_bootstrap(&without).unwrap_err();
    assert_eq!(missing.code, "IdentityModuleMissing");
    assert_eq!(missing.component, "efivarfs");

    let manifest = parse_manifest(fixtures::MANIFEST).unwrap();
    validate_bootstrap(&manifest).unwrap();
    let mut without_backend = manifest.clone();
    without_backend
        .modules
        .retain(|entry| entry.name != "efivar_store");
    let error = validate_bootstrap(&without_backend).unwrap_err();
    assert_eq!(error.code, "IdentityModuleMissing");
    assert_eq!(error.component, "efivar_store");
    let mut altered_frontend = manifest.clone();
    altered_frontend
        .modules
        .iter_mut()
        .find(|entry| entry.name == "efivarfs")
        .unwrap()
        .params = "dev=by-name:bdsvars".into();
    let error = validate_bootstrap(&altered_frontend).unwrap_err();
    assert_eq!(error.code, "IdentityModuleMismatch");
    assert_eq!(error.component, "efivarfs");

    for (entry, component) in [
        ("dev=by-name:bdsvars", "efivar_store"),
        ("lib/efivarfs.ko", "efivarfs"),
        ("lib/efivar_store.ko", "efivar_store"),
        ("lib/kernelesp.ko", "kernelesp"),
    ] {
        let replacement = match entry {
            "dev=by-name:bdsvars" => "dev=8:16",
            _ => "lib/other.ko",
        };
        let text = fixtures::MANIFEST.replace(entry, replacement);
        let manifest = parse_manifest(&text).unwrap();
        let error = validate_bootstrap(&manifest).unwrap_err();
        assert_eq!(error.code, "IdentityModuleMismatch", "{entry}");
        assert_eq!(error.component, component, "{entry}");
    }
}

/// C1 identity contract: the ROM file path is the manifest's ROM directory plus
/// the selected id, and only identifiers of at most 59 bytes may name a ROM, so
/// a traversal or over-long id can never reach a file read.
#[test]
fn rom_identity_selects_the_manifest_directory_and_an_identifier() {
    let manifest = parse_manifest(fixtures::MANIFEST).unwrap();

    for id in ["android-a", "android.b_2", "recovery", &"x".repeat(59)] {
        assert!(rom_id(id), "{id}");
        assert_eq!(rom_config_path(&manifest, id), format!("roms/{id}.toml"));
    }

    for id in ["", ".", "..", "../a", "a/b", "é", "a b", &"x".repeat(60)] {
        assert!(!rom_id(id), "{id}");
    }

    parse_selected_rom(fixtures::ROM1, "rom1").unwrap();
    assert_eq!(
        parse_selected_rom(fixtures::ROM1, "other")
            .unwrap_err()
            .code,
        "RomIdMismatch"
    );
    assert_eq!(
        parse_selected_rom(fixtures::ROM1, "../rom1")
            .unwrap_err()
            .code,
        "RomIdInvalid"
    );
}

/// C1 parsing contract: a valid managed configuration keeps the module order,
/// the params and the projection exactly as written, so admission never rewrites
/// what the payload declares.
#[test]
fn valid_managed_configuration_preserves_order_and_projection() {
    let manifest = parse_manifest(fixtures::MANIFEST).unwrap();
    let rom = parse_rom(fixtures::ROM1).unwrap();
    validate_managed(&manifest, &rom).unwrap();

    assert_eq!(manifest.rom, "roms");
    assert_eq!(
        manifest
            .modules
            .iter()
            .map(|module| module.name.as_str())
            .collect::<Vec<_>>(),
        ["kernelesp", "gpt", "efivarfs", "efivar_store"]
    );
    assert_eq!(manifest.modules[1].params, "debug=0");
    assert_eq!(rom.partitions[0].backend, "/dev/block/by-name/system");
    assert!(rom.partitions[0].read_only);
    assert_eq!(manifest.schema_version, SCHEMA_VERSION);
}

/// C1 manifest contract: the schema version, the core-module rule and the unique
/// module names are all enforced, so a reordered or duplicated module list is
/// never executed.
#[test]
fn manifest_enforces_schema_core_order_and_unique_names() {
    let text = fixtures::MANIFEST.replace("schema_version = 1", "schema_version = 2");
    assert_eq!(
        parse_manifest(&text).unwrap_err().code,
        "ManifestSchemaVersion"
    );

    let text = fixtures::MANIFEST
        .replace("name = \"kernelesp\"", "name = \"other\"")
        .replace("lib/kernelesp.ko", "lib/other.ko");
    let error = parse_manifest(&text).unwrap_err();
    assert_eq!(error.code, "ManifestCoreFirst");
    assert_eq!(error.component, "other");

    let text = fixtures::MANIFEST
        .replace("name = \"gpt\"", "name = \"kernelesp\"")
        .replace("lib/gpt.ko", "lib/kernelesp.ko");
    let error = parse_manifest(&text).unwrap_err();
    assert_eq!(error.code, "ManifestDuplicateModule");
    assert_eq!(error.component, "kernelesp");

    let text = "schema_version = 1\nrom = \"roms\"\nmodules = []\nmodules_order = []";
    assert_eq!(
        parse_manifest(text).unwrap_err().code,
        "ManifestModulesEmpty"
    );
}

/// C1 managed-ROM contract: a managed ROM requires `gpt` after the core module,
/// an unmanaged ROM must not load `gpt` at all, and `managed = true` requires a
/// nonempty partition list, so a projection can never run with no contract or an
/// unmanaged ROM with one.
#[test]
fn managed_requires_gpt_and_partitions_without_unmanaged_fallback() {
    let manifest = parse_manifest(&fixtures::manifest_without_gpt()).unwrap();
    let rom = parse_rom(fixtures::ROM1).unwrap();

    assert_eq!(
        validate_managed(&manifest, &rom).unwrap_err().code,
        "ManifestManagedGptMissing"
    );

    let prefix = fixtures::ROM1.split("[[partitions]]").next().unwrap();
    for text in [prefix.to_owned(), format!("{prefix}partitions = []\n")] {
        assert_eq!(parse_rom(&text).unwrap_err().code, "RomPartitionsEmpty");
    }

    assert_eq!(
        parse_rom(&fixtures::ROM1.replace("managed = true", "managed = false"))
            .unwrap_err()
            .code,
        "RomPartitionsNotAllowed"
    );

    let unmanaged = parse_rom(&prefix.replace("managed = true", "managed = false")).unwrap();
    assert!(!unmanaged.managed);
    assert!(unmanaged.partitions.is_empty());

    // An unmanaged ROM with a manifest that loads `gpt` is refused: there is no
    // projection contract to apply.
    let error =
        validate_managed(&parse_manifest(fixtures::MANIFEST).unwrap(), &unmanaged).unwrap_err();
    assert_eq!(error.code, "ManifestUnmanagedGpt");
    assert_eq!(error.component, "gpt");

    // The same unmanaged ROM with an unmanaged manifest is valid.
    let plain = parse_manifest(&fixtures::manifest_without_gpt()).unwrap();
    validate_managed(&plain, &unmanaged).unwrap();
}

/// C1 managed-ROM contract: `gpt` placed before the core module is rejected,
/// because the projection would run before the identity modules resolved the
/// device.
#[test]
fn managed_rom_rejects_gpt_placed_before_the_core_module() {
    let manifest = esu_config::Manifest {
        schema_version: SCHEMA_VERSION,
        rom: "roms".to_owned(),
        modules_order: vec![],
        modules: vec![
            esu_config::ModuleEntry {
                name: "gpt".to_owned(),
                path: "lib/gpt.ko".to_owned(),
                params: String::new(),
            },
            esu_config::ModuleEntry {
                name: "kernelesp".to_owned(),
                path: "lib/kernelesp.ko".to_owned(),
                params: String::new(),
            },
        ],
    };
    let rom = parse_rom(fixtures::ROM1).unwrap();

    let error = validate_managed(&manifest, &rom).unwrap_err();
    assert_eq!(error.code, "ManifestManagedGptOrder");
    assert_eq!(error.component, "gpt");

    // The same manifest is rejected earlier by the ordered-manifest rule.
    assert_eq!(
        validate_manifest(&manifest).unwrap_err().code,
        "ManifestCoreFirst"
    );
}

/// C1 ROM contract: the schema version and unique projection names are mandatory
/// for a ROM, so a duplicate projected name can never hide a second device.
#[test]
fn rom_rejects_schema_and_duplicate_partition_names() {
    assert_eq!(
        parse_rom(&fixtures::ROM1.replace("schema_version = 1", "schema_version = 0"))
            .unwrap_err()
            .code,
        "RomSchemaVersion"
    );

    let text = fixtures::rom_with(
        "rom1",
        [
            ("system", "/dev/block/by-name/system"),
            ("system", "/dev/loop0"),
        ],
    );
    let error = parse_rom(&text).unwrap_err();
    assert_eq!(error.code, "RomPartitionDuplicate");
    assert_eq!(error.component, "system");
}

/// C1 backend contract: exactly the documented backend spellings are
/// accepted and classified, so a whole logical unit, an arbitrary path, an
/// offset or a traversal spelling can never be projected.
#[test]
fn rom_backends_accept_documented_forms_and_reject_other_devices() {
    for backend in [
        "/dev/block/by-name/system",
        "/dev/loop12",
        "/dev/mapper/lv-system",
        "esp-file:esu/backing.img",
        "rom-image:boot",
        "rom-image:vendor_kernel_boot",
        "rom-image:vbmeta_vendor",
    ] {
        let text = fixtures::rom_with("rom1", [("system", backend)]);
        parse_rom(&text).unwrap();
    }

    for (backend, code) in [
        ("/dev/block/sda", "BackendUnsupportedLocation"),
        ("/dev/block/by-name", "BackendUnsupportedLocation"),
        ("/dev/block/by-name/../system", "BackendUnsupportedLocation"),
        (
            "/dev/block/by-name/system/extra",
            "BackendUnsupportedLocation",
        ),
        ("/dev/mapper/", "BackendUnsupportedLocation"),
        ("/dev/mapper/a/b", "BackendUnsupportedLocation"),
        ("/dev/loop", "BackendUnsupportedLocation"),
        ("/dev/loop0x", "BackendUnsupportedLocation"),
        ("esp-file:", "BackendUnsupportedLocation"),
        ("esp-file:/absolute", "BackendUnsupportedLocation"),
        ("esp-file:../escape", "BackendUnsupportedLocation"),
        ("esp-file:a//b", "BackendUnsupportedLocation"),
        ("esp-file:a/", "BackendUnsupportedLocation"),
        ("esp-file:.", "BackendUnsupportedLocation"),
        ("esp-file:esu/backing img", "BackendUnsupportedLocation"),
        ("relative/backend", "BackendNotAbsolute"),
        ("file:esu/backing.img", "BackendNotAbsolute"),
        ("rom-image-boot", "BackendNotAbsolute"),
    ] {
        let text = fixtures::rom_with("rom1", [("system", backend)]);
        let error = parse_rom(&text).unwrap_err();
        assert_eq!(error.code, code, "{backend}");
        assert_eq!(error.component, "system", "{backend}");
    }

    // An unknown `rom-image:` base is attributed to the base it named, not to
    // the partition that carried it.
    for (backend, component) in [
        ("rom-image:xbl", "xbl"),
        ("rom-image:boot_a", "boot_a"),
        ("rom-image:BOOT", "BOOT"),
        ("rom-image:system", "system"),
    ] {
        let text = fixtures::rom_with("rom1", [("system", backend)]);
        let error = parse_rom(&text).unwrap_err();
        assert_eq!(error.code, "BackendRomImageBase", "{backend}");
        assert_eq!(error.component, component, "{backend}");
    }
}

/// C1 backend contract: a `rom-image:` value must name one of the image bases,
/// so a typo can never name a replacement the executor would not accept. The
/// base is carried in the error component and the empty spelling stays
/// component-less.
#[test]
fn rom_image_backends_name_a_declared_base() {
    let entry = |backend: &str| esu_config::PartitionEntry {
        name: "boot_a".to_owned(),
        backend: backend.to_owned(),
        read_only: false,
        metadata: None,
    };

    for base in esu_config::IMAGE_BASES {
        assert_eq!(
            entry(&format!("rom-image:{base}")).backend().unwrap(),
            Backend::RomImage(base),
            "{base}"
        );
    }

    let error = entry("rom-image:xbl").backend().unwrap_err();
    assert_eq!(error.code, "BackendRomImageBase");
    assert_eq!(error.component, "xbl");

    let error = entry("rom-image:").backend().unwrap_err();
    assert_eq!(error.code, "BackendRomImageBase");
    assert_eq!(error.component, "");
}

/// C1 backend contract: the classification carries the parsed name or index, not
/// just an acceptance, so consumers project the same device the config named.
#[test]
fn backend_spellings_are_classified() {
    let entry = |backend: &str| esu_config::PartitionEntry {
        name: "system".to_owned(),
        backend: backend.to_owned(),
        read_only: true,
        metadata: None,
    };

    assert_eq!(
        entry("/dev/block/by-name/system").backend().unwrap(),
        Backend::ByName("system")
    );
    assert_eq!(
        entry("/dev/mapper/lv-system").backend().unwrap(),
        Backend::Mapper("lv-system")
    );
    assert_eq!(entry("/dev/loop7").backend().unwrap(), Backend::Loop(7));
    assert_eq!(
        entry("esp-file:esu/backing.img").backend().unwrap(),
        Backend::EspFile("esu/backing.img")
    );
    // A loop index that cannot name a device is refused, not truncated.
    assert_eq!(
        entry("/dev/loop4294967296").backend().unwrap_err().code,
        "BackendUnsupportedLocation"
    );
    // NUL and control bytes are refused before any spelling is considered.
    assert_eq!(
        entry("esp-file:esu/back\ning.img")
            .backend()
            .unwrap_err()
            .code,
        "BackendUnsupportedLocation"
    );
    assert_eq!(
        entry("/dev/loop0\0").backend().unwrap_err().code,
        "BackendInvalidCharacter"
    );
}

/// C1 ESP-file contract: a ROM 1 projection may read an ESP file but never write
/// one, because only a managed ROM `>= 2` holds the ESP read-write; later ROMs
/// may project a writable file.
#[test]
fn esp_file_backends_are_read_only_on_rom_one_and_writable_later() {
    let writable = fixtures::ROM1
        .replace("/dev/block/by-name/system", "esp-file:esu/backing.img")
        .replace("read_only = true", "read_only = false");

    let error = numbered(&writable, 1).unwrap_err();
    assert_eq!(error.code, "RomEspFileWritable");
    assert_eq!(error.component, "system");

    let rom = numbered(&writable, 2).unwrap();
    assert!(!rom.partitions[0].read_only);
    assert_eq!(
        rom.partitions[0].backend().unwrap(),
        Backend::EspFile("esu/backing.img")
    );

    let read_only = fixtures::ROM1.replace("/dev/block/by-name/system", "esp-file:esu/backing.img");
    assert!(numbered(&read_only, 1).is_ok());
}

/// C1 firmware-view contract: views exist only on a managed ROM `>= 2`, so a
/// ROM 1 or unmanaged boot can never publish a thin device for a firmware
/// partition, and a number outside 1..=5 is refused before any view.
#[test]
fn firmware_views_require_a_managed_rom_from_two_on() {
    let rom = numbered(fixtures::FW_ROM, 2).unwrap();
    assert_eq!(rom.firmware_views[0].thin_id, 131_073);
    assert_eq!(
        numbered(fixtures::FW_ROM, 1).unwrap_err().code,
        "RomFirmwareViewsRomNumber"
    );

    let unmanaged = "schema_version = 1\nid = \"android-b\"\nmanaged = false\n\
        [[firmware_views]]\nname = \"xbl_a\"\nthin_id = 131073\n";
    assert_eq!(
        numbered(unmanaged, 2).unwrap_err().code,
        "RomFirmwareViewsUnmanaged"
    );
    assert_eq!(
        numbered(fixtures::FW_ROM, 6).unwrap_err().code,
        "RomNumberInvalid"
    );
}

/// C1 firmware-view contract: a view names a physical `<base>_a`/`<base>_b`
/// PARTNAME whose base the running kernel does not select, carries the reserved
/// `(rom_number << 16) | index` thin id, and appears once.
#[test]
fn firmware_view_names_and_reserved_thin_ids_are_pinned() {
    for name in ["xbl", "xbl_c", "boot_a", "vbmeta_vendor_b"] {
        let text = fixtures::FW_ROM.replacen(
            "name = \"xbl_a\"\nthin_id",
            &format!("name = {name:?}\nthin_id"),
            1,
        );
        let error = numbered(&text, 2).unwrap_err();
        assert_eq!(error.code, "RomFirmwareViewName", "{name}");
        assert_eq!(error.component, name);
    }

    for id in ["0", "1", "131074", "16777216"] {
        let text = fixtures::FW_ROM.replacen("thin_id = 131073", &format!("thin_id = {id}"), 1);
        let error = numbered(&text, 2).unwrap_err();
        assert_eq!(error.code, "RomFirmwareViewThinId", "{id}");
        assert_eq!(error.component, "xbl_a");
    }

    let duplicate = fixtures::FW_ROM.replace(
        "[[partitions]]",
        "[[firmware_views]]\nname = \"xbl_a\"\nthin_id = 131074\n\n[[partitions]]",
    );
    let error = numbered(&duplicate, 2).unwrap_err();
    assert_eq!(error.code, "RomFirmwareViewDuplicate");
    assert_eq!(error.component, "xbl_a");
}

/// C1 firmware-view contract: every view needs its own writable projection at
/// `/dev/mapper/rom<N>-fw-<name>`, numbered by list position, so a view can
/// never silently project the wrong device or a read-only one.
#[test]
fn every_firmware_view_needs_its_own_writable_projection() {
    let prefix = fixtures::FW_ROM.split("[[partitions]]").next().unwrap();
    let other = "[[partitions]]\nname = \"system\"\nbackend = \"/dev/block/by-name/system\"\nread_only = false\n";

    for text in [
        format!("{prefix}{other}"),
        format!(
            "{prefix}[[partitions]]\nname = \"xbl_a\"\nbackend = \"/dev/block/by-name/xbl_a\"\nread_only = false\n"
        ),
        fixtures::FW_ROM.replace("read_only = false", "read_only = true"),
    ] {
        let error = numbered(&text, 2).unwrap_err();
        assert_eq!(error.code, "RomFirmwareViewProjection");
        assert_eq!(error.component, "xbl_a");
    }

    let both = fixtures::FW_ROM
        .replace(
            "[[partitions]]",
            "[[firmware_views]]\nname = \"tz_a\"\nthin_id = 131074\n\n[[partitions]]",
        )
        .replace(
            "read_only = false\n",
            "read_only = false\n\n[[partitions]]\nname = \"tz_a\"\nbackend = \"/dev/mapper/rom2-fw-tz_a\"\nread_only = false\n",
        );
    let rom = numbered(&both, 2).unwrap();
    assert_eq!(
        rom.firmware_views
            .iter()
            .map(|view| (view.name.as_str(), view.thin_id))
            .collect::<Vec<_>>(),
        [("xbl_a", 131_073), ("tz_a", 131_074)]
    );

    let rom = numbered(
        &fixtures::FW_ROM
            .replacen("thin_id = 131073", "thin_id = 327681", 1)
            .replacen("/dev/mapper/rom2-fw-xbl_a", "/dev/mapper/rom5-fw-xbl_a", 1),
        5,
    )
    .unwrap();
    assert_eq!(rom.firmware_views[0].thin_id, 327_681);
}

/// C1 projection-bound contract: the `gpt` ABI carries 128 projections, so the
/// next one is refused before it could be silently dropped.
#[test]
fn projections_are_bounded_by_the_gpt_abi() {
    let mut partitions: Vec<(String, String)> = (0..128)
        .map(|index| (format!("p{index}"), format!("/dev/loop{index}")))
        .collect();

    let text = fixtures::rom_with(
        "rom1",
        partitions
            .iter()
            .map(|(name, backend)| (name.as_str(), backend.as_str())),
    );
    parse_rom(&text).unwrap();

    partitions.push(("extra".to_owned(), "/dev/loop200".to_owned()));
    let text = fixtures::rom_with(
        "rom1",
        partitions
            .iter()
            .map(|(name, backend)| (name.as_str(), backend.as_str())),
    );
    assert_eq!(parse_rom(&text).unwrap_err().code, "RomPartitionsTooMany");
}

/// C1 path contract: manifest and module paths are relative paths rooted at the
/// ESP subtree, so an absolute path, a traversal, an empty component or a
/// trailing separator can never reach a file read.
#[test]
fn manifest_paths_reject_traversal_and_malformed_components() {
    for (path, code) in [
        ("../escape", "PathTraversal"),
        ("modules/../escape", "PathTraversal"),
        ("./module.ko", "PathTraversal"),
        ("/module.ko", "PathAbsolute"),
        ("modules//module.ko", "PathEmptyComponent"),
        ("modules/", "PathTrailingSeparator"),
        ("", "PathEmpty"),
    ] {
        for text in [
            fixtures::MANIFEST.replace("roms", path),
            fixtures::MANIFEST.replace("lib/gpt.ko", path),
        ] {
            assert_eq!(parse_manifest(&text).unwrap_err().code, code, "{path}");
        }
    }

    parse_manifest(&fixtures::MANIFEST.replace("lib/gpt.ko", "lib/gpt..ko")).unwrap();
}

/// C1 limit contract: identifiers are 1..=64 bytes, projected names 1..=36
/// bytes, module params 1 KiB, schema paths 4 KiB, and ESP file paths are
/// `/`-separated identifiers under the same bound; each limit accepts its exact
/// value and rejects the next byte.
#[test]
fn bounded_values_accept_exact_limits_and_reject_next_byte() {
    validate_module_name(&"m".repeat(MAX_IDENTIFIER_BYTES)).unwrap();
    validate_partition_name(&"p".repeat(MAX_PARTITION_NAME_BYTES)).unwrap();
    assert_eq!(
        validate_module_name(&"m".repeat(MAX_IDENTIFIER_BYTES + 1))
            .unwrap_err()
            .code,
        "ModuleNameTooLong"
    );
    assert_eq!(
        validate_partition_name(&"p".repeat(MAX_PARTITION_NAME_BYTES + 1))
            .unwrap_err()
            .code,
        "PartitionNameTooLong"
    );
    for name in ["", "bad/name", "module.ko", "é"] {
        assert!(validate_module_name(name).is_err(), "{name}");
        assert!(validate_partition_name(name).is_err(), "{name}");
    }

    assert!(identifier(&"x".repeat(MAX_IDENTIFIER_BYTES)));
    assert!(!identifier(&"x".repeat(MAX_IDENTIFIER_BYTES + 1)));
    for value in ["", ".", ".."] {
        assert!(!identifier(value), "{value}");
    }

    validate_relative_path(&"p".repeat(MAX_PATH_BYTES)).unwrap();
    assert_eq!(
        validate_relative_path(&"p".repeat(MAX_PATH_BYTES + 1))
            .unwrap_err()
            .code,
        "PathTooLong"
    );

    const BY_NAME: &str = "/dev/block/by-name/";
    assert_eq!(
        parse_backend(&format!(
            "{BY_NAME}{}",
            "p".repeat(MAX_PATH_BYTES - 1 - BY_NAME.len())
        ))
        .unwrap(),
        Backend::ByName(&"p".repeat(MAX_PATH_BYTES - 1 - BY_NAME.len()))
    );
    assert_eq!(
        parse_backend(&format!("/{}", "p".repeat(MAX_PATH_BYTES)))
            .unwrap_err()
            .code,
        "BackendPathTooLong"
    );
    assert_eq!(
        parse_backend("dev/block/system").unwrap_err().code,
        "BackendNotAbsolute"
    );
    assert_eq!(
        parse_backend("/dev/\0bad").unwrap_err().code,
        "BackendInvalidCharacter"
    );

    validate_module_params(&"x".repeat(MAX_PARAMS_BYTES)).unwrap();
    assert_eq!(
        validate_module_params(&"x".repeat(MAX_PARAMS_BYTES + 1))
            .unwrap_err()
            .code,
        "ModuleParamsTooLong"
    );
    for params in ["a\0b", "a\nb", "a\rb"] {
        assert_eq!(
            validate_module_params(params).unwrap_err().code,
            "ModuleParamsInvalidCharacter"
        );
    }
}

/// C1 ESP-file path contract: an `esp-file:` backing path is ESP-root-relative
/// and `/`-separated with bounded identifier components, so a traversal, an
/// absolute path, a space or an over-long component can never name a backing
/// file.
#[test]
fn esp_file_paths_are_bounded_identifiers() {
    assert!(relative_path("esu/backing.img"));
    assert!(relative_path(&format!(
        "{}/{}",
        "x".repeat(64),
        "y".repeat(64)
    )));

    for value in [
        "",
        "/esu/backing.img",
        "esu/",
        "esu//backing.img",
        "../esu/backing.img",
        "esu/./backing.img",
        "esu/../backing.img",
        "esu/backing img.img",
        "esu/back\\ing.img",
        "esu/.",
        "..",
    ] {
        assert!(!relative_path(value), "{value}");
    }

    assert!(!relative_path(&format!("esu/{}", "x".repeat(65))));

    let mut parts = vec!["x".repeat(63); 63];
    parts.push("x".repeat(64));
    let exact = parts.join("/");
    assert_eq!(exact.len(), MAX_PATH_BYTES);
    assert!(relative_path(&exact));
    assert!(!relative_path(&format!("{exact}/x")));
}

/// Metadata selection cannot escape the ESP or silently mutate a physical
/// partition, and only footer-bearing slotted payload images can be grafted.
#[test]
fn metadata_rejects_unsafe_paths_and_unsupported_projection_combinations() {
    let text = |name: &str, backend: &str, metadata: &str| {
        format!(
            "{}metadata = {metadata:?}\n",
            fixtures::rom_with("rom2", [(name, backend)])
        )
    };
    for metadata in [
        "",
        "/rom/a.vbmd",
        "../a.vbmd",
        "rom//a.vbmd",
        "rom/a.img",
        "rom/a\\b.vbmd",
    ] {
        let error =
            parse_rom(&text("recovery_a", "esp-file:rom/recovery.img", metadata)).unwrap_err();
        assert_eq!(error.code, "RomMetadataPath", "{metadata}");
    }
    for name in [
        "recovery",
        "_a",
        "vbmeta_a",
        "vbmeta_system_b",
        "vbmeta_vendor_a",
    ] {
        let error = parse_rom(&text(
            name,
            "esp-file:rom/recovery.img",
            "rom/recovery.vbmd",
        ))
        .unwrap_err();
        assert_eq!(error.code, "RomMetadataPartition", "{name}");
    }
    for backend in ["/dev/block/by-name/recovery_a", "/dev/loop0"] {
        let error = parse_rom(&text("recovery_a", backend, "rom/recovery.vbmd")).unwrap_err();
        assert_eq!(error.code, "RomMetadataBackend", "{backend}");
    }
    let rom = parse_rom(&text(
        "recovery_a",
        "esp-file:rom/recovery.img",
        "rom/recovery.vbmd",
    ))
    .unwrap();
    assert_eq!(
        rom.partitions[0].metadata.as_deref(),
        Some("rom/recovery.vbmd")
    );
    parse_rom(&text(
        "system_a",
        "esp-file:rom/system.img",
        "rom/system.vbmd",
    ))
    .unwrap();
    let missing = parse_rom(&fixtures::rom_with(
        "rom2",
        [("recovery_a", "esp-file:rom/recovery.img")],
    ))
    .unwrap();
    assert!(missing.partitions[0].metadata.is_none());
}

/// A mapper spelling alone does not authorize graft writes: it must be the
/// selected ROM's configured external-origin firmware view.
#[test]
fn metadata_mapper_requires_matching_rom_local_firmware_view() {
    let text = fixtures::FW_ROM.replace("xbl_a", "recovery_a").replace(
        "read_only = false",
        "read_only = false\nmetadata = \"rom/recovery.vbmd\"",
    );
    numbered(&text, 2).unwrap();
    assert_eq!(numbered(&text, 3).unwrap_err().code, "RomMetadataBackend");
    let missing = format!(
        "{}metadata = \"rom/recovery.vbmd\"\n",
        fixtures::rom_with("rom2", [("recovery_a", "/dev/mapper/rom2-fw-recovery_a")])
    );
    assert_eq!(
        numbered(&missing, 2).unwrap_err().code,
        "RomMetadataBackend"
    );
}
