// SPDX-License-Identifier: GPL-3.0-only
//! The names of the OTA staging objects, and the device-name tree they live in.
//!
//! These strings are a cross-component contract: the boot HAL creates and
//! reloads the per-base switch devices, `ota-stage` publishes them at PID 1,
//! esuinit resolves the running slot's backends through them, esud publishes the
//! nodes they point at and Surfacer boots the staged letter from the staging
//! LVs. They are therefore derived here once instead of being spelled out at
//! each site.
//!
//! Every input is a validated identifier: base names come from
//! `esu_config::IMAGE_BASES` through the ROM config, partition names from the
//! device tree. Nothing here re-validates them, and nothing here touches a
//! device.

/// The one LVM volume group every ROM's storage is carved from.
pub const VG: &str = "rom";

/// Device-mapper name of the pool's data sub-volume. Its single `slaves/` entry
/// is the physical `userdata` PV that [`ESD_PV`] points at.
pub const POOL_TDATA: &str = "rom-pool_tdata";

/// Root of the device-name tree esud publishes at `early`, before any
/// `early_hal` service runs.
pub const ESD_ROOT: &str = "/dev/block/esd";
/// The physical `userdata` PV, addressed by name instead of a sysfs scan.
pub const ESD_PV: &str = "/dev/block/esd/pv/a";
/// The directory esd exposes every projected-away original partition in.
pub const ESD_BY_NAME: &str = "/dev/block/esd/by-name";
/// The directory esd exposes every active `rom-` device-mapper node in.
pub const ESD_LV: &str = "/dev/block/esd/lv";
/// LVM's own device directory, so `lvm` finds nodes where it expects them.
pub const ESD_MAPPER: &str = "/dev/block/esd/mapper";
/// Locking, run and configuration directories of the static `lvm`.
pub const ESD_LOCK: &str = "/dev/block/esd/lock";
pub const ESD_RUN: &str = "/dev/block/esd/run";
pub const ESD_ETC: &str = "/dev/block/esd/etc";

/// SELinux context of every node under [`ESD_ROOT`].
pub const ESD_CONTEXT: &str = "u:object_r:esu_blk_device:s0";

/// LVM name of one ROM's staging LV for one base image, `rom<N>-stage-<base>`.
pub fn stage_lv_name(rom_number: u32, base: &str) -> String {
    format!("rom{rom_number}-stage-{base}")
}

/// Device-mapper name of that staging LV: the VG and LV names joined with one
/// hyphen and every hyphen in them doubled, which is LVM's own mapping.
pub fn stage_dm_name(rom_number: u32, base: &str) -> String {
    format!("rom-rom{rom_number}--stage--{base}")
}

/// Device-mapper name of the switch a ROM's running slot reads one base image
/// through: linear over the staging LV while an update is staged, an error
/// target otherwise.
pub fn ota_dm_name(rom_number: u32, base: &str) -> String {
    format!("rom{rom_number}-ota-{base}")
}

/// Node esd publishes for one physical partition the projection hides.
pub fn esd_by_name(partition: &str) -> String {
    format!("{ESD_BY_NAME}/{partition}")
}

/// Node esd publishes for one active logical volume or device-mapper device.
pub fn esd_lv(lv: &str) -> String {
    format!("{ESD_LV}/{lv}")
}

/// Node under esd's `mapper` directory.
pub fn esd_mapper(name: &str) -> String {
    format!("{ESD_MAPPER}/{name}")
}

/// The staging payload file name inside one ROM's ESP directory.
pub const STAGE_PAYLOAD: &str = "esu.stage.cpio";
/// The committed takeover payload file name inside one ROM's ESP directory.
pub const COMMITTED_PAYLOAD: &str = "esu.cpio";

/// ESP-relative path of one ROM's staging payload, the archive Surfacer boots
/// the staged letter from.
pub fn stage_payload(id: &str) -> String {
    format!("rom/{id}/{STAGE_PAYLOAD}")
}

/// ESP-relative path of one ROM's committed takeover payload, the archive every
/// normal boot uses.
pub fn committed_payload(id: &str) -> String {
    format!("rom/{id}/{COMMITTED_PAYLOAD}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_names_match_the_wire_contract() {
        assert_eq!(stage_lv_name(2, "boot"), "rom2-stage-boot");
        assert_eq!(stage_dm_name(2, "boot"), "rom-rom2--stage--boot");
        assert_eq!(
            ota_dm_name(2, "vendor_kernel_boot"),
            "rom2-ota-vendor_kernel_boot"
        );
        // LVM doubles hyphens in both halves of the name; a base name has none.
        assert_eq!(stage_dm_name(5, "init_boot"), "rom-rom5--stage--init_boot");
    }

    #[test]
    fn esd_nodes_and_payloads_are_rooted_once() {
        assert_eq!(esd_by_name("boot_b"), "/dev/block/esd/by-name/boot_b");
        assert_eq!(
            esd_lv("rom2-stage-boot"),
            "/dev/block/esd/lv/rom2-stage-boot"
        );
        assert_eq!(
            esd_mapper("rom2-ota-boot"),
            "/dev/block/esd/mapper/rom2-ota-boot"
        );
        assert_eq!(stage_payload("rom2"), "rom/rom2/esu.stage.cpio");
        assert_eq!(committed_payload("rom2"), "rom/rom2/esu.cpio");
        assert_eq!(ESD_PV, "/dev/block/esd/pv/a");
        assert_eq!(ESD_ROOT, "/dev/block/esd");
    }
}
