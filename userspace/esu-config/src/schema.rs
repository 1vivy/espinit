//! Installed manifest and ROM schema: parsing and pure validation.
//!
//! Field names, serde attributes, validation order, limits and failure codes
//! follow espinit's PID-1 validator; Linux backend resolution deliberately does
//! not live here.

use std::collections::HashSet;

use serde::Deserialize;

use crate::{
    Error, IMAGE_BASES, MAX_IDENTIFIER_BYTES, MAX_PARAMS_BYTES, MAX_PARTITION_NAME_BYTES,
    MAX_PATH_BYTES, MAX_PROJECTIONS, MAX_ROM_NUMBER, SCHEMA_VERSION, identifier, relative_path,
    rom_id,
};

/// `manifest.toml`, parsed with unknown/duplicate/missing fields rejected.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u64,
    /// Directory of per-ROM `<id>.toml` files, relative to the payload root.
    pub rom: String,
    pub modules: Vec<ModuleEntry>,
    pub modules_order: Vec<String>,
}

/// One ordered manifest module entry.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModuleEntry {
    pub name: String,
    pub path: String,
    pub params: String,
}

/// Selected per-ROM configuration, parsed with unknown/duplicate/missing fields
/// rejected.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RomConfig {
    pub schema_version: u64,
    pub id: String,
    /// First-boot identity for firmware creating a missing Slot record. Once a
    /// record exists its number remains authoritative; old configs may omit this.
    #[serde(default)]
    pub number: Option<u32>,
    pub managed: bool,
    #[serde(default)]
    pub partitions: Vec<PartitionEntry>,
    /// Per-ROM firmware views: thin devices of the shared pool that serve a
    /// physical firmware partition's exact bytes until this ROM writes to them.
    /// Only valid on a managed ROM `>= 2`; ROM 1 and single-ROM payloads leave
    /// the list absent.
    #[serde(default)]
    pub firmware_views: Vec<FirmwareView>,
}

/// One projected partition. `read_only` is explicit; there is no default.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PartitionEntry {
    pub name: String,
    pub backend: String,
    pub read_only: bool,
    /// Standalone AVB metadata at a safe ESP-root-relative path, seeded when
    /// the backing is initialized. Subsequent OTA/current backing bytes win:
    /// ESP files are grafted at provisioning, firmware COW views when empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
}

impl PartitionEntry {
    /// Validate this partition's backend spelling and classify it. The code is
    /// `BackendNotAbsolute`, `BackendPathTooLong`, `BackendInvalidCharacter` or
    /// `BackendUnsupportedLocation`; the component stays empty until the caller
    /// attaches the partition or projection it belongs to.
    pub fn backend(&self) -> Result<Backend<'_>, Error> {
        parse_backend(&self.backend)
    }
}

/// One per-ROM firmware view. The physical partition named by `name` is served
/// by a thin device of the shared pool through the external-origin `thin`
/// table, so unwritten blocks read the physical bytes and every ROM's OTA
/// writes land in its own provisioned blocks.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FirmwareView {
    /// Physical sysfs `PARTNAME` this view shadows: `<base>_a` or `<base>_b`.
    pub name: String,
    /// Pool-unique thin id, fixed to `(rom_number << 16) | index` with the
    /// 1-based position of this view in the list, so ids are deterministic and
    /// never collide with an LVM2-owned id below `0x10000`.
    pub thin_id: u32,
}

