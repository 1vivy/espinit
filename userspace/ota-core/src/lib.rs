// SPDX-License-Identifier: GPL-3.0-only
//! The primitives every OTA participant shares.
//!
//! The staging transaction is one small vocabulary used by four different
//! programs: the boot HAL seals an update and promotes it, `ota-stage` publishes
//! the switch devices at PID 1, esuinit resolves the running slot's backends and
//! esud publishes the device-name tree. Surfacer boots the staged letter from the
//! same names. This crate is the one place those names, the base-image geometry,
//! the KMI/ARB/`efisp` probes and the takeover overlay are derived, so a change
//! lands in all of them at once instead of drifting.
//!
//! Nothing here touches a device, a mount or an efivarfs variable: every function
//! is either pure (names, the overlay, the ARB and KMI scans) or takes the file
//! handles and paths the caller already opened.
//!
//! The `Stage-<id>` transaction record itself lives in `esu-platform`
//! ([`esu_platform::stage`]) next to the other efivarfs records; this crate
//! consumes it through [`esu_platform::efivars`].

pub mod abl;
pub mod arb;
pub mod copy;
pub mod kmi;
pub mod modules;
pub mod names;
pub mod overlay;
pub mod switch;

pub use abl::{EFISP, abl_has_efisp, loader_has_efisp};
pub use arb::Arb;
pub use copy::{IMAGE_ALIGNMENT, SECTOR_SIZE, copy_range, exact_sectors, verify_equal};
pub use kmi::{Kmi, kmi_from_boot, parse_banner};
pub use modules::{Module, ModuleSet, select_module_set, set_dir};
pub use names::{
    ESD_BY_NAME, ESD_ETC, ESD_LOCK, ESD_LV, ESD_MAPPER, ESD_PV, ESD_ROOT, ESD_RUN, POOL_TDATA, VG,
    committed_payload, esd_by_name, esd_lv, esd_mapper, ota_dm_name, stage_dm_name, stage_lv_name,
    stage_payload,
};
pub use overlay::{BUILD_ID, LZ4_BLOCK_SIZE, LZ4_LEGACY_MAGIC, build_overlay, legacy_lz4};
pub use switch::{error_target, linear};
