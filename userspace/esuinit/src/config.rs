//! Linux-side validation of the managed-boot configuration.
//!
//! The schema, its limits and every pure validation live in the shared
//! `esu-config` crate; this module re-exports them and adds only what the
//! running device needs: backend resolution and the resolved-backend state
//! retained for the `gpt` APPLY.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::ops::Deref;
use std::sync::OnceLock;

use crate::block::{self, ResolvedBackend};
use crate::receipt::{Failure, Stage};

pub use esu_config::{
    Backend, FirmwareView, MAX_ROM_NUMBER, Manifest, ModuleEntry, PartitionEntry, SCHEMA_VERSION,
    parse_manifest, validate_bootstrap, validate_managed, validate_manifest,
};

/// The `gpt` ABI carries this many label bytes and this many projections, and
/// DeviceInfo admits five ROM numbers: the shared crate's limits must never
/// drift from those interfaces.
const _: () = assert!(esu_config::MAX_PARTITION_NAME_BYTES == crate::gpt_uapi::GPT_LABEL_BYTES);
const _: () = assert!(esu_config::MAX_PROJECTIONS == crate::gpt_uapi::GPT_MAX_PROJECTIONS);
const _: () = assert!(esu_config::MAX_ROM_NUMBER == esu_platform::efivars::MAX_ROM_NUMBER);

