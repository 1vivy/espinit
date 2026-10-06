//! Strict validation of the managed-boot configuration.
//!
//! The manifest and ROM files are the only configuration inputs, TOML is used
//! directly, and every rule is enforced here: schema version, generation
//! identity, module order/name/path, and the managed-ROM projection rules.
//! Invalid configuration is never interpreted as `managed = false`, and the
//! loader never falls back to another generation or an implicit module.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::block::{self, ResolvedBackend};
use crate::receipt::{Failure, Stage};

/// Only schema version 1 is accepted by both files.
pub const SCHEMA_VERSION: u64 = 1;

/// Highest managed ROM number. Five ROMs exactly fill DeviceInfo's 32
/// per-ROM rollback windows, so a larger number could never be isolated.
pub const MAX_ROM_NUMBER: u32 = 5;

/// Physical bases whose `_a`/`_b` pair is selected by the kernel from the
/// current slot. A ROM never shadows them with its own firmware view: the
/// running kernel already chose the slot it booted from.
const KERNEL_SET_BASES: [&str; 7] = [
    "boot",
    "init_boot",
    "vendor_boot",
    "dtbo",
    "vbmeta",
    "vbmeta_system",
    "vbmeta_vendor",
];

/// Manifest generation limit, matching every compiled payload component.
pub const MAX_GENERATION_BYTES: usize = 63;

/// Logical module name limit.
pub const MAX_NAME_BYTES: usize = 64;

/// Projected partition label limit: the `gpt` ABI carries this many label
/// bytes plus the terminating NUL, so a longer name could never be projected.
pub const MAX_PARTITION_NAME_BYTES: usize = crate::gpt_uapi::GPT_LABEL_BYTES;

/// Kernel module parameter string limit enforced by the kernel loader.
pub const MAX_PARAMS_BYTES: usize = 1024;

const MAX_PATH_BYTES: usize = 4096;

/// A configuration rejection carrying the stable error identifier, the
/// attributable component (when known) and a bounded detail.
#[derive(Debug)]
pub struct ConfigError {
    pub error: &'static str,
    pub component: Option<String>,
    pub detail: String,
    /// Set only for a backend that is not enumerated yet, so a bounded retry
    /// can wait for it without weakening any other rejection.
    pending: bool,
}

impl ConfigError {
    fn new(error: &'static str, detail: impl Into<String>) -> Self {
        Self {
            error,
            component: None,
            detail: detail.into(),
            pending: false,
        }
    }

    fn at(error: &'static str, component: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            error,
            component: Some(component.into()),
            detail: detail.into(),
            pending: false,
        }
    }

    /// Whether the rejection is a device or sysfs entry that is not enumerated
    /// yet and may appear within the bounded enumeration window. Ambiguity,
    /// malformed metadata, wrong device type, unsupported paths, duplicates,
    /// and every strict TOML/schema rejection are never pending.
    pub fn is_pending(&self) -> bool {
        self.pending
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.error, self.detail)
    }
}

impl From<ConfigError> for Failure {
    fn from(error: ConfigError) -> Self {
        Failure::at(
            Stage::Configuration,
            error.component.as_deref(),
            error.error,
            error.detail,
        )
    }
}

/// `manifest.toml`, parsed with unknown/duplicate/missing fields rejected.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u64,
    pub generation: String,
    /// Directory of per-ROM `<id>.toml` files, relative to the payload root.
    pub rom: String,
    pub modules: Vec<ModuleEntry>,
    #[serde(default)]
    pub platform: Option<esu_platform::Platform>,
}

/// One ordered manifest module entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleEntry {
    pub name: String,
    pub path: String,
    pub params: String,
}

/// Default managed ROM number: ROM 1, the stock ROM, which every payload that
/// predates `rom_number` describes.
fn default_rom_number() -> u32 {
    1
}

/// Selected per-ROM configuration, parsed with unknown/duplicate/missing fields rejected.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RomConfig {
    pub schema_version: u64,
    pub generation: String,
    pub id: String,
    pub managed: bool,
    /// Managed ROM number, identical to this ROM's `Slot-<id>` record.
    /// `1` for the stock ROM and for every payload that predates the field;
    /// `2..=5` for the additional ROMs that share the firmware storage.
    #[serde(default = "default_rom_number")]
    pub rom_number: u32,
    #[serde(default)]
    pub partitions: Vec<PartitionEntry>,
    /// Per-ROM firmware views: thin devices of the shared pool that serve a
    /// physical firmware partition's exact bytes until this ROM writes to them.
    /// Only valid on a managed ROM `>= 2`; ROM 1 and single-ROM payloads leave
    /// the list absent.
    #[serde(default)]
    pub firmware_views: Vec<FirmwareView>,
    /// Stable block nodes resolved by [`validate_backends`], in document order.
    /// Never read from the file: the logical `partitions` stay unchanged.
    #[serde(skip)]
    resolved: OnceLock<Vec<ResolvedBackend>>,
}

impl RomConfig {
    /// Stable block nodes resolved from the logical backends, ready for the
    /// later `gpt` APPLY. Empty until [`validate_backends`] has succeeded.
    pub fn resolved_backends(&self) -> &[ResolvedBackend] {
        self.resolved.get().map(Vec::as_slice).unwrap_or_default()
    }

    /// Whether any projection writes through an `esp-file:` backend, i.e. the
    /// loader must hold the ESP superblock writable for this boot so its own
    /// loop devices can reach the preallocated images. Every other payload
    /// keeps the ESP read-only.
    pub fn has_writable_esp_file(&self) -> bool {
        self.partitions
            .iter()
            .any(|partition| !partition.read_only && block::is_esp_file(&partition.backend))
    }

    /// Summarize requested access modes without exposing partition/backend names.
    /// Call after validation; this describes requests, not applied GPT state.
    pub fn partition_modes(&self) -> PartitionModes {
        let read_only = self
            .partitions
            .iter()
            .filter(|partition| partition.read_only)
            .count();
        PartitionModes {
            total: self.partitions.len(),
            read_only,
            writable: self.partitions.len() - read_only,
        }
    }
}

/// Fixed-size, path-free diagnostics for the requested partition access modes.
#[derive(Clone, Copy)]
pub struct PartitionModes {
    total: usize,
    read_only: usize,
    writable: usize,
}

impl fmt::Display for PartitionModes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "requested partitions={}, read-only={}, writable={}",
            self.total, self.read_only, self.writable
        )
    }
}

/// One projected partition. `read_only` is explicit; there is no default.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartitionEntry {
    pub name: String,
    pub backend: String,
    pub read_only: bool,
}

/// One per-ROM firmware view. The physical partition named by `name` is served
/// by a thin device of the shared pool through the external-origin `thin`
/// table, so unwritten blocks read the physical bytes and every ROM's OTA
/// writes land in its own provisioned blocks.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareView {
    /// Physical sysfs `PARTNAME` this view shadows: `<base>_a` or `<base>_b`.
    pub name: String,
    /// Pool-unique thin id, fixed to `(rom_number << 16) | index` with the
    /// 1-based position of this view in the list, so ids are deterministic and
    /// never collide with an LVM2-owned id below `0x10000`.
    pub thin_id: u32,
}