/// Backend spelling distinctions preserved: `/dev/block/by-name/<P>`,
/// `/dev/mapper/<n>`, `/dev/loopN`, `esp-file:<rel>`, `rom-image:<base>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend<'a> {
    /// A physical partition, addressed by its unique sysfs `PARTNAME`.
    ByName(&'a str),
    /// A device-mapper name.
    Mapper(&'a str),
    /// An existing loop device index.
    Loop(u32),
    /// A preallocated regular file inside the mounted ESP, ESP-root-relative.
    EspFile(&'a str),
    /// An image base whose bytes are served by the running esu boot from
    /// `esu_config::base_image_path(rom, base)`: the current ROM's own base file
    /// for the booted letter, the staging LV of that base for the staged letter.
    RomImage(&'a str),
}

/// Documented by-name backend prefix.
const BY_NAME_PREFIX: &str = "/dev/block/by-name/";
/// Documented device-mapper backend prefix.
const MAPPER_PREFIX: &str = "/dev/mapper/";
/// Documented ESP regular-file backend prefix.
const ESP_FILE_PREFIX: &str = "esp-file:";
/// Documented image-base backend prefix.
const ROM_IMAGE_PREFIX: &str = "rom-image:";
/// Documented loop backend prefix.
const LOOP_PREFIX: &str = "/dev/loop";

/// Validate one backend path and classify its spelling.
///
/// Anything else is rejected, including whole logical units, offsets, arbitrary
/// paths, and symlink or traversal spellings; the device itself is resolved by
/// the Linux backend resolver immediately before the `gpt` entry. An
/// `rom-image:` value must name one of [`crate::IMAGE_BASES`], because no other
/// name is a replacement the executor could boot.
pub fn parse_backend(value: &str) -> Result<Backend<'_>, Error> {
    if !value.starts_with('/')
        && !value.starts_with(ESP_FILE_PREFIX)
        && !value.starts_with(ROM_IMAGE_PREFIX)
    {
        return Err(Error::new("BackendNotAbsolute"));
    }

    if value.len() > MAX_PATH_BYTES {
        return Err(Error::new("BackendPathTooLong"));
    }

    if value.bytes().any(|byte| byte == 0) {
        return Err(Error::new("BackendInvalidCharacter"));
    }

    if let Some(base) = value.strip_prefix(ROM_IMAGE_PREFIX) {
        return if IMAGE_BASES.contains(&base) {
            Ok(Backend::RomImage(base))
        } else if base.is_empty() {
            Err(Error::new("BackendRomImageBase"))
        } else {
            Err(Error::at("BackendRomImageBase", base))
        };
    }

    let classified = if let Some(label) = value.strip_prefix(BY_NAME_PREFIX) {
        name_component(label).then_some(Backend::ByName(label))
    } else if let Some(name) = value.strip_prefix(MAPPER_PREFIX) {
        name_component(name).then_some(Backend::Mapper(name))
    } else if let Some(file) = value.strip_prefix(ESP_FILE_PREFIX) {
        relative_path(file).then_some(Backend::EspFile(file))
    } else if let Some(number) = value.strip_prefix(LOOP_PREFIX) {
        loop_index(number).map(Backend::Loop)
    } else {
        None
    };

    classified.ok_or_else(|| Error::new("BackendUnsupportedLocation"))
}

/// Loop backend index: decimal, nonempty, and inside the device index space.
/// Anything larger is refused instead of being truncated to a different device.
fn loop_index(number: &str) -> Option<u32> {
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }

    number.parse().ok()
}

/// A single safe partition-name or device-mapper-name component: nonempty, not
/// `.`/`..`, no separators and no non-printable bytes.
fn name_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'/' && byte != b'\\')
}

