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

use esu_config::base_image_path;
use esu_platform::stage::StageState;

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

/// The letters of the OTA transaction as this boot derives them.
///
/// `current` is the letter this boot runs from (`androidboot.slot_suffix`),
/// `state` the dispatched ROM's `Stage-<id>` record, and `selected` the letter
/// its `Slot-<id>` record selected — `None` for a ROM 1 boot, where PID 1 reads
/// neither a switch letter nor that byte. The ROM number scopes every rule: a
/// ROM 1 has no staged letter, an empty `ESU_STAGE` and no image role, so it
/// never turns a letter into a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Letters {
    current: u8,
    state: StageState,
    selected: Option<u8>,
    rom_number: u32,
}

/// The backend one `<base>_<x>` image partition is projected from this boot.
/// Both forms are block devices for the `gpt` APPLY; only the switch device is
/// ever written, and only while it serves the letter that is not booted.
#[derive(Debug, PartialEq, Eq)]
pub enum ImageBackend {
    /// The per-base switch device `rom<N>-ota-<base>`: linear over the active
    /// staging LV while an update is staged, an error target of the base's
    /// exact size otherwise. Read-only exactly while it serves the booted
    /// letter, so the bytes the running slot booted cannot change under it.
    Switch { name: String, read_only: bool },
    /// The base's ESP image, read-only, at the ESP-root-relative
    /// [`esu_config::base_image_path`].
    Esp { path: String },
}

/// The other letter of a two-slot device.
fn other(letter: u8) -> u8 {
    letter ^ 1
}

/// A letter as its one-character slot spelling, or `None` outside the two slots.
fn slot_letter(letter: u8) -> Option<&'static str> {
    match letter {
        0 => Some("a"),
        1 => Some("b"),
        _ => None,
    }
}

impl Letters {
    /// Derive the letters of one managed boot. `current` and a `selected` letter
    /// are `0` for `_a` and `1` for `_b`.
    pub fn derive(current: u8, state: StageState, selected: Option<u8>, rom_number: u32) -> Self {
        Self {
            current,
            state,
            selected,
            rom_number,
        }
    }

    /// The letter a staged set runs from: the other letter while an update is
    /// staged, the selected letter once it is sealed or being promoted, and
    /// nothing while no transaction is in flight.
    pub fn staged(&self) -> Option<u8> {
        match self.state {
            StageState::None => None,
            StageState::Staging => Some(other(self.current)),
            StageState::Sealed | StageState::Promote => self.selected,
        }
    }

    /// The letter the switch device serves: the staged letter while a
    /// transaction is in flight, the other letter while idle. The non-booted
    /// letter is always the switch device, so an updater's writes reach
    /// whatever the boot HAL routes there.
    pub fn switch(&self) -> u8 {
        self.staged().unwrap_or_else(|| other(self.current))
    }

    /// `ESU_STAGE`, exported to the PID-1 module scripts and their helpers next
    /// to `ESU_ROM`/`ESU_ROM_NUMBER`: the empty string when nothing is staged or
    /// the ROM is 1, otherwise `<a|b>:<ro|rw>`, read-only exactly for a staged
    /// letter that is also the booted one.
    pub fn esu_stage(&self) -> String {
        if self.rom_number < 2 {
            return String::new();
        }

        let Some(letter) = self.staged() else {
            return String::new();
        };
        let access = if letter == self.current { "ro" } else { "rw" };

        format!(
            "{}:{access}",
            slot_letter(letter).expect("a derived letter is one of the two slots")
        )
    }