/// Parse and validate `manifest.toml` text.
pub fn parse_manifest(text: &str) -> Result<Manifest, ConfigError> {
    let manifest: Manifest = toml::from_str(text)
        .map_err(|error| ConfigError::new("ManifestParse", error.to_string()))?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// Parse and validate a selected ROM snapshot against the manifest generation.
pub fn parse_rom(text: &str, manifest_generation: &str) -> Result<RomConfig, ConfigError> {
    let rom: RomConfig =
        toml::from_str(text).map_err(|error| ConfigError::new("RomParse", error.to_string()))?;
    validate_rom(&rom, manifest_generation)?;
    Ok(rom)
}

/// Select only the explicit Surfacer ID. Missing, duplicate or conflicting
/// boot arguments are errors, never a request for a default ROM.
pub fn selected_rom_id<'a>(bootconfig: &'a str, cmdline: &'a str) -> Result<&'a str, ConfigError> {
    const KEY: &str = "androidboot.esu.rom";
    fn entry<'a>(text: &'a str, selected: &mut Option<&'a str>) -> Result<(), ConfigError> {
        let (key, value) = text.split_once('=').unwrap_or((text, ""));
        if key.trim() != KEY {
            return Ok(());
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        validate_rom_id(value)?;
        if selected.replace(value).is_some() {
            return Err(ConfigError::new("RomSelectionDuplicate", KEY));
        }
        Ok(())
    }
    let mut boot = None;
    let mut command = None;
    for line in bootconfig.lines() {
        entry(line.trim(), &mut boot)?;
    }
    for token in cmdline.split_whitespace() {
        entry(token, &mut command)?;
    }
    if matches!((boot, command), (Some(a), Some(b)) if a != b) {
        return Err(ConfigError::new("RomSelectionConflict", KEY));
    }
    boot.or(command)
        .ok_or_else(|| ConfigError::new("RomSelectionMissing", KEY))
}

fn validate_rom_id(id: &str) -> Result<(), ConfigError> {
    if id.len() > 59 {
        return Err(ConfigError::new("RomIdInvalid", "ROM ID exceeds 59 bytes"));
    }
    esu_platform::identifier(id)
        .map_err(|error| ConfigError::new("RomIdInvalid", error.to_string()))
}

pub fn rom_path(manifest: &Manifest, id: &str) -> Result<String, ConfigError> {
    validate_rom_id(id)?;
    let path = format!("{}/{id}.toml", manifest.rom);
    validate_relative_path(&path)?;
    Ok(path)
}

pub fn parse_selected_rom(
    text: &str,
    generation: &str,
    id: &str,
) -> Result<RomConfig, ConfigError> {
    validate_rom_id(id)?;
    let rom = parse_rom(text, generation)?;
    if rom.id != id {
        return Err(ConfigError::new(
            "RomIdMismatch",
            format!("selected {id}, configured {}", rom.id),
        ));
    }
    Ok(rom)
}

/// Enforce the manifest schema, generation rule, and ordered module rules.
pub fn validate_manifest(manifest: &Manifest) -> Result<(), ConfigError> {
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(ConfigError::new(
            "ManifestSchemaVersion",
            format!(
                "expected {SCHEMA_VERSION}, found {}",
                manifest.schema_version
            ),
        ));
    }

    validate_generation(&manifest.generation)
        .map_err(|error| error.with_component("manifest.toml"))?;

    validate_relative_path(&manifest.rom).map_err(|error| error.with_component("manifest.toml"))?;
    if let Some(platform) = &manifest.platform {
        platform.validate().map_err(|error| {
            ConfigError::at("PlatformConfiguration", "manifest.toml", error.to_string())
        })?;
    }

    if manifest.modules.is_empty() {
        return Err(ConfigError::new(
            "ManifestModulesEmpty",
            "no modules listed",
        ));
    }

    let mut names: Vec<&str> = Vec::with_capacity(manifest.modules.len());

    for (index, module) in manifest.modules.iter().enumerate() {
        validate_module_name(&module.name)
            .map_err(|error| error.with_component(format!("modules[{index}]")))?;

        if names.contains(&module.name.as_str()) {
            return Err(ConfigError::at(
                "ManifestDuplicateModule",
                module.name.clone(),
                "module name is listed more than once",
            ));
        }
        names.push(&module.name);

        validate_relative_path(&module.path)
            .map_err(|error| error.with_component(module.name.clone()))?;

        validate_module_params(&module.params)
            .map_err(|error| error.with_component(module.name.clone()))?;
    }

    if manifest.modules[0].name != "kernelesp" {
        return Err(ConfigError::at(
            "ManifestCoreFirst",
            manifest.modules[0].name.clone(),
            "the first manifest entry must be the core module `esu`",
        ));
    }

    Ok(())
}

/// Enforce the ROM schema, generation equality, and projection rules.
pub fn validate_rom(rom: &RomConfig, manifest_generation: &str) -> Result<(), ConfigError> {
    if rom.schema_version != SCHEMA_VERSION {
        return Err(ConfigError::new(
            "RomSchemaVersion",
            format!("expected {SCHEMA_VERSION}, found {}", rom.schema_version),
        )
        .with_component("rom.toml"));
    }

    validate_generation(&rom.generation).map_err(|error| error.with_component("rom.toml"))?;
    validate_rom_id(&rom.id)?;

    if rom.rom_number == 0 || rom.rom_number > MAX_ROM_NUMBER {
        return Err(ConfigError::at(
            "RomNumberInvalid",
            "rom.toml",
            format!(
                "rom_number must be 1..={MAX_ROM_NUMBER}, found {}",
                rom.rom_number
            ),
        ));
    }

    if rom.generation != manifest_generation {
        return Err(ConfigError::at(
            "RomGenerationMismatch",
            "rom.toml",
            format!(
                "rom generation {} does not match manifest generation {}",
                rom.generation, manifest_generation
            ),
        ));
    }

    if rom.managed && rom.partitions.is_empty() {
        return Err(ConfigError::new(
            "RomPartitionsEmpty",
            "managed = true requires a nonempty partitions array",
        )
        .with_component("rom.toml"));
    }

    if !rom.managed && !rom.partitions.is_empty() {
        return Err(ConfigError::new(
            "RomPartitionsNotAllowed",
            "managed = false requires an empty or absent partitions array",
        )
        .with_component("rom.toml"));
    }

    if rom.partitions.len() > crate::gpt_uapi::GPT_MAX_PROJECTIONS {
        return Err(ConfigError::new(
            "RomPartitionsTooMany",
            format!(
                "at most {} projections are supported",
                crate::gpt_uapi::GPT_MAX_PROJECTIONS
            ),
        ));
    }

    let mut names: Vec<&str> = Vec::with_capacity(rom.partitions.len());

    for (index, partition) in rom.partitions.iter().enumerate() {
        validate_partition_name(&partition.name)
            .map_err(|error| error.with_component(format!("partitions[{index}]")))?;

        if names.contains(&partition.name.as_str()) {
            return Err(ConfigError::at(
                "RomPartitionDuplicate",
                partition.name.clone(),
                "projected name is listed more than once",
            ));
        }
        names.push(&partition.name);

        validate_backend_path(&partition.backend)
            .map_err(|error| error.with_component(partition.name.clone()))?;

        if block::is_esp_file(&partition.backend) && !partition.read_only && rom.rom_number < 2 {
            return Err(ConfigError::at(
                "RomEspFileWritable",
                partition.name.clone(),
                "a writable ESP-file backend requires a managed ROM >= 2",
            ));
        }
    }

    validate_firmware_views(rom)?;

    Ok(())
}

