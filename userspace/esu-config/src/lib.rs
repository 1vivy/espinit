//! The installed esu configuration schema shared by Surfacer, provisioning and
//! the public esu PID 1: `esu/manifest.toml` and `esu/<rom-dir>/<id>.toml`.
//!
//! Parsing and validation are pure: this crate performs no device, sysfs,
//! efivarfs or filesystem access, and holds no state beyond the values it
//! returns. Linux backend resolution, the ESP read-write lifecycle and the
//! PID-1 identity bootstrap stay in espinit's `esuinit`, which consumes these
//! types.

mod installed;
mod schema;

pub use installed::{InstalledConfig, KernelImage, KernelImages, Slot, parse_installed};
pub use schema::{
    Backend, FirmwareView, Manifest, ModuleEntry, PartitionEntry, RomConfig, parse_backend,
    parse_manifest, parse_rom, parse_selected_rom, rom_config_path, validate_bootstrap,
    validate_managed, validate_manifest, validate_module_name, validate_module_params,
    validate_partition_name, validate_relative_path, validate_rom,
};

use std::fmt;

/// Installed manifest and ROM schema version.
pub const SCHEMA_VERSION: u64 = 1;
/// Highest managed ROM number. DeviceInfo's per-ROM rollback windows admit
/// exactly this many isolated ROMs, so a larger number could never be isolated.
pub const MAX_ROM_NUMBER: u32 = 5;
/// ROM identifier limit: the installed efivar identity carries this many bytes.
pub const MAX_ROM_ID_BYTES: usize = 59;
/// Identifier limit shared by module names, module-order IDs and path
/// components.
pub const MAX_IDENTIFIER_BYTES: usize = 64;
/// Limit of an ESP-root-relative path.
pub const MAX_PATH_BYTES: usize = 4096;
/// Kernel module parameter string limit enforced by the kernel loader.
pub const MAX_PARAMS_BYTES: usize = 1024;
/// Projections carried by one GPT APPLY.
pub const MAX_PROJECTIONS: usize = 128;
/// Projected partition label limit: the `gpt` ABI carries this many label bytes
/// plus the terminating NUL, so a longer name could never be projected.
pub const MAX_PARTITION_NAME_BYTES: usize = 36;
/// Ordered AVB kernel image bases. Every ROM `>= 2` carries both slot suffixes
/// of each base as managed ESP kernel images; a ROM never shadows a physical
/// `<base>_a`/`<base>_b` partition, because the running kernel already chose
/// the slot it booted from.
pub const KERNEL_SET_BASES: [&str; 7] = [
    "boot",
    "init_boot",
    "vendor_boot",
    "dtbo",
    "vbmeta",
    "vbmeta_system",
    "vbmeta_vendor",
];

/// A configuration rejection: the stable failure identifier and the component
/// it is attributable to.
///
/// `code` reuses the espinit PID-1 failure names (`RomParse`, `RomIdInvalid`,
/// `ManifestCoreFirst`, `PathTraversal`, ...), so a rejection keeps the same
/// classification on every consumer. `component` names the subject of the
/// rejection (a module, partition, firmware view, ROM id, module index or file
/// name) and is empty when no single subject is attributable. [`fmt::Display`]
/// renders `"<code>: <component>"`, or the bare code when `component` is empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub code: &'static str,
    pub component: String,
}

impl Error {
    /// A rejection with no attributable component.
    pub(crate) fn new(code: &'static str) -> Self {
        Self {
            code,
            component: String::new(),
        }
    }

    /// A rejection attributable to one component.
    pub(crate) fn at(code: &'static str, component: impl Into<String>) -> Self {
        Self {
            code,
            component: component.into(),
        }
    }

    /// Attach a component when the rejection does not carry one yet.
    pub(crate) fn with_component(mut self, component: impl Into<String>) -> Self {
        if self.component.is_empty() {
            self.component = component.into();
        }
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.component.is_empty() {
            formatter.write_str(self.code)
        } else {
            write!(formatter, "{}: {}", self.code, self.component)
        }
    }
}

impl std::error::Error for Error {}

/// Identifier: 1..=64 bytes of `[A-Za-z0-9._-]`, never `.` or `..`.
pub fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_BYTES
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// ROM id: an [`identifier`] no longer than 59 bytes.
pub fn rom_id(value: &str) -> bool {
    value.len() <= MAX_ROM_ID_BYTES && identifier(value)
}

/// ESP-root-relative path: 1..=4096 bytes, `/`-separated, every component an
/// [`identifier`]. Absolute paths, trailing separators, empty components and
/// `.`/`..` components are all rejected, because no component is an identifier.
pub fn relative_path(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_PATH_BYTES && value.split('/').all(identifier)
}