/// A backend-layer rejection, or a shared-schema rejection carried into the
/// early-boot failure receipt. `error` is the stable identifier, `component` the
/// attributable file, module, partition or view, and `detail` a bounded
/// diagnostic string.
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

    fn with_pending(mut self) -> Self {
        self.pending = true;
        self
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

impl From<esu_config::Error> for ConfigError {
    fn from(error: esu_config::Error) -> Self {
        let detail = error.to_string();

        Self {
            error: error.code,
            component: (!error.component.is_empty()).then_some(error.component),
            detail,
            pending: false,
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

/// The selected ROM configuration plus the backend state only the running boot
/// can produce.
///
/// The schema itself is [`esu_config::RomConfig`], reached through [`Deref`], so
/// the projection list, the firmware views and every validated field are the
/// shared contract, while the resolved block nodes stay with this boot.
#[derive(Debug)]
pub struct RomConfig {
    config: esu_config::RomConfig,
    /// Stable block nodes resolved by [`validate_backends`], in document order.
    /// Never read from the file: the logical `partitions` stay unchanged.
    resolved: OnceLock<Vec<ResolvedBackend>>,
}

impl RomConfig {
    /// Wrap a parsed configuration; nothing is resolved yet.
    fn new(config: esu_config::RomConfig) -> Self {
        Self {
            config,
            resolved: OnceLock::new(),
        }
    }

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
        self.partitions.iter().any(|partition| {
            !partition.read_only && matches!(partition.backend(), Ok(Backend::EspFile(_)))
        })
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

impl Deref for RomConfig {
    type Target = esu_config::RomConfig;

    fn deref(&self) -> &Self::Target {
        &self.config
    }
}

/// Parse and structurally validate a ROM; runtime callers validate its number
/// separately.
pub fn parse_rom(text: &str) -> Result<RomConfig, esu_config::Error> {
    esu_config::parse_rom(text).map(RomConfig::new)
}

/// Parse a ROM and require it to be the selected one.
pub fn parse_selected_rom(text: &str, id: &str) -> Result<RomConfig, esu_config::Error> {
    esu_config::parse_selected_rom(text, id).map(RomConfig::new)
}

/// Validate ROM identity-dependent projection rules with the bdsvars number.
pub fn validate_rom(rom: &RomConfig, rom_number: u32) -> Result<(), esu_config::Error> {
    esu_config::validate_rom(rom, rom_number)
}

/// ESP-relative path of the selected ROM configuration, validated before any
/// file read.
pub fn rom_path(manifest: &Manifest, id: &str) -> Result<String, ConfigError> {
    validate_rom_id(id)?;

    let path = esu_config::rom_config_path(manifest, id);
    esu_config::validate_relative_path(&path).map_err(ConfigError::from)?;
    Ok(path)
}

/// ROM id: bounded to the installed efivar identity and restricted to the
/// identifier alphabet, so a selection can never escape the payload root.
fn validate_rom_id(id: &str) -> Result<(), ConfigError> {
    if esu_config::rom_id(id) {
        return Ok(());
    }

    Err(ConfigError::new(
        "RomIdInvalid",
        format!("invalid ROM id {id:?}"),
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
schema_version = 1
rom = "roms"
modules_order = ["thin", "fw-views"]
[[modules]]
name = "kernelesp"
path = "lib/kernelesp.ko"
params = ""
[[modules]]
name = "gpt"
path = "lib/gpt.ko"
params = "debug=0"
[[modules]]
name = "efivarfs"
path = "lib/efivarfs.ko"
params = "dev=by-name:bdsvars"
"#;
    const ROM: &str = r#"
schema_version = 1
id = "rom1"
managed = true
[[partitions]]
name = "system"
backend = "/dev/block/by-name/system"
read_only = true
"#;

    /// Parse a ROM and validate its number, as the managed boot does.
    fn numbered(text: &str, number: u32) -> Result<RomConfig, esu_config::Error> {
        let rom = parse_rom(text)?;
        validate_rom(&rom, number)?;
        Ok(rom)
    }

    #[test]
    fn resolved_backend_identity_rejects_aliases_but_not_distinct_devices() {
        let rom = parse_rom(ROM).unwrap();
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

    /// Linux ESP lifecycle contract: exactly a writable `esp-file:` projection
    /// needs the read-write ESP mount, so the predicate must see only
    /// non-read-only ESP files and never another backend.
    #[test]
    fn only_a_writable_esp_file_projection_needs_the_read_write_esp_mount() {
        let writable = ROM
            .replace("/dev/block/by-name/system", "esp-file:esu/backing.img")
            .replace("read_only = true", "read_only = false");
        assert!(numbered(&writable, 2).unwrap().has_writable_esp_file());

        let read_only = ROM.replace("/dev/block/by-name/system", "esp-file:esu/backing.img");
        assert!(!numbered(&read_only, 2).unwrap().has_writable_esp_file());

        let mapper = ROM
            .replace("/dev/block/by-name/system", "/dev/mapper/rom2-system")
            .replace("read_only = true", "read_only = false");
        assert!(!numbered(&mapper, 2).unwrap().has_writable_esp_file());

        assert!(!numbered(ROM, 2).unwrap().has_writable_esp_file());
    }

    /// Linux selection contract: the ROM file comes from the manifest's ROM
    /// directory plus the id, and an id outside the identifier alphabet is
    /// refused before any path is built, so a selection can never escape the
    /// payload root.
    #[test]
    fn rom_path_validates_the_selected_id_before_building_the_path() {
        let manifest = parse_manifest(MANIFEST).unwrap();
        assert_eq!(rom_path(&manifest, "rom1").unwrap(), "roms/rom1.toml");

        for id in ["", ".", "..", "../rom1", "rom/1", &"x".repeat(60)] {
            let error = rom_path(&manifest, id).unwrap_err();
            assert_eq!(error.error, "RomIdInvalid", "{id}");
        }
    }

    /// Early-boot failure contract: a shared-schema rejection becomes a
    /// configuration failure carrying the schema code and its component, with a
    /// bounded diagnostic, so the ESP receipt classifies the same failure the
    /// payload declared.
    #[test]
    fn rejections_convert_to_configuration_failures_with_the_component() {
        let rejection =
            esu_config::parse_rom(&ROM.replace("schema_version = 1", "schema_version = 0"))
                .unwrap_err();
        let failure = Failure::from(rejection);
        assert_eq!(failure.stage, Stage::Configuration);
        assert_eq!(failure.error, "RomSchemaVersion");
        assert_eq!(failure.component.as_deref(), Some("rom.toml"));

        let unattributed = Failure::from(
            esu_config::parse_manifest(
                &MANIFEST.replace("schema_version = 1", "schema_version = 9"),
            )
            .unwrap_err(),
        );
        assert_eq!(unattributed.stage, Stage::Configuration);
        assert_eq!(unattributed.error, "ManifestSchemaVersion");
        assert_eq!(unattributed.component, None);

        let long = Failure::from(
            esu_config::parse_manifest(&MANIFEST.replace("roms", &"p".repeat(4250))).unwrap_err(),
        );
        assert_eq!(long.error, "PathTooLong");
        assert!(long.detail.len() <= crate::receipt::MAX_DETAIL_BYTES);
    }

    /// Enumeration contract: only an absent device is pending, so a malformed
    /// configuration or an unsupported backend spelling fails immediately
    /// instead of being retried into a boot.
    #[test]
    fn strict_rejections_are_immediate_configuration_failures() {
        for (error, code) in [
            (
                esu_config::parse_rom(&ROM.replace("schema_version = 1", "schema_version = 0"))
                    .unwrap_err(),
                "RomSchemaVersion",
            ),
            (
                esu_config::parse_rom(&ROM.replace("managed = true", "managed = \"true\""))
                    .unwrap_err(),
                "RomParse",
            ),
            (
                esu_config::parse_manifest(
                    &MANIFEST.replace("schema_version = 1", "schema_version = 9"),
                )
                .unwrap_err(),
                "ManifestSchemaVersion",
            ),
            (
                esu_config::parse_backend("/dev/block/sda").unwrap_err(),
                "BackendUnsupportedLocation",
            ),
            (
                esu_config::parse_backend("/dev/loopx").unwrap_err(),
                "BackendUnsupportedLocation",
            ),
        ] {
            let failure = Failure::from(error);
            assert_eq!(failure.stage, Stage::Configuration);
            assert_eq!(failure.error, code);
        }
    }
}