/// Enforce the per-ROM firmware-view schema.
///
/// Views exist only on a managed ROM `>= 2`: ROM 1 and single-ROM payloads read
/// the physical firmware partitions directly. Every view names a physical
/// `<base>_a`/`<base>_b` PARTNAME that is not one of the seven kernel-set bases
/// (the running kernel already chose that slot), carries the deterministic
/// `(rom_number << 16) | index` thin id of its 1-based list position, appears
/// once, and is projected as the writable `/dev/mapper/rom<N>-fw-<name>`
/// partition that `fw-views` creates before the `gpt` entry runs.
fn validate_firmware_views(rom: &RomConfig) -> Result<(), ConfigError> {
    if rom.firmware_views.is_empty() {
        return Ok(());
    }

    if !rom.managed {
        return Err(ConfigError::new(
            "RomFirmwareViewsUnmanaged",
            "firmware views require a managed ROM",
        ));
    }

    if rom.rom_number < 2 {
        return Err(ConfigError::new(
            "RomFirmwareViewsRomNumber",
            format!(
                "firmware views require rom_number >= 2, found {}",
                rom.rom_number
            ),
        ));
    }

    let mut names: Vec<&str> = Vec::with_capacity(rom.firmware_views.len());

    for (offset, view) in rom.firmware_views.iter().enumerate() {
        let component = || format!("firmware_views[{offset}]");

        validate_partition_name(&view.name).map_err(|error| error.with_component(component()))?;

        if names.contains(&view.name.as_str()) {
            return Err(ConfigError::at(
                "RomFirmwareViewDuplicate",
                view.name.clone(),
                "firmware view name is listed more than once",
            ));
        }
        names.push(&view.name);

        let base = view
            .name
            .strip_suffix("_a")
            .or_else(|| view.name.strip_suffix("_b"))
            .filter(|base| !base.is_empty());

        let Some(base) = base else {
            return Err(ConfigError::at(
                "RomFirmwareViewName",
                view.name.clone(),
                "firmware view must name a physical <base>_a or <base>_b partition",
            ));
        };

        if KERNEL_SET_BASES.contains(&base) {
            return Err(ConfigError::at(
                "RomFirmwareViewName",
                view.name.clone(),
                format!("{base} is selected by the kernel and is never viewed"),
            ));
        }

        let expected = (rom.rom_number << 16) | (offset as u32 + 1);

        if view.thin_id != expected {
            return Err(ConfigError::at(
                "RomFirmwareViewThinId",
                view.name.clone(),
                format!(
                    "thin id {} is not the reserved id {expected} for firmware view {}",
                    view.thin_id, view.name
                ),
            ));
        }

        let backend = format!("/dev/mapper/rom{}-fw-{}", rom.rom_number, view.name);

        let projected = rom.partitions.iter().any(|partition| {
            partition.name == view.name && partition.backend == backend && !partition.read_only
        });

        if !projected {
            return Err(ConfigError::at(
                "RomFirmwareViewProjection",
                view.name.clone(),
                format!("firmware view requires a writable partition with backend {backend}"),
            ));
        }
    }

    Ok(())
}

/// Enforce the managed-ROM module rules: a managed ROM requires `gpt` after the
/// core module, and an unmanaged ROM must not load `gpt` at all, because an
/// unmanaged manifest has no projection contract to apply.
pub fn validate_managed(manifest: &Manifest, rom: &RomConfig) -> Result<(), ConfigError> {
    if !rom.managed {
        if manifest.modules.iter().any(|module| module.name == "gpt") {
            return Err(ConfigError::at(
                "ManifestUnmanagedGpt",
                "gpt",
                "an unmanaged ROM must not load gpt",
            ));
        }

        return Ok(());
    }

    match manifest
        .modules
        .iter()
        .position(|module| module.name == "gpt")
    {
        Some(0) => Err(ConfigError::at(
            "ManifestManagedGptOrder",
            "gpt",
            "a managed ROM requires `gpt` after the core module",
        )),
        Some(_) => Ok(()),
        None => Err(ConfigError::new(
            "ManifestManagedGptMissing",
            "a managed ROM requires a `gpt` module after the core module",
        )),
    }
}

/// Resolve each projection backend, immediately before the `gpt` entry and
/// after every earlier ordered module and its scripts have run, so a logical
/// volume, mapper device, loop or ESP file published by them is visible. A
/// `/dev/block/by-name/<PARTNAME>` or `/dev/mapper/<name>` backend is resolved
/// from sysfs to an owned stable block node, an existing `/dev/loopN` is
/// accepted as it is, and an `esp-file:<relative-path>` backend is attached to
/// a fresh loop device with the projection's access: read-only below the
/// read-only ESP mount, writable only for a managed ROM `>= 2` whose loader
/// holds the ESP read-write. Each accepted form establishes its own allowed
/// device kind and access, so a whole logical unit, a mismatched ESP file and
/// every other path are rejected. The resolved
/// set is retained on the configuration and returned for the `gpt` APPLY, which
/// keeps every loop guard open, while the logical `partitions` stay untouched.
///
/// Only an absent device or sysfs entry is classified pending by
/// [`ConfigError::is_pending`]; the caller may retry that within a bounded
/// window. Ambiguous, malformed, unsupported and duplicated backends fail
/// immediately, and the complete resolved set is published at once.
pub fn validate_backends<'a>(
    rom: &'a RomConfig,
    esp_mount: &str,
) -> Result<&'a [ResolvedBackend], ConfigError> {
    if let Some(published) = rom.resolved.get() {
        return Ok(published.as_slice());
    }

    let mut backends: HashMap<u64, String> = HashMap::new();
    let mut resolved = Vec::with_capacity(rom.partitions.len());

    for partition in &rom.partitions {
        let access = if partition.read_only {
            block::Access::ReadOnly
        } else {
            block::Access::Writable
        };
        let backend = block::resolve(&partition.backend, esp_mount, access)
            .map_err(|error| backend_error(partition, &error))?;

        validate_backend_identity(&mut backends, &backend, partition)?;

        resolved.push(backend);
    }

    // Publish the complete set at once, only after every projection resolved.
    // A re-entry (bounded retry or a later caller) reuses the published set.
    let _ = rom.resolved.set(resolved);

    Ok(rom.resolved_backends())
}

/// Classify one backend resolution failure. Only an absent device or sysfs
/// entry ([`block::is_pending`]) may appear once enumeration finishes; every
/// other resolution failure stops boot immediately.
fn backend_error(partition: &PartitionEntry, error: &io::Error) -> ConfigError {
    let rejected = ConfigError::at(
        "RomBackendUnavailable",
        partition.name.clone(),
        format!("{}: {error}", partition.backend),
    );

    if block::is_pending(error) {
        rejected.with_pending()
    } else {
        rejected
    }
}