    /// The backend of the image partition `name` carrying `base`.
    ///
    /// `name` must be exactly `<base>_a` or `<base>_b`. The switch letter is the
    /// switch device, read-only exactly when it serves the booted letter; the
    /// other letter is the base's ESP image file, read-only, so an updater
    /// always writes through the switch device whatever letter the boot HAL
    /// routes there. A ROM 1 must not use an image role at all: it reads the
    /// physical partitions, which is the install-time admission's own rule and
    /// carries its identifier.
    pub fn image_backend(
        &self,
        id: &str,
        name: &str,
        base: &str,
    ) -> Result<ImageBackend, ConfigError> {
        if self.rom_number < 2 {
            return Err(ConfigError::at(
                "KernelSetBackend",
                name,
                "ROM 1 reads the physical partition, not an image role",
            ));
        }

        let letter = if name == format!("{base}_a") {
            0
        } else if name == format!("{base}_b") {
            1
        } else {
            return Err(ConfigError::at(
                "KernelSetBackend",
                name,
                format!("rom-image:{base} does not name <base>_a or <base>_b"),
            ));
        };

        if letter == self.switch() {
            Ok(ImageBackend::Switch {
                name: ota_core::ota_dm_name(self.rom_number, base),
                read_only: letter == self.current,
            })
        } else {
            Ok(ImageBackend::Esp {
                path: base_image_path(id, base),
            })
        }
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
/// holds the ESP read-write. An image role `rom-image:<base>` is resolved
/// through the derived letters instead ([`Letters::image_backend`]): the switch
/// device a sibling PID-1 helper published, or the base's read-only ESP image,
/// so the ESP access mode of an image role is never writable and
/// [`RomConfig::has_writable_esp_file`] keeps its meaning. Each accepted form
/// establishes its own allowed device kind and access, so a whole logical unit,
/// a mismatched ESP file and every other path are rejected. The resolved set is
/// retained on the configuration and returned for the `gpt` APPLY, which keeps
/// every loop guard open, while the logical `partitions` stay untouched.
///
/// Only an absent device or sysfs entry is classified pending by
/// [`ConfigError::is_pending`]; the caller may retry that within a bounded
/// window. Ambiguous, malformed, unsupported and duplicated backends fail
/// immediately, and the complete resolved set is published at once.
pub fn validate_backends<'a>(
    rom: &'a RomConfig,
    esp_mount: &str,
    letters: Letters,
) -> Result<&'a [ResolvedBackend], ConfigError> {
    if let Some(published) = rom.resolved.get() {
        return Ok(published.as_slice());
    }

    let mut backends: HashMap<u64, String> = HashMap::new();
    let mut resolved = Vec::with_capacity(rom.partitions.len());

    for partition in &rom.partitions {
        let backend = resolve_partition(rom, partition, esp_mount, letters)?;

        validate_backend_identity(&mut backends, &backend, partition)?;

        resolved.push(backend);
    }

    // Publish the complete set at once, only after every projection resolved.
    // A re-entry (bounded retry or a later caller) reuses the published set.
    let _ = rom.resolved.set(resolved);

    Ok(rom.resolved_backends())
}

/// Resolve one projection to its stable block node. A `rom-image:<base>` role
/// goes through the derived letters; every other backend is resolved exactly as
/// the partition declares it with the access the projection requests.
fn resolve_partition(
    rom: &RomConfig,
    partition: &PartitionEntry,
    esp_mount: &str,
    letters: Letters,
) -> Result<ResolvedBackend, ConfigError> {
    let Ok(Backend::RomImage(base)) = partition.backend() else {
        let access = if partition.read_only {
            block::Access::ReadOnly
        } else {
            block::Access::Writable
        };

        return block::resolve(&partition.backend, esp_mount, access)
            .map_err(|error| backend_error(partition, &partition.backend, &error));
    };

    match letters.image_backend(&rom.id, &partition.name, base)? {
        ImageBackend::Switch { name, read_only } => {
            let backend = format!("/dev/mapper/{name}");
            let access = if read_only {
                block::Access::ReadOnly
            } else {
                block::Access::Writable
            };

            block::resolve(&backend, esp_mount, access)
                .map_err(|error| image_error(partition, &backend, &error))
        }
        ImageBackend::Esp { path } => {
            let backend = format!("esp-file:{path}");

            block::resolve(&backend, esp_mount, block::Access::ReadOnly)
                .map_err(|error| backend_error(partition, &backend, &error))
        }
    }
}

/// Classify one backend resolution failure. Only an absent device or sysfs
/// entry ([`block::is_pending`]) may appear once enumeration finishes; every
/// other resolution failure stops boot immediately.
fn backend_error(partition: &PartitionEntry, backend: &str, error: &io::Error) -> ConfigError {
    let rejected = ConfigError::at(
        "RomBackendUnavailable",
        partition.name.clone(),
        format!("{backend}: {error}"),
    );

    if block::is_pending(error) {
        rejected.with_pending()
    } else {
        rejected
    }
}

