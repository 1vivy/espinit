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

/// Manifest generation limit, per the layout specification.
pub const MAX_GENERATION_BYTES: usize = 64;

/// Logical module and projected partition name limit.
pub const MAX_NAME_BYTES: usize = 64;

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
    pub rom: String,
    pub modules: Vec<ModuleEntry>,
}

/// One ordered manifest module entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleEntry {
    pub name: String,
    pub path: String,
    pub params: String,
}

/// `rom.toml`, parsed with unknown/duplicate/missing fields rejected.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RomConfig {
    pub schema_version: u64,
    pub generation: String,
    pub managed: bool,
    #[serde(default)]
    pub partitions: Vec<PartitionEntry>,
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

/// Parse and validate `manifest.toml` text.
pub fn parse_manifest(text: &str) -> Result<Manifest, ConfigError> {
    let manifest: Manifest = toml::from_str(text)
        .map_err(|error| ConfigError::new("ManifestParse", error.to_string()))?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// Parse and validate `rom.toml` text against the selected manifest generation.
pub fn parse_rom(text: &str, manifest_generation: &str) -> Result<RomConfig, ConfigError> {
    let rom: RomConfig =
        toml::from_str(text).map_err(|error| ConfigError::new("RomParse", error.to_string()))?;
    validate_rom(&rom, manifest_generation)?;
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

    validate_relative_path(&manifest.rom).map_err(|error| error.with_component("rom.toml"))?;

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

    if manifest.modules[0].name != "espinit" {
        return Err(ConfigError::at(
            "ManifestCoreFirst",
            manifest.modules[0].name.clone(),
            "the first manifest entry must be the core module `espinit`",
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
    }

    Ok(())
}

/// Enforce the managed-ROM requirement that `gpt` follows the core module.
pub fn validate_managed(manifest: &Manifest, rom: &RomConfig) -> Result<(), ConfigError> {
    if !rom.managed {
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

/// Resolve each projection backend before any projection exists: a documented
/// `/dev/block/by-name/<PARTNAME>` backend is resolved from sysfs to an owned
/// stable block node, an existing `/dev/loopN` is accepted as it is, and the
/// resolved device must be a partition or loop device that no other projection
/// uses. The resolved nodes are retained on the configuration and returned for
/// the later `gpt` APPLY; the logical `partitions` stay untouched.
///
/// Only an absent device or sysfs entry is classified pending by
/// [`ConfigError::is_pending`]; the caller may retry that within a bounded
/// window. Ambiguous, malformed, unsupported and duplicated backends fail
/// immediately, and the complete resolved set is published at once.
pub fn validate_backends(rom: &RomConfig) -> Result<&[ResolvedBackend], ConfigError> {
    if let Some(published) = rom.resolved.get() {
        return Ok(published.as_slice());
    }

    let mut backends: HashMap<u64, String> = HashMap::new();
    let mut resolved = Vec::with_capacity(rom.partitions.len());

    for partition in &rom.partitions {
        let backend =
            block::resolve(&partition.backend).map_err(|error| backend_error(partition, &error))?;

        if !is_partition_or_loop(backend.rdev) {
            return Err(ConfigError::at(
                "RomBackendWholeDevice",
                partition.name.clone(),
                format!(
                    "{} is a whole block device, not a partition or loop device",
                    partition.backend
                ),
            ));
        }

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

/// Whether the device behind `rdev` is a partition or a loop device, using
/// sysfs rather than a device-name allowlist.
fn is_partition_or_loop(rdev: u64) -> bool {
    let major = rustix::fs::major(rdev);
    let minor = rustix::fs::minor(rdev);
    let directory = format!("/sys/dev/block/{major}:{minor}");

    if std::path::Path::new(&directory).join("partition").exists() {
        return true;
    }

    std::path::Path::new(&directory).join("loop").exists()
}

/// Generation identity: nonempty ASCII letters/digits plus `.`, `_`, `-`,
/// at most 64 bytes. It identifies one coordinated payload, not a kernel
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

    if name.len() > MAX_NAME_BYTES {
        return Err(ConfigError::new(
            "PartitionNameTooLong",
            format!(
                "projected name is {} bytes, limit {MAX_NAME_BYTES}",
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

/// Relative path rooted at the ESP `/espinit` subtree: no absolute paths, no
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

/// Absolute backend path in one of the two documented forms:
/// `/dev/block/by-name/<PARTNAME>` or an existing `/dev/loopN`. Anything else is
/// rejected here, including whole logical units, offsets, arbitrary paths, and
/// symlink or traversal spellings; [`validate_backends`] resolves the device
/// itself before any projection exists.
pub fn validate_backend_path(path: &str) -> Result<(), ConfigError> {
    if !path.starts_with('/') {
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
            format!("{path} is not a /dev/block/by-name/<PARTNAME> or /dev/loopN backend"),
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
rom = "roms/rom.toml"
[[modules]]
name = "espinit"
path = "modules/espinit.ko"
params = ""
[[modules]]
name = "gpt"
path = "modules/gpt.ko"
params = "debug=0"
"#;
    const ROM: &str = r#"
schema_version = 1
generation = "release-1"
managed = true
[[partitions]]
name = "system"
backend = "/dev/block/by-name/system"
read_only = true
"#;

    #[test]
    fn valid_managed_configuration_preserves_order_and_projection() {
        let manifest = parse_manifest(MANIFEST).unwrap();
        let rom = parse_rom(ROM, &manifest.generation).unwrap();
        validate_managed(&manifest, &rom).unwrap();
        assert_eq!(manifest.rom, "roms/rom.toml");
        assert_eq!(
            manifest
                .modules
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            ["espinit", "gpt"]
        );
        assert_eq!(manifest.modules[1].params, "debug=0");
        assert_eq!(rom.partitions[0].backend, "/dev/block/by-name/system");
        assert!(rom.partitions[0].read_only);
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
                MANIFEST.replace("name = \"espinit\"", "name = \"other\""),
                "ManifestCoreFirst",
            ),
            (
                MANIFEST.replace("name = \"gpt\"", "name = \"espinit\""),
                "ManifestDuplicateModule",
            ),
            (
                "schema_version=1\ngeneration=\"release-1\"\nrom=\"rom.toml\"\nmodules=[]".into(),
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
        validate_managed(&manifest, &unmanaged).unwrap();
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
            path: "/dev/espinit/backends/sda1".into(),
            rdev: 8,
        };
        let alias = ResolvedBackend {
            path: "/dev/espinit/backends/sdb1".into(),
            rdev: 8,
        };
        let distinct = ResolvedBackend {
            path: "/dev/espinit/backends/sdc1".into(),
            rdev: 9,
        };
        let mut seen = HashMap::new();
        validate_backend_identity(&mut seen, &system, first).unwrap();
        let error = validate_backend_identity(&mut seen, &alias, &vendor).unwrap_err();
        assert_eq!(error.error, "RomBackendDuplicate");
        assert_eq!(error.component.as_deref(), Some("vendor"));
        assert!(error.detail.contains("/dev/espinit/backends/sda1"));
        assert!(error.detail.contains("/dev/loop7"));
        let mut separate = HashMap::new();
        validate_backend_identity(&mut separate, &system, first).unwrap();
        validate_backend_identity(&mut separate, &distinct, &vendor).unwrap();
    }

    #[test]
    fn rom_backends_accept_documented_forms_and_reject_other_devices() {
        for backend in ["/dev/block/by-name/system", "/dev/loop12"] {
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
            ("/dev/mapper/vendor", "BackendUnsupportedLocation"),
            ("/dev/loop", "BackendUnsupportedLocation"),
            ("relative/backend", "BackendNotAbsolute"),
        ] {
            let text = ROM.replace("/dev/block/by-name/system", backend);
            let rejection = parse_rom(&text, "release-1").unwrap_err();
            assert_eq!(rejection.error, error, "{backend}");
            assert_eq!(rejection.component.as_deref(), Some("system"), "{backend}");
        }
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
                MANIFEST.replace("roms/rom.toml", path),
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
        validate_partition_name(&"p".repeat(MAX_NAME_BYTES)).unwrap();
        assert_eq!(
            validate_module_name(&"m".repeat(MAX_NAME_BYTES + 1))
                .unwrap_err()
                .error,
            "ModuleNameTooLong"
        );
        assert_eq!(
            validate_partition_name(&"p".repeat(MAX_NAME_BYTES + 1))
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
        for path in [
            "/dev/block/sda",
            "/dev/block/by-name/",
            "/dev/block/by-name/.",
            "/dev/block/by-name/..",
            "/dev/block/by-name/../escape",
            "/dev/block/by-name/system/extra",
            "/dev/loop",
            "/dev/loop0x",
            "/dev/loopx",
            "/dev/block/by-name",
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
            rom: "roms/rom.toml".to_owned(),
            modules: vec![
                ModuleEntry {
                    name: "gpt".to_owned(),
                    path: "modules/gpt.ko".to_owned(),
                    params: String::new(),
                },
                ModuleEntry {
                    name: "espinit".to_owned(),
                    path: "modules/espinit.ko".to_owned(),
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
            parse_manifest(&MANIFEST.replace("roms/rom.toml", &"p".repeat(MAX_PATH_BYTES + 50)))
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
            "backend must be /dev/block/by-name/<PARTNAME> or /dev/loopN",
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