/// Check resolved device identities independently of filesystem discovery: a
/// device number may back exactly one projection.
fn validate_backend_identity(
    backends: &mut HashMap<u64, String>,
    backend: &ResolvedBackend,
    partition: &PartitionEntry,
) -> Result<(), ConfigError> {
    if let Some(retained) = backends.insert(backend.rdev, backend.path.clone()) {
        return Err(ConfigError::at(
            "RomBackendDuplicate",
            partition.name.clone(),
            format!(
                "{} is {retained}, already used by another projection",
                partition.backend
            ),
        ));
    }
    Ok(())
}

/// Generation identity: nonempty ASCII letters/digits plus `.`, `_`, `-`,
/// at most 63 bytes. It identifies one coordinated payload, not a kernel
/// version, and is compared by exact byte equality everywhere.
pub fn validate_generation(generation: &str) -> Result<(), ConfigError> {
    if generation.is_empty() {
        return Err(ConfigError::new("GenerationEmpty", "generation is empty"));
    }

    if generation.len() > MAX_GENERATION_BYTES {
        return Err(ConfigError::new(
            "GenerationTooLong",
            format!(
                "generation is {} bytes, limit {MAX_GENERATION_BYTES}",
                generation.len()
            ),
        ));
    }

    if !generation.is_ascii() {
        return Err(ConfigError::new(
            "GenerationNotAscii",
            "generation must be ASCII",
        ));
    }

    if let Some(byte) = generation
        .bytes()
        .find(|byte| !byte.is_ascii_alphanumeric() && !matches!(*byte, b'.' | b'_' | b'-'))
    {
        return Err(ConfigError::new(
            "GenerationInvalidCharacter",
            format!("generation contains {byte:#04x} outside [A-Za-z0-9._-]"),
        ));
    }

    Ok(())
}

/// Logical module name: the loaded module name without `.ko`, bounded and
/// restricted to characters the kernel accepts in a module name.
pub fn validate_module_name(name: &str) -> Result<(), ConfigError> {
    if name.is_empty() {
        return Err(ConfigError::new("ModuleNameEmpty", "module name is empty"));
    }

    if name.len() > MAX_NAME_BYTES {
        return Err(ConfigError::new(
            "ModuleNameTooLong",
            format!(
                "module name is {} bytes, limit {MAX_NAME_BYTES}",
                name.len()
            ),
        ));
    }

    if let Some(byte) = name
        .bytes()
        .find(|byte| !byte.is_ascii_alphanumeric() && !matches!(*byte, b'_' | b'-'))
    {
        return Err(ConfigError::new(
            "ModuleNameInvalidCharacter",
            format!("module name contains {byte:#04x} outside [A-Za-z0-9_-]"),
        ));
    }

    Ok(())
}

/// Projected partition name: ASCII letters/digits plus `_` and `-`, bounded,
/// with no path separators.
pub fn validate_partition_name(name: &str) -> Result<(), ConfigError> {
    if name.is_empty() {
        return Err(ConfigError::new(
            "PartitionNameEmpty",
            "projected name is empty",
        ));
    }

    if name.len() > MAX_PARTITION_NAME_BYTES {
        return Err(ConfigError::new(
            "PartitionNameTooLong",
            format!(
                "projected name is {} bytes, limit {MAX_PARTITION_NAME_BYTES}",
                name.len()
            ),
        ));
    }

    if let Some(byte) = name
        .bytes()
        .find(|byte| !byte.is_ascii_alphanumeric() && !matches!(*byte, b'_' | b'-'))
    {
        return Err(ConfigError::new(
            "PartitionNameInvalidCharacter",
            format!("projected name contains {byte:#04x} outside [A-Za-z0-9_-]"),
        ));
    }

    Ok(())
}

/// Relative path rooted at the ESP `/esu` subtree: no absolute paths, no
/// empty components, and no `.` or `..` components.
pub fn validate_relative_path(path: &str) -> Result<(), ConfigError> {
    if path.is_empty() {
        return Err(ConfigError::new("PathEmpty", "path is empty"));
    }

    if path.len() > MAX_PATH_BYTES {
        return Err(ConfigError::new(
            "PathTooLong",
            format!("path is {} bytes, limit {MAX_PATH_BYTES}", path.len()),
        ));
    }

    if path.starts_with('/') {
        return Err(ConfigError::new(
            "PathAbsolute",
            format!("{path} is absolute"),
        ));
    }

    if path.ends_with('/') {
        return Err(ConfigError::new(
            "PathTrailingSeparator",
            format!("{path} ends with a separator"),
        ));
    }

    for component in path.split('/') {
        if component.is_empty() {
            return Err(ConfigError::new(
                "PathEmptyComponent",
                format!("{path} contains an empty component"),
            ));
        }

        if component == "." || component == ".." {
            return Err(ConfigError::new(
                "PathTraversal",
                format!("{path} contains a {component} component"),
            ));
        }
    }

    Ok(())
}

/// Absolute backend path in one of the four documented forms:
/// `/dev/block/by-name/<PARTNAME>`, `/dev/mapper/<name>`, an existing
/// `/dev/loopN`, or `esp-file:<relative-path>`. Anything else is rejected here,
/// including whole logical units, offsets, arbitrary paths, and symlink or
/// traversal spellings; [`validate_backends`] resolves the device itself
/// immediately before the `gpt` entry.
pub fn validate_backend_path(path: &str) -> Result<(), ConfigError> {
    if !path.starts_with('/') && !block::is_esp_file(path) {
        return Err(ConfigError::new(
            "BackendNotAbsolute",
            format!("{path} is not an absolute block-device path"),
        ));
    }

    if path.len() > MAX_PATH_BYTES {
        return Err(ConfigError::new(
            "BackendPathTooLong",
            format!("backend is {} bytes, limit {MAX_PATH_BYTES}", path.len()),
        ));
    }

    if path.bytes().any(|byte| byte == 0) {
        return Err(ConfigError::new(
            "BackendInvalidCharacter",
            "backend contains NUL",
        ));
    }

    if !block::is_supported(path) {
        return Err(ConfigError::new(
            "BackendUnsupportedLocation",
            format!("{path} is not a supported partition, mapper, loop, or ESP-file backend"),
        ));
    }

    Ok(())
}

/// Module parameters are handed to the kernel as an opaque string, never
/// evaluated by a shell. NUL and newline are rejected because they cannot be
/// represented in the kernel parameter encoding.
pub fn validate_module_params(params: &str) -> Result<(), ConfigError> {
    if params.len() > MAX_PARAMS_BYTES {
        return Err(ConfigError::new(
            "ModuleParamsTooLong",
            format!(
                "params are {} bytes, limit {MAX_PARAMS_BYTES}",
                params.len()
            ),
        ));
    }

    if let Some(byte) = params
        .bytes()
        .find(|byte| matches!(*byte, 0 | b'\n' | b'\r'))
    {
        return Err(ConfigError::new(
            "ModuleParamsInvalidCharacter",
            format!("params contain {byte:#04x}"),
        ));
    }

    Ok(())
}