/// Parse and validate `manifest.toml` text: strict schema and the ordered
/// module lists.
///
/// The fixed cpio bootstrap module set is admitted separately by
/// [`validate_bootstrap`]: the PID-1 loader owns that step on the device, and
/// the payload builder validates manifests whose identity modules it does not
/// select.
pub fn parse_manifest(text: &str) -> Result<Manifest, Error> {
    let manifest: Manifest = toml::from_str(text).map_err(|_| Error::new("ManifestParse"))?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// Parse and structurally validate a ROM; runtime callers validate its number
/// separately.
pub fn parse_rom(text: &str) -> Result<RomConfig, Error> {
    let rom: RomConfig = toml::from_str(text).map_err(|_| Error::new("RomParse"))?;
    validate_rom_structure(&rom)?;
    Ok(rom)
}

/// Parse a ROM and require it to be the selected one.
pub fn parse_selected_rom(text: &str, id: &str) -> Result<RomConfig, Error> {
    validate_rom_id(id)?;
    let rom = parse_rom(text)?;
    if rom.id != id {
        return Err(Error::new("RomIdMismatch"));
    }
    Ok(rom)
}

/// ESP-relative path of the selected ROM's configuration file, rooted at the
/// manifest's ROM directory. Validate the id with [`rom_id`] before using it.
pub fn rom_config_path(manifest: &Manifest, id: &str) -> String {
    format!("{}/{id}.toml", manifest.rom)
}

/// Enforce strict schema and the distinct kernel/ESP module lists.
pub fn validate_manifest(manifest: &Manifest) -> Result<(), Error> {
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(Error::new("ManifestSchemaVersion"));
    }

    validate_relative_path(&manifest.rom).map_err(|error| error.with_component("manifest.toml"))?;

    let mut ids = HashSet::new();
    for id in &manifest.modules_order {
        if !identifier(id) {
            return Err(Error::at("ModuleIdInvalid", id));
        }
        if !ids.insert(id) {
            return Err(Error::at("ModuleIdDuplicate", id));
        }
    }

    if manifest.modules.is_empty() {
        return Err(Error::new("ManifestModulesEmpty"));
    }

    let mut names: Vec<&str> = Vec::with_capacity(manifest.modules.len());

    for (index, module) in manifest.modules.iter().enumerate() {
        validate_module_name(&module.name)
            .map_err(|error| error.with_component(format!("modules[{index}]")))?;

        if names.contains(&module.name.as_str()) {
            return Err(Error::at("ManifestDuplicateModule", module.name.clone()));
        }
        names.push(&module.name);

        validate_relative_path(&module.path)
            .map_err(|error| error.with_component(module.name.clone()))?;
        if !module.path.starts_with("lib/")
            || !module.path.ends_with(".ko")
            || module.path.split('/').count() != 2
        {
            return Err(Error::at("ModulePathInvalid", &module.name));
        }

        validate_module_params(&module.params)
            .map_err(|error| error.with_component(module.name.clone()))?;
    }

    if manifest.modules[0].name != "kernelesp" {
        return Err(Error::at(
            "ManifestCoreFirst",
            manifest.modules[0].name.clone(),
        ));
    }

    Ok(())
}

/// Fixed cpio identity modules were loaded before reading an ESP manifest, so a
/// manifest that disagrees with them can never describe the running boot.
pub fn validate_bootstrap(manifest: &Manifest) -> Result<(), Error> {
    for (name, params) in [
        ("kernelesp", ""),
        ("efivarfs", ""),
        ("efivar_store", "dev=by-name:bdsvars"),
    ] {
        let entry = manifest
            .modules
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| Error::at("IdentityModuleMissing", name))?;
        if entry.path != format!("lib/{name}.ko") || entry.params != params {
            return Err(Error::at("IdentityModuleMismatch", name));
        }
    }
    Ok(())
}

/// Validate ROM identity-dependent projection rules with the bdsvars number.
pub fn validate_rom(rom: &RomConfig, rom_number: u32) -> Result<(), Error> {
    validate_rom_structure(rom)?;

    if !(1..=MAX_ROM_NUMBER).contains(&rom_number) {
        return Err(Error::new("RomNumberInvalid"));
    }

    for partition in &rom.partitions {
        if matches!(partition.backend(), Ok(Backend::EspFile(_)))
            && !partition.read_only
            && rom_number < 2
        {
            return Err(Error::at("RomEspFileWritable", &partition.name));
        }
        if partition.metadata.is_some()
            && matches!(partition.backend(), Ok(Backend::Mapper(_)))
            && (partition.backend != format!("/dev/mapper/rom{rom_number}-fw-{}", partition.name)
                || !rom
                    .firmware_views
                    .iter()
                    .any(|view| view.name == partition.name))
        {
            return Err(Error::at("RomMetadataBackend", &partition.name));
        }
    }

    validate_firmware_views(rom, rom_number)
}