/// Classify a switch-device resolution failure. The device is created by the
/// `ota` module's PID-1 helper, so an absent mapper is pending and is retried
/// within the same bounded window as every other helper-created mapper; when it
/// is still absent at the deadline the image role is unresolved and the boot
/// stops instead of projecting another device. The resolved device is the switch
/// the HAL reloads, so no other backend form is ever a substitute.
fn image_error(partition: &PartitionEntry, backend: &str, error: &io::Error) -> ConfigError {
    let rejected = ConfigError::at(
        "RomImageUnresolved",
        partition.name.clone(),
        format!("{backend}: {error}"),
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
params = ""
[[modules]]
name = "efivar_store"
path = "lib/efivar_store.ko"
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
            metadata: None,
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
            metadata: None,
        };

        let absent = backend_error(
            &partition,
            "/dev/block/by-name/system",
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
                    !backend_error(
                        &partition,
                        "/dev/block/by-name/system",
                        &io::Error::new(kind, detail)
                    )
                    .is_pending(),
                    "{detail}"
                );
            }
        }
    }

    /// The switch device is published by a PID-1 helper like every other mapper
    /// this boot resolves, so its absence stays a bounded pending rejection, and
    /// at the deadline it is the role that is unresolved — never another
    /// device.
    #[test]
    fn an_absent_switch_device_is_a_pending_unresolved_image_role() {
        let partition = PartitionEntry {
            name: "boot_b".to_owned(),
            backend: "rom-image:boot".to_owned(),
            read_only: false,
            metadata: None,
        };

        let absent = image_error(
            &partition,
            "/dev/mapper/rom2-ota-boot",
            &io::Error::new(io::ErrorKind::NotFound, "no such device"),
        );
        assert_eq!(absent.error, "RomImageUnresolved");
        assert_eq!(absent.component.as_deref(), Some("boot_b"));
        assert!(absent.detail.contains("/dev/mapper/rom2-ota-boot"));
        assert!(absent.is_pending());

        let ambiguous = image_error(
            &partition,
            "/dev/mapper/rom2-ota-boot",
            &io::Error::new(
                io::ErrorKind::InvalidInput,
                "multiple device-mapper devices share the backend name",
            ),
        );
        assert_eq!(ambiguous.error, "RomImageUnresolved");
        assert!(!ambiguous.is_pending());
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

        // An image role is a switch device or a read-only ESP image, so even a
        // writable `rom-image:` projection keeps the ESP mounted read-only.
        let image = ROM
            .replace("name = \"system\"", "name = \"boot_b\"")
            .replace("/dev/block/by-name/system", "rom-image:boot")
            .replace("read_only = true", "read_only = false");
        assert!(!numbered(&image, 2).unwrap().has_writable_esp_file());
    }

    /// Letters contract (C1/C2): the staged letter, the switch letter, the
    /// exported `ESU_STAGE` and the backend of each of a base's two partitions
    /// for every state of the transaction.
    #[test]
    fn image_roles_follow_the_derived_letters() {
        // Idle: the other letter is the switch device (an error target until the
        // HAL prepares one), writable, and the booted letter reads the ESP image
        // read-only.
        let idle = Letters::derive(0, StageState::None, Some(0), 2);
        assert_eq!(idle.staged(), None);
        assert_eq!(idle.switch(), 1);
        assert_eq!(idle.esu_stage(), "");
        assert_eq!(
            idle.image_backend("rom2", "boot_b", "boot").unwrap(),
            ImageBackend::Switch {
                name: "rom2-ota-boot".into(),
                read_only: false,
            }
        );
        assert_eq!(
            idle.image_backend("rom2", "boot_a", "boot").unwrap(),
            ImageBackend::Esp {
                path: "rom/rom2/boot.img".into(),
            }
        );

        // Staging: the staged letter is the other one, so the switch device is
        // the staging LV and an updater's writes reach it.
        let staging = Letters::derive(0, StageState::Staging, Some(0), 2);
        assert_eq!(staging.staged(), Some(1));
        assert_eq!(staging.switch(), 1);
        assert_eq!(staging.esu_stage(), "b:rw");
        assert_eq!(
            staging.image_backend("rom2", "boot_b", "boot").unwrap(),
            ImageBackend::Switch {
                name: "rom2-ota-boot".into(),
                read_only: false,
            }
        );
        assert_eq!(
            staging.image_backend("rom2", "boot_a", "boot").unwrap(),
            ImageBackend::Esp {
                path: "rom/rom2/boot.img".into(),
            }
        );

        // Sealed with the booted letter selected: the switch device serves the
        // running slot read-only, so the bytes it booted cannot change under it.
        let sealed_current = Letters::derive(0, StageState::Sealed, Some(0), 2);
        assert_eq!(sealed_current.staged(), Some(0));
        assert_eq!(sealed_current.switch(), 0);
        assert_eq!(sealed_current.esu_stage(), "a:ro");
        assert_eq!(
            sealed_current
                .image_backend("rom2", "boot_a", "boot")
                .unwrap(),
            ImageBackend::Switch {
                name: "rom2-ota-boot".into(),
                read_only: true,
            }
        );
        assert_eq!(
            sealed_current
                .image_backend("rom2", "boot_b", "boot")
                .unwrap(),
            ImageBackend::Esp {
                path: "rom/rom2/boot.img".into(),
            }
        );

        // Sealed for the other letter: the staged set is written through the
        // switch device and the running slot keeps reading the ESP image.
        let sealed_other = Letters::derive(0, StageState::Sealed, Some(1), 2);
        assert_eq!(sealed_other.staged(), Some(1));
        assert_eq!(sealed_other.esu_stage(), "b:rw");
        assert_eq!(
            sealed_other
                .image_backend("rom2", "boot_b", "boot")
                .unwrap(),
            ImageBackend::Switch {
                name: "rom2-ota-boot".into(),
                read_only: false,
            }
        );
        assert_eq!(
            sealed_other
                .image_backend("rom2", "boot_a", "boot")
                .unwrap(),
            ImageBackend::Esp {
                path: "rom/rom2/boot.img".into(),
            }
        );

        // Promote follows the selected letter exactly as Sealed does, and its
        // switch device is read-only while it serves the booted letter.
        let promote = Letters::derive(1, StageState::Promote, Some(1), 3);
        assert_eq!(promote.staged(), Some(1));
        assert_eq!(promote.switch(), 1);
        assert_eq!(promote.esu_stage(), "b:ro");
        assert_eq!(
            promote.image_backend("rom3", "boot_b", "boot").unwrap(),
            ImageBackend::Switch {
                name: "rom3-ota-boot".into(),
                read_only: true,
            }
        );
        assert_eq!(
            promote.image_backend("rom3", "boot_a", "boot").unwrap(),
            ImageBackend::Esp {
                path: "rom/rom3/boot.img".into(),
            }
        );
    }

    /// A ROM 1 boots the physical partitions: every state keeps the empty
    /// `ESU_STAGE` and refuses an image role instead of resolving a device.
    #[test]
    fn a_rom_one_never_derives_a_staged_letter_or_an_image_role() {
        for letters in [
            Letters::derive(0, StageState::None, None, 1),
            Letters::derive(0, StageState::Staging, None, 1),
            Letters::derive(1, StageState::Sealed, None, 1),
            Letters::derive(1, StageState::Promote, None, 1),
        ] {
            assert_eq!(letters.esu_stage(), "");
            assert_eq!(
                letters
                    .image_backend("rom1", "boot_a", "boot")
                    .unwrap_err()
                    .error,
                "KernelSetBackend"
            );
        }
    }

    /// An image role must name its own base's partition: an image declared for
    /// the wrong base, or a partition that is not a slot of any image, is the
    /// install-time admission's rejection and never a guessed device.
    #[test]
    fn an_image_role_must_name_its_own_base_partition() {
        let letters = Letters::derive(0, StageState::None, Some(0), 2);

        for name in ["boot", "boot_c", "boot_b_extra", "xbl_config_b"] {
            let error = letters.image_backend("rom2", name, "boot").unwrap_err();
            assert_eq!(error.error, "KernelSetBackend", "{name}");
            assert_eq!(error.component.as_deref(), Some(name));
        }

        assert!(letters.image_backend("rom2", "boot_a", "boot").is_ok());
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