impl ConfigError {
    fn with_component(mut self, component: impl Into<String>) -> Self {
        if self.component.is_none() {
            self.component = Some(component.into());
        }
        self
    }

    fn with_pending(mut self) -> Self {
        self.pending = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
schema_version = 1
generation = "release-1"
rom = "roms"
[[modules]]
name = "kernelesp"
path = "modules/kernelesp.ko"
params = ""
[[modules]]
name = "gpt"
path = "modules/gpt.ko"
params = "debug=0"
"#;
    const ROM: &str = r#"
schema_version = 1
generation = "release-1"
id = "android-a"
managed = true
[[partitions]]
name = "system"
backend = "/dev/block/by-name/system"
read_only = true
"#;
    /// One managed ROM 2 view of the physical `xbl_a` firmware partition.
    const FW_ROM: &str = r#"
schema_version = 1
generation = "release-1"
id = "android-b"
rom_number = 2
managed = true
[[firmware_views]]
name = "xbl_a"
thin_id = 131073
[[partitions]]
name = "xbl_a"
backend = "/dev/mapper/rom2-fw-xbl_a"
read_only = false
"#;

    #[test]
    fn boot_selection_is_explicit_unique_and_matches_the_rom() {
        for id in ["android-a", "android.b_2", "recovery", &"x".repeat(59)] {
            let boot = format!("androidboot.esu.rom = \"{id}\"\n");
            let command = format!("androidboot.esu.rom={id}");
            assert_eq!(selected_rom_id(&boot, "").unwrap(), id);
            assert_eq!(selected_rom_id("", &command).unwrap(), id);
            assert_eq!(selected_rom_id(&boot, &command).unwrap(), id);
            assert_eq!(
                rom_path(&parse_manifest(MANIFEST).unwrap(), id).unwrap(),
                format!("roms/{id}.toml")
            );
        }
        for (boot, command) in [
            ("", ""),
            ("androidboot.qshim.rom = \"android-a\"", ""),
            ("androidboot.esu.rom = \"a\"", "androidboot.esu.rom=b"),
            (
                "androidboot.esu.rom = \"a\"\nandroidboot.esu.rom = \"a\"",
                "",
            ),
            ("", "androidboot.esu.rom=a androidboot.esu.rom=a"),
        ] {
            assert!(selected_rom_id(boot, command).is_err());
        }
        for id in [
            "",
            ".",
            "..",
            "../a",
            "a/b",
            "é",
            "a b",
            "\"a",
            "a\"",
            &"x".repeat(60),
        ] {
            assert!(selected_rom_id(&format!("androidboot.esu.rom = {id}"), "").is_err());
            assert!(rom_path(&parse_manifest(MANIFEST).unwrap(), id).is_err());
        }
        parse_selected_rom(ROM, "release-1", "android-a").unwrap();
        assert_eq!(
            parse_selected_rom(ROM, "release-1", "android-b")
                .unwrap_err()
                .error,
            "RomIdMismatch"
        );
        assert!(parse_rom(&ROM.replace("android-a", "../a"), "release-1").is_err());
    }

    #[test]
    fn valid_managed_configuration_preserves_order_and_projection() {
        let manifest = parse_manifest(MANIFEST).unwrap();
        let rom = parse_rom(ROM, &manifest.generation).unwrap();
        validate_managed(&manifest, &rom).unwrap();
        assert_eq!(manifest.rom, "roms");
        assert_eq!(
            manifest
                .modules
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            ["kernelesp", "gpt"]
        );
        assert_eq!(manifest.modules[1].params, "debug=0");
        assert_eq!(rom.partitions[0].backend, "/dev/block/by-name/system");
        assert!(rom.partitions[0].read_only);
    }

    #[test]
    fn rom_number_defaults_to_the_stock_rom_and_stays_within_the_rollback_table() {
        assert_eq!(parse_rom(ROM, "release-1").unwrap().rom_number, 1);

        let explicit = ROM.replace("managed = true", "managed = true\nrom_number = 3");
        assert_eq!(
            parse_selected_rom(&explicit, "release-1", "android-a")
                .unwrap()
                .rom_number,
            3
        );

        for invalid in [0, MAX_ROM_NUMBER + 1, u32::MAX] {
            let text = ROM.replace(
                "managed = true",
                &format!("managed = true\nrom_number = {invalid}"),
            );
            let error = parse_rom(&text, "release-1").unwrap_err();
            assert_eq!(error.error, "RomNumberInvalid", "{invalid}");
            assert_eq!(error.component.as_deref(), Some("rom.toml"));
            assert!(!error.is_pending());
            assert!(parse_selected_rom(&text, "release-1", "android-a").is_err());
        }

        assert_eq!(
            parse_rom(
                &ROM.replace("managed = true", "managed = true\nrom_number = \"1\""),
                "release-1"
            )
            .unwrap_err()
            .error,
            "RomParse"
        );
    }

    #[test]
    fn manifest_rejects_unknown_duplicate_missing_and_wrong_type_fields() {
        for text in [
            format!("unexpected = true\n{MANIFEST}"),
            format!("{MANIFEST}unexpected = true\n"),
            MANIFEST.replace(
                "schema_version = 1",
                "schema_version = 1\nschema_version = 1",
            ),
            MANIFEST.replace("params = \"\"\n", ""),
            MANIFEST.replace("schema_version = 1", "schema_version = \"1\""),
        ] {
            assert_eq!(
                parse_manifest(&text).unwrap_err().error,
                "ManifestParse",
                "{text}"
            );
        }
    }

    #[test]
    fn rom_rejects_unknown_duplicate_missing_and_wrong_type_fields() {
        for text in [
            format!("unexpected = true\n{ROM}"),
            format!("{ROM}unexpected = true\n"),
            ROM.replace("managed = true", "managed = true\nmanaged = false"),
            ROM.replace("read_only = true\n", ""),
            ROM.replace("id = \"android-a\"\n", ""),
            ROM.replace("managed = true", "managed = \"true\""),
        ] {
            assert_eq!(
                parse_rom(&text, "release-1").unwrap_err().error,
                "RomParse",
                "{text}"
            );
        }
    }

    #[test]
    fn manifest_enforces_schema_core_order_and_unique_names() {
        for (text, error) in [
            (
                MANIFEST.replace("schema_version = 1", "schema_version = 2"),
                "ManifestSchemaVersion",
            ),
            (
                MANIFEST.replace("name = \"kernelesp\"", "name = \"other\""),
                "ManifestCoreFirst",
            ),
            (
                MANIFEST.replace("name = \"gpt\"", "name = \"kernelesp\""),
                "ManifestDuplicateModule",
            ),
            (
                "schema_version=1\ngeneration=\"release-1\"\nrom=\"roms\"\nmodules=[]".into(),
                "ManifestModulesEmpty",
            ),
        ] {
            assert_eq!(parse_manifest(&text).unwrap_err().error, error);
        }
    }

    #[test]
    fn managed_requires_gpt_and_partitions_without_unmanaged_fallback() {
        let manifest =
            parse_manifest(&MANIFEST.replace("name = \"gpt\"", "name = \"other\"")).unwrap();
        let rom = parse_rom(ROM, "release-1").unwrap();
        assert_eq!(
            validate_managed(&manifest, &rom).unwrap_err().error,
            "ManifestManagedGptMissing"
        );
        let prefix = ROM.split("[[partitions]]").next().unwrap();
        for text in [prefix.to_owned(), format!("{prefix}partitions = []\n")] {
            assert_eq!(
                parse_rom(&text, "release-1").unwrap_err().error,
                "RomPartitionsEmpty"
            );
        }
        assert_eq!(
            parse_rom(
                &ROM.replace("managed = true", "managed = false"),
                "release-1"
            )
            .unwrap_err()
            .error,
            "RomPartitionsNotAllowed"
        );
        let unmanaged = parse_rom(
            &prefix.replace("managed = true", "managed = false"),
            "release-1",
        )
        .unwrap();
        assert!(!unmanaged.managed);
        assert!(unmanaged.partitions.is_empty());

        // An unmanaged manifest must not load `gpt`: there is no projection
        // contract to apply, so the entry is rejected instead of skipped.
        let error = validate_managed(&parse_manifest(MANIFEST).unwrap(), &unmanaged).unwrap_err();
        assert_eq!(error.error, "ManifestUnmanagedGpt");
        assert_eq!(error.component.as_deref(), Some("gpt"));

        // The same unmanaged ROM with an unmanaged manifest is valid.
        let plain = parse_manifest(&MANIFEST.replace(
            "\n[[modules]]\nname = \"gpt\"\npath = \"modules/gpt.ko\"\nparams = \"debug=0\"\n",
            "",
        ))
        .unwrap();
        validate_managed(&plain, &unmanaged).unwrap();
    }

    #[test]
    fn rom_rejects_generation_schema_and_duplicate_partition_names() {
        assert_eq!(
            parse_rom(ROM, "Release-1").unwrap_err().error,
            "RomGenerationMismatch"
        );
        assert_eq!(
            parse_rom(
                &ROM.replace("schema_version = 1", "schema_version = 0"),
                "release-1"
            )
            .unwrap_err()
            .error,
            "RomSchemaVersion"
        );
        let duplicate = format!(
            "{ROM}\n[[partitions]]\nname=\"system\"\nbackend=\"/dev/other\"\nread_only=false\n"
        );
        let error = parse_rom(&duplicate, "release-1").unwrap_err();
        assert_eq!(error.error, "RomPartitionDuplicate");
        assert_eq!(error.component.as_deref(), Some("system"));
    }

    #[test]
    fn resolved_backend_identity_rejects_aliases_but_not_distinct_devices() {
        let rom = parse_rom(ROM, "release-1").unwrap();
        let first = &rom.partitions[0];
        let vendor = PartitionEntry {
            name: "vendor".into(),
            backend: "/dev/loop7".into(),
            read_only: false,
        };
        let system = ResolvedBackend {
            path: "/dev/esu/backends/sda1".into(),
            rdev: 8,
            guard: None,
        };
        let alias = ResolvedBackend {
            path: "/dev/esu/backends/sdb1".into(),
            rdev: 8,
            guard: None,
        };
        let distinct = ResolvedBackend {
            path: "/dev/esu/backends/sdc1".into(),
            rdev: 9,
            guard: None,
        };
        let mut seen = HashMap::new();
        validate_backend_identity(&mut seen, &system, first).unwrap();
        let error = validate_backend_identity(&mut seen, &alias, &vendor).unwrap_err();
        assert_eq!(error.error, "RomBackendDuplicate");
        assert_eq!(error.component.as_deref(), Some("vendor"));
        assert!(error.detail.contains("/dev/esu/backends/sda1"));
        assert!(error.detail.contains("/dev/loop7"));
        let mut separate = HashMap::new();
        validate_backend_identity(&mut separate, &system, first).unwrap();
        validate_backend_identity(&mut separate, &distinct, &vendor).unwrap();
    }

    #[test]
    fn rom_backends_accept_documented_forms_and_reject_other_devices() {
        for backend in [
            "/dev/block/by-name/system",
            "/dev/loop12",
            "/dev/mapper/lv-system",
            "esp-file:esu/backing.img",
        ] {
            let text = ROM.replace("/dev/block/by-name/system", backend);
            parse_rom(&text, "release-1").unwrap();
        }

        for (backend, error) in [
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
            ("esp-file:", "BackendUnsupportedLocation"),
            ("esp-file:/absolute", "BackendUnsupportedLocation"),
            ("esp-file:../escape", "BackendUnsupportedLocation"),
            ("esp-file:a//b", "BackendUnsupportedLocation"),
            ("relative/backend", "BackendNotAbsolute"),
            ("file:esu/backing.img", "BackendNotAbsolute"),
        ] {
            let text = ROM.replace("/dev/block/by-name/system", backend);
            let rejection = parse_rom(&text, "release-1").unwrap_err();
            assert_eq!(rejection.error, error, "{backend}");
            assert_eq!(rejection.component.as_deref(), Some("system"), "{backend}");
        }
    }

    #[test]
    fn esp_file_backends_are_read_only_on_rom_one_and_writable_later() {
        let writable = ROM
            .replace("/dev/block/by-name/system", "esp-file:esu/backing.img")
            .replace("read_only = true", "read_only = false");

        let error = parse_rom(&writable, "release-1").unwrap_err();
        assert_eq!(error.error, "RomEspFileWritable");
        assert_eq!(error.component.as_deref(), Some("system"));

        // A managed ROM >= 2 owns the ESP-file image and may write through it.
        let later = writable.replace("managed = true", "managed = true\nrom_number = 2");
        let rom = parse_rom(&later, "release-1").unwrap();
        assert_eq!(rom.rom_number, 2);
        assert!(rom.has_writable_esp_file());
        assert!(!parse_rom(ROM, "release-1").unwrap().has_writable_esp_file());
    }

    #[test]
    fn firmware_views_require_a_managed_rom_from_two_on() {
        let rom = parse_rom(FW_ROM, "release-1").unwrap();
        assert_eq!(rom.rom_number, 2);
        assert_eq!(rom.firmware_views.len(), 1);
        assert_eq!(rom.firmware_views[0].name, "xbl_a");
        assert_eq!(rom.firmware_views[0].thin_id, 131_073);

        // ROM 1 reads the physical firmware partitions directly.
        let error = parse_rom(
            &FW_ROM.replace("rom_number = 2", "rom_number = 1"),
            "release-1",
        )
        .unwrap_err();
        assert_eq!(error.error, "RomFirmwareViewsRomNumber");

        // Only a managed ROM has a projection contract for a view.
        let unmanaged = "schema_version = 1\ngeneration = \"release-1\"\nid = \"android-b\"\n\
                         rom_number = 2\nmanaged = false\n\
                         [[firmware_views]]\nname = \"xbl_a\"\nthin_id = 131073\n";
        let error = parse_rom(unmanaged, "release-1").unwrap_err();
        assert_eq!(error.error, "RomFirmwareViewsUnmanaged");
        assert_eq!(error.component.as_deref(), None);

        // Five ROMs fill the reserved id range, so a sixth never has a view.
        let error = parse_rom(
            &FW_ROM.replace("rom_number = 2", "rom_number = 6"),
            "release-1",
        )
        .unwrap_err();
        assert_eq!(error.error, "RomNumberInvalid");
    }

    #[test]
    fn firmware_view_names_and_reserved_thin_ids_are_pinned() {
        // The name must be a physical `<base>_a`/`<base>_b` PARTNAME and its base
        // must not be one the running kernel selects from the current slot.
        for name in ["xbl", "xbl_c", "boot_a", "vbmeta_vendor_b"] {
            let text = FW_ROM.replacen(
                "name = \"xbl_a\"\nthin_id",
                &format!("name = {name:?}\nthin_id"),
                1,
            );
            let error = parse_rom(&text, "release-1").unwrap_err();
            assert_eq!(error.error, "RomFirmwareViewName", "{name}");
            assert_eq!(error.component.as_deref(), Some(name));
        }

        // The id is the reserved `(rom_number << 16) | index` value, never an
        // LVM2-owned id and never another ROM's id.
        for id in ["0", "1", "131074", "16777216"] {
            let text = FW_ROM.replacen("thin_id = 131073", &format!("thin_id = {id}"), 1);
            let error = parse_rom(&text, "release-1").unwrap_err();
            assert_eq!(error.error, "RomFirmwareViewThinId", "{id}");
            assert_eq!(error.component.as_deref(), Some("xbl_a"));
        }

        // Repeating a name is refused even when every other rule would pass.
        let duplicate = FW_ROM.replace(
            "[[partitions]]",
            "[[firmware_views]]\nname = \"xbl_a\"\nthin_id = 131074\n\n[[partitions]]",
        );
        let error = parse_rom(&duplicate, "release-1").unwrap_err();
        assert_eq!(error.error, "RomFirmwareViewDuplicate");
        assert_eq!(error.component.as_deref(), Some("xbl_a"));
    }

    #[test]
    fn every_firmware_view_needs_its_own_writable_projection() {
        let prefix = FW_ROM.split("[[partitions]]").next().unwrap();
        let other = "[[partitions]]\nname = \"system\"\nbackend = \"/dev/block/by-name/system\"\nread_only = false\n";

        for text in [
            // No projection at all.
            format!("{prefix}{other}"),
            // Right name, wrong device.
            format!(
                "{prefix}[[partitions]]\nname = \"xbl_a\"\nbackend = \"/dev/block/by-name/xbl_a\"\nread_only = false\n"
            ),
            // Right device, read-only.
            FW_ROM.replace("read_only = false", "read_only = true"),
        ] {
            let error = parse_rom(&text, "release-1").unwrap_err();
            assert_eq!(error.error, "RomFirmwareViewProjection");
            assert_eq!(error.component.as_deref(), Some("xbl_a"));
        }

        // Two views are numbered by their list position, and ROM 5 owns the top
        // of the reserved range.
        let both = FW_ROM
            .replace(
                "[[partitions]]",
                "[[firmware_views]]\nname = \"tz_a\"\nthin_id = 131074\n\n[[partitions]]",
            )
            .replace(
                "read_only = false\n",
                "read_only = false\n\n[[partitions]]\nname = \"tz_a\"\nbackend = \"/dev/mapper/rom2-fw-tz_a\"\nread_only = false\n",
            );
        let rom = parse_rom(&both, "release-1").unwrap();
        assert_eq!(
            rom.firmware_views
                .iter()
                .map(|view| (view.name.as_str(), view.thin_id))
                .collect::<Vec<_>>(),
            [("xbl_a", 131_073), ("tz_a", 131_074)]
        );

        let rom = parse_rom(
            &FW_ROM
                .replacen("rom_number = 2", "rom_number = 5", 1)
                .replacen("thin_id = 131073", "thin_id = 327681", 1)
                .replacen("/dev/mapper/rom2-fw-xbl_a", "/dev/mapper/rom5-fw-xbl_a", 1),
            "release-1",
        )
        .unwrap();
        assert_eq!(rom.firmware_views[0].thin_id, 327_681);
    }

    #[test]
    fn projections_are_bounded_by_the_gpt_abi() {
        let prefix = ROM.split("[[partitions]]").next().unwrap();
        let mut rom = prefix.to_owned();

        for index in 0..crate::gpt_uapi::GPT_MAX_PROJECTIONS {
            rom.push_str(&format!(
                "[[partitions]]\nname=\"p{index}\"\nbackend=\"/dev/loop{index}\"\nread_only=true\n"
            ));
        }

        parse_rom(&rom, "release-1").unwrap();

        rom.push_str("[[partitions]]\nname=\"extra\"\nbackend=\"/dev/loop200\"\nread_only=true\n");

        let error = parse_rom(&rom, "release-1").unwrap_err();
        assert_eq!(error.error, "RomPartitionsTooMany");
        assert!(
            error
                .detail
                .contains(&crate::gpt_uapi::GPT_MAX_PROJECTIONS.to_string())
        );
    }

    #[test]
    fn manifest_paths_reject_traversal_and_malformed_components() {
        for (path, error) in [
            ("../escape", "PathTraversal"),
            ("modules/../escape", "PathTraversal"),
            ("./module.ko", "PathTraversal"),
            ("/module.ko", "PathAbsolute"),
            ("modules//module.ko", "PathEmptyComponent"),
            ("modules/", "PathTrailingSeparator"),
            ("", "PathEmpty"),
        ] {
            for text in [
                MANIFEST.replace("roms", path),
                MANIFEST.replace("modules/gpt.ko", path),
            ] {
                assert_eq!(parse_manifest(&text).unwrap_err().error, error, "{path}");
            }
        }
        parse_manifest(&MANIFEST.replace("modules/gpt.ko", "modules/gpt..ko")).unwrap();
    }

    #[test]
    fn bounded_values_accept_exact_limits_and_reject_next_byte() {
        validate_generation(&"g".repeat(MAX_GENERATION_BYTES)).unwrap();
        assert_eq!(
            validate_generation(&"g".repeat(MAX_GENERATION_BYTES + 1))
                .unwrap_err()
                .error,
            "GenerationTooLong"
        );
        for (value, error) in [
            ("", "GenerationEmpty"),
            ("é", "GenerationNotAscii"),
            ("release/1", "GenerationInvalidCharacter"),
        ] {
            assert_eq!(
                parse_manifest(&MANIFEST.replace("release-1", value))
                    .unwrap_err()
                    .error,
                error
            );
        }
        validate_module_name(&"m".repeat(MAX_NAME_BYTES)).unwrap();
        validate_partition_name(&"p".repeat(MAX_PARTITION_NAME_BYTES)).unwrap();
        assert_eq!(
            validate_module_name(&"m".repeat(MAX_NAME_BYTES + 1))
                .unwrap_err()
                .error,
            "ModuleNameTooLong"
        );
        assert_eq!(
            validate_partition_name(&"p".repeat(MAX_PARTITION_NAME_BYTES + 1))
                .unwrap_err()
                .error,
            "PartitionNameTooLong"
        );
        for name in ["", "bad/name", "module.ko", "é"] {
            assert!(validate_module_name(name).is_err());
            assert!(validate_partition_name(name).is_err());
        }
        validate_relative_path(&"p".repeat(MAX_PATH_BYTES)).unwrap();
        assert_eq!(
            validate_relative_path(&"p".repeat(MAX_PATH_BYTES + 1))
                .unwrap_err()
                .error,
            "PathTooLong"
        );
        const BY_NAME: &str = "/dev/block/by-name/";
        validate_backend_path(&format!(
            "{BY_NAME}{}",
            "p".repeat(MAX_PATH_BYTES - 1 - BY_NAME.len())
        ))
        .unwrap();
        assert_eq!(
            validate_backend_path(&format!("/{}", "p".repeat(MAX_PATH_BYTES)))
                .unwrap_err()
                .error,
            "BackendPathTooLong"
        );
        assert_eq!(
            validate_backend_path("dev/block/system").unwrap_err().error,
            "BackendNotAbsolute"
        );
        assert_eq!(
            validate_backend_path("/dev/\0bad").unwrap_err().error,
            "BackendInvalidCharacter"
        );
        validate_backend_path("/dev/loop0").unwrap();
        validate_backend_path("/dev/loop127").unwrap();
        validate_backend_path("/dev/mapper/lv-system").unwrap();
        validate_backend_path("esp-file:esu/backing.img").unwrap();
        for path in [
            "/dev/block/sda",
            "/dev/block/by-name/",
            "/dev/block/by-name/.",
            "/dev/block/by-name/..",
            "/dev/block/by-name/../escape",
            "/dev/block/by-name/system/extra",
            "/dev/mapper/",
            "/dev/mapper/a/b",
            "/dev/mapper/..",
            "/dev/loop",
            "/dev/loop0x",
            "/dev/loopx",
            "/dev/block/by-name",
            "esp-file:",
            "esp-file:/absolute",
            "esp-file:../escape",
            "esp-file:a//b",
            "esp-file:a/",
            "esp-file:.",
        ] {
            assert_eq!(
                validate_backend_path(path).unwrap_err().error,
                "BackendUnsupportedLocation",
                "{path}"
            );
        }
        validate_module_params(&"x".repeat(MAX_PARAMS_BYTES)).unwrap();
        assert_eq!(
            validate_module_params(&"x".repeat(MAX_PARAMS_BYTES + 1))
                .unwrap_err()
                .error,
            "ModuleParamsTooLong"
        );
        for params in ["a\0b", "a\nb", "a\rb"] {
            assert_eq!(
                validate_module_params(params).unwrap_err().error,
                "ModuleParamsInvalidCharacter"
            );
        }
    }

    #[test]
    fn managed_rom_rejects_gpt_placed_before_the_core_module() {
        let manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            generation: "release-1".to_owned(),
            rom: "roms".to_owned(),
            platform: None,
            modules: vec![
                ModuleEntry {
                    name: "gpt".to_owned(),
                    path: "modules/gpt.ko".to_owned(),
                    params: String::new(),
                },
                ModuleEntry {
                    name: "kernelesp".to_owned(),
                    path: "modules/kernelesp.ko".to_owned(),
                    params: String::new(),
                },
            ],
        };
        let rom = parse_rom(ROM, "release-1").unwrap();

        let error = validate_managed(&manifest, &rom).unwrap_err();
        assert_eq!(error.error, "ManifestManagedGptOrder");
        assert_eq!(error.component.as_deref(), Some("gpt"));

        // The same manifest is rejected earlier by the ordered-manifest rule.
        assert_eq!(
            validate_manifest(&manifest).unwrap_err().error,
            "ManifestCoreFirst"
        );
    }