fn validate_rom_structure(rom: &RomConfig) -> Result<(), Error> {
    if rom.schema_version != SCHEMA_VERSION {
        return Err(Error::new("RomSchemaVersion").with_component("rom.toml"));
    }

    validate_rom_id(&rom.id)?;
    if rom
        .number
        .is_some_and(|number| !(1..=MAX_ROM_NUMBER).contains(&number))
    {
        return Err(Error::new("RomNumberInvalid"));
    }

    if rom.managed && rom.partitions.is_empty() {
        return Err(Error::new("RomPartitionsEmpty").with_component("rom.toml"));
    }

    if !rom.managed && !rom.partitions.is_empty() {
        return Err(Error::new("RomPartitionsNotAllowed").with_component("rom.toml"));
    }

    if rom.partitions.len() > MAX_PROJECTIONS {
        return Err(Error::new("RomPartitionsTooMany"));
    }

    let mut names: Vec<&str> = Vec::with_capacity(rom.partitions.len());

    for (index, partition) in rom.partitions.iter().enumerate() {
        validate_partition_name(&partition.name)
            .map_err(|error| error.with_component(format!("partitions[{index}]")))?;

        if names.contains(&partition.name.as_str()) {
            return Err(Error::at("RomPartitionDuplicate", partition.name.clone()));
        }
        names.push(&partition.name);

        let backend = partition
            .backend()
            .map_err(|error| error.with_component(partition.name.clone()))?;

        if let Some(metadata) = &partition.metadata {
            if !relative_path(metadata) || !metadata.ends_with(".vbmd") {
                return Err(Error::at("RomMetadataPath", &partition.name));
            }
            let base = partition
                .name
                .strip_suffix("_a")
                .or_else(|| partition.name.strip_suffix("_b"));
            if base.is_none_or(|base| {
                base.is_empty() || matches!(base, "vbmeta" | "vbmeta_system" | "vbmeta_vendor")
            }) {
                return Err(Error::at("RomMetadataPartition", &partition.name));
            }
            if !matches!(backend, Backend::EspFile(_) | Backend::Mapper(_)) {
                return Err(Error::at("RomMetadataBackend", &partition.name));
            }
        }
    }

    Ok(())
}

/// Enforce the per-ROM firmware-view schema.
///
/// Views exist only on a managed ROM `>= 2`: ROM 1 and single-ROM payloads read
/// the physical firmware partitions directly. Every view names a physical
/// `<base>_a`/`<base>_b` PARTNAME that is not one of the image bases
/// (the running kernel already chose that slot), carries the deterministic
/// `(rom_number << 16) | index` thin id of its 1-based list position, appears
/// once, and is projected as the writable `/dev/mapper/rom<N>-fw-<name>`
/// partition that `fw-views` creates before the `gpt` entry runs.
fn validate_firmware_views(rom: &RomConfig, rom_number: u32) -> Result<(), Error> {
    if rom.firmware_views.is_empty() {
        return Ok(());
    }

    if !rom.managed {
        return Err(Error::new("RomFirmwareViewsUnmanaged"));
    }

    if rom_number < 2 {
        return Err(Error::new("RomFirmwareViewsRomNumber"));
    }

    let mut names: Vec<&str> = Vec::with_capacity(rom.firmware_views.len());

    for (offset, view) in rom.firmware_views.iter().enumerate() {
        validate_partition_name(&view.name)
            .map_err(|error| error.with_component(format!("firmware_views[{offset}]")))?;

        if names.contains(&view.name.as_str()) {
            return Err(Error::at("RomFirmwareViewDuplicate", view.name.clone()));
        }
        names.push(&view.name);

        let base = view
            .name
            .strip_suffix("_a")
            .or_else(|| view.name.strip_suffix("_b"))
            .filter(|base| !base.is_empty());

        let Some(base) = base else {
            return Err(Error::at("RomFirmwareViewName", view.name.clone()));
        };

        if IMAGE_BASES.contains(&base) {
            return Err(Error::at("RomFirmwareViewName", view.name.clone()));
        }

        let expected = (rom_number << 16) | (offset as u32 + 1);

        if view.thin_id != expected {
            return Err(Error::at("RomFirmwareViewThinId", view.name.clone()));
        }

        let backend = format!("/dev/mapper/rom{rom_number}-fw-{}", view.name);

        let projected = rom.partitions.iter().any(|partition| {
            partition.name == view.name && partition.backend == backend && !partition.read_only
        });

        if !projected {
            return Err(Error::at("RomFirmwareViewProjection", view.name.clone()));
        }
    }

    Ok(())
}

