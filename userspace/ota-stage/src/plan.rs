// SPDX-License-Identifier: GPL-3.0-only
//! Pure planning of the per-base switch devices `ota-stage` publishes at PID 1.
//!
//! Every managed ROM `>= 2` owns one switch device per declared base image,
//! `rom<N>-ota-<base>`. It is created once per boot: an error target of the
//! image's exact size while nothing is staged, or a linear map over the base's
//! staging LV while a staged letter runs. The boot HAL reloads the same device
//! as the transaction moves, but only `ota-stage` creates it, so both build the
//! table through [`ota_core::switch`] and cannot disagree about its text.
//!
//! This module is pure: it turns the declared bases, the ROM number and the
//! `ESU_STAGE` value into each switch's name, table and access. The device work
//! (resolving the image size and the staging LV's device number, and issuing
//! the device-mapper ioctls) stays in the binary.

use dm::{DeviceNumber, Target};
use ota_core::{names, switch};

/// Access the running boot gives the staged letter's switch device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageAccess {
    /// The staged letter is the booted one: the switch device serves the bytes
    /// the running slot booted from, so it must never be written.
    ReadOnly,
    /// The staged letter is not booted: the updater writes through this device.
    Writable,
}

/// Parse the `ESU_STAGE` value PID 1 exports.
///
/// Empty means no transaction, or a ROM 1. Otherwise the value is
/// `<a|b>:<ro|rw>`: the staged letter and the access it is read with. The
/// letter is not needed here (a switch device is base-named, not slot-named),
/// but it is validated so a truncated, reordered or re-spelled value can never
/// be read as idle and publish an error target where a staging map belongs.
pub fn parse_stage(value: &str) -> Result<Option<StageAccess>, String> {
    if value.is_empty() {
        return Ok(None);
    }

    let invalid = || format!("invalid ESU_STAGE {value:?}");
    let (letter, access) = value.split_once(':').ok_or_else(invalid)?;
    if !matches!(letter, "a" | "b") {
        return Err(invalid());
    }
    match access {
        "ro" => Ok(Some(StageAccess::ReadOnly)),
        "rw" => Ok(Some(StageAccess::Writable)),
        _ => Err(invalid()),
    }
}

/// One declared base image and its switch geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Base {
    /// Base name, one of [`esu_config::IMAGE_BASES`].
    pub base: &'static str,
    /// Sectors of the base image, from `ota_core::copy::exact_sectors`.
    pub sectors: u64,
}

/// One switch device to publish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Switch {
    /// Device-mapper name `rom<N>-ota-<base>`.
    pub name: String,
    /// Base image this device stands in for.
    pub base: &'static str,
    /// Whether the table is linear over the base's staging LV.
    pub staged: bool,
    /// The image's size in sectors, the length of the one target.
    pub sectors: u64,
    /// Whether the device is created read-only.
    pub read_only: bool,
}

impl Switch {
    /// The one-target table this switch serves.
    ///
    /// `stage` is the staging LV's device number, required exactly when the
    /// switch is staged; an absent number is an error rather than an error
    /// target, because publishing an idle switch over an active staged set
    /// would hide the update from the updater.
    pub fn table(&self, stage: Option<DeviceNumber>) -> Result<Vec<Target>, String> {
        if self.staged {
            let device = stage.ok_or_else(|| format!("{} has no staging device", self.name))?;
            Ok(vec![switch::linear(device, self.sectors)])
        } else {
            Ok(vec![switch::error_target(self.sectors)])
        }
    }
}

/// Plan one switch per declared base.
///
/// `stage` is the parsed `ESU_STAGE`: when it is set every switch is linear over
/// its base's staging LV and read-only exactly for [`StageAccess::ReadOnly`];
/// when it is absent every switch is an error target of the image's size, so the
/// ROM config's generated `SOURCE_COPY` size checks see the exact image
/// geometry while no update is in flight.
pub fn switches(bases: &[Base], rom_number: u32, stage: Option<StageAccess>) -> Vec<Switch> {
    bases
        .iter()
        .map(|base| Switch {
            name: names::ota_dm_name(rom_number, base.base),
            base: base.base,
            staged: stage.is_some(),
            sectors: base.sectors,
            read_only: matches!(stage, Some(StageAccess::ReadOnly)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(major: u32, minor: u32) -> DeviceNumber {
        DeviceNumber { major, minor }
    }

    const BASES: [Base; 2] = [
        Base {
            base: "boot",
            sectors: 2048,
        },
        Base {
            base: "dtb",
            sectors: 4096,
        },
    ];

    #[test]
    fn an_unstaged_rom_plans_one_error_target_per_base() {
        let switches = switches(&BASES, 2, None);

        assert_eq!(switches.len(), 2);
        assert_eq!(switches[0].name, "rom2-ota-boot");
        assert_eq!(switches[1].name, "rom2-ota-dtb");
        for (planned, base) in switches.iter().zip(BASES) {
            assert_eq!(planned.base, base.base);
            assert!(!planned.staged);
            assert!(!planned.read_only);
            assert_eq!(planned.sectors, base.sectors);
            assert_eq!(
                planned.table(None).unwrap(),
                vec![switch::error_target(base.sectors)]
            );
        }
    }

    #[test]
    fn a_staged_rom_plans_a_linear_table_read_only_exactly_for_ro() {
        let stage = device(253, 7);

        let read_only = switches(&BASES, 3, Some(StageAccess::ReadOnly));
        assert!(
            read_only
                .iter()
                .all(|planned| planned.staged && planned.read_only)
        );
        assert_eq!(
            read_only[0].table(Some(stage)).unwrap(),
            vec![switch::linear(stage, 2048)]
        );

        let writable = switches(&BASES, 3, Some(StageAccess::Writable));
        assert!(writable.iter().all(|planned| planned.staged));
        assert!(writable.iter().all(|planned| !planned.read_only));
        assert_eq!(
            writable[1].table(Some(stage)).unwrap(),
            vec![switch::linear(stage, 4096)]
        );
    }

    #[test]
    fn a_staged_switch_without_a_device_is_an_error() {
        let planned = &switches(&BASES, 2, Some(StageAccess::Writable))[0];

        assert!(planned.table(None).is_err());
    }

    #[test]
    fn the_staged_letter_does_not_change_the_device_name() {
        let a = switches(&BASES, 4, Some(StageAccess::ReadOnly));
        let b = switches(&BASES, 4, Some(StageAccess::Writable));

        assert_eq!(a[0].name, b[0].name);
    }

    #[test]
    fn an_empty_stage_is_idle_and_a_spelled_stage_carries_its_access() {
        assert_eq!(parse_stage(""), Ok(None));
        assert_eq!(parse_stage("a:ro"), Ok(Some(StageAccess::ReadOnly)));
        assert_eq!(parse_stage("b:rw"), Ok(Some(StageAccess::Writable)));
    }

    #[test]
    fn a_malformed_stage_is_rejected() {
        for value in [
            "a", "a:", "a:r", ":ro", "A:ro", "_a:ro", "a:RO", "a:ro ", " a:ro", "a:ro:x", "x:rw",
            "ab:ro", "a_b:ro",
        ] {
            assert!(parse_stage(value).is_err(), "{value:?} must be rejected");
        }
    }
}