    #[test]
    fn rejections_convert_to_configuration_failures_with_the_component() {
        let rejection = parse_rom(ROM, "Release-1").unwrap_err();
        let failure = Failure::from(rejection);
        assert_eq!(failure.stage, Stage::Configuration);
        assert_eq!(failure.error, "RomGenerationMismatch");
        assert_eq!(failure.component.as_deref(), Some("rom.toml"));
        assert!(failure.detail.contains("Release-1"));

        let unattributed = Failure::from(
            parse_manifest(&MANIFEST.replace("schema_version = 1", "schema_version = 9"))
                .unwrap_err(),
        );
        assert_eq!(unattributed.stage, Stage::Configuration);
        assert_eq!(unattributed.error, "ManifestSchemaVersion");
        assert_eq!(unattributed.component, None);

        let long = Failure::from(
            parse_manifest(&MANIFEST.replace("roms", &"p".repeat(MAX_PATH_BYTES + 50)))
                .unwrap_err(),
        );
        assert_eq!(long.error, "PathTooLong");
        assert!(long.detail.len() <= crate::receipt::MAX_DETAIL_BYTES);
    }

    #[test]
    fn resolved_backends_stay_unpublished_until_validation_succeeds() {
        let rom = parse_rom(ROM, "release-1").unwrap();
        assert!(rom.resolved_backends().is_empty());
    }