/// Enforce the managed-ROM module rules: a managed ROM requires `gpt` after the
/// core module, and an unmanaged ROM must not load `gpt` at all, because an
/// unmanaged manifest has no projection contract to apply.
pub fn validate_managed(manifest: &Manifest, rom: &RomConfig) -> Result<(), Error> {
    if !rom.managed {
        if manifest.modules.iter().any(|module| module.name == "gpt") {
            return Err(Error::at("ManifestUnmanagedGpt", "gpt"));
        }

        return Ok(());
    }

    match manifest
        .modules
        .iter()
        .position(|module| module.name == "gpt")
    {
        Some(0) => Err(Error::at("ManifestManagedGptOrder", "gpt")),
        Some(_) => Ok(()),
        None => Err(Error::new("ManifestManagedGptMissing")),
    }
}

fn validate_rom_id(id: &str) -> Result<(), Error> {
    if rom_id(id) {
        Ok(())
    } else {
        Err(Error::new("RomIdInvalid"))
    }
}

/// Logical module name: the loaded module name without `.ko`, bounded and
/// restricted to characters the kernel accepts in a module name.
pub fn validate_module_name(name: &str) -> Result<(), Error> {
    if name.is_empty() {
        return Err(Error::new("ModuleNameEmpty"));
    }

    if name.len() > MAX_IDENTIFIER_BYTES {
        return Err(Error::new("ModuleNameTooLong"));
    }

    if name
        .bytes()
        .any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'-'))
    {
        return Err(Error::new("ModuleNameInvalidCharacter"));
    }

    Ok(())
}

/// Projected partition name: ASCII letters/digits plus `_` and `-`, bounded,
/// with no path separators.
pub fn validate_partition_name(name: &str) -> Result<(), Error> {
    if name.is_empty() {
        return Err(Error::new("PartitionNameEmpty"));
    }

    if name.len() > MAX_PARTITION_NAME_BYTES {
        return Err(Error::new("PartitionNameTooLong"));
    }

    if name
        .bytes()
        .any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'-'))
    {
        return Err(Error::new("PartitionNameInvalidCharacter"));
    }

    Ok(())
}

/// Relative path rooted at the ESP subtree: no absolute paths, no trailing
/// separator, no empty components, and no `.` or `..` components.
pub fn validate_relative_path(path: &str) -> Result<(), Error> {
    if path.is_empty() {
        return Err(Error::new("PathEmpty"));
    }

    if path.len() > MAX_PATH_BYTES {
        return Err(Error::new("PathTooLong"));
    }

    if path.starts_with('/') {
        return Err(Error::new("PathAbsolute"));
    }

    if path.ends_with('/') {
        return Err(Error::new("PathTrailingSeparator"));
    }

    for component in path.split('/') {
        if component.is_empty() {
            return Err(Error::new("PathEmptyComponent"));
        }

        if component == "." || component == ".." {
            return Err(Error::new("PathTraversal"));
        }
    }

    Ok(())
}

/// Module parameters are handed to the kernel as an opaque string, never
/// evaluated by a shell. NUL and newline are rejected because they cannot be
/// represented in the kernel parameter encoding.
pub fn validate_module_params(params: &str) -> Result<(), Error> {
    if params.len() > MAX_PARAMS_BYTES {
        return Err(Error::new("ModuleParamsTooLong"));
    }

    if params.bytes().any(|byte| matches!(byte, 0 | b'\n' | b'\r')) {
        return Err(Error::new("ModuleParamsInvalidCharacter"));
    }

    Ok(())
}