    #[test]
    fn only_an_absent_device_is_a_pending_backend_rejection() {
        let partition = PartitionEntry {
            name: "system".to_owned(),
            backend: "/dev/block/by-name/system".to_owned(),
            read_only: true,
        };

        let absent = backend_error(
            &partition,
            &io::Error::new(
                io::ErrorKind::NotFound,
                "no block device has the backend PARTNAME",
            ),
        );
        assert_eq!(absent.error, "RomBackendUnavailable");
        assert_eq!(absent.component.as_deref(), Some("system"));
        assert!(absent.detail.contains("/dev/block/by-name/system"));
        assert!(absent.is_pending());

        for detail in [
            "multiple block devices share the backend PARTNAME",
            "named backend is not a partition",
            "loop backend is not a block device",
            "sysfs device number is malformed",
            "backend must be /dev/block/by-name/<PARTNAME>, /dev/mapper/<name>, /dev/loopN, or esp-file:<relative-path>",
            "ESP file backend is sparse",
            "multiple device-mapper devices share the backend name",
        ] {
            for kind in [io::ErrorKind::InvalidInput, io::ErrorKind::PermissionDenied] {
                assert!(
                    !backend_error(&partition, &io::Error::new(kind, detail)).is_pending(),
                    "{detail}"
                );
            }
        }
    }

    #[test]
    fn strict_and_ambiguous_rejections_are_never_pending() {
        for error in [
            parse_rom(ROM, "Release-1").unwrap_err(),
            parse_rom(
                &ROM.replace("managed = true", "managed = \"true\""),
                "release-1",
            )
            .unwrap_err(),
            parse_manifest(&MANIFEST.replace("schema_version = 1", "schema_version = 9"))
                .unwrap_err(),
            validate_backend_path("/dev/block/sda").unwrap_err(),
            validate_backend_path("/dev/loopx").unwrap_err(),
        ] {
            assert!(!error.is_pending(), "{}", error.error);
        }
    }
}
