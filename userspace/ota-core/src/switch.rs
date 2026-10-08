// SPDX-License-Identifier: GPL-3.0-only
//! The two tables a ROM's per-base switch device serves.
//!
//! `rom<N>-ota-<base>` is created once per boot and reloaded as the transaction
//! moves: it is an error target while nothing is staged, a linear map over the
//! staging LV while the staged letter runs, and a read-only linear map over the
//! ESP image after the promote. Both tables are built here so `ota-stage` (which
//! creates the device) and the boot HAL (which reloads it) cannot disagree about
//! the target text.

use dm::{DeviceNumber, Target};

/// One linear table covering `sectors` sectors of `device` from offset 0.
///
/// The staging LV is exactly one base image long, so the switch device has the
/// same size as the image it stands in for and every offset the ROM config
/// projects is in range.
pub fn linear(device: DeviceNumber, sectors: u64) -> Target {
    Target {
        start: 0,
        length: sectors,
        kind: "linear".to_owned(),
        params: format!("{}:{} 0", device.major, device.minor),
    }
}

/// One error table of the base image's size.
///
/// A switch device with no staged set must fail reads loudly rather than serve
/// stale bytes: the ROM config resolves the staged letter's partitions to this
/// device, and a read that cannot be served has to become an I/O error at the
/// reader instead of the wrong image. The length keeps the device geometry equal
/// to the base image it stands in for.
pub fn error_target(sectors: u64) -> Target {
    Target {
        start: 0,
        length: sectors,
        kind: "error".to_owned(),
        params: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_linear_table_names_the_device_and_covers_the_whole_image() {
        let table = linear(
            DeviceNumber {
                major: 253,
                minor: 7,
            },
            2048,
        );
        assert_eq!(table.start, 0);
        assert_eq!(table.length, 2048);
        assert_eq!(table.kind, "linear");
        assert_eq!(table.params, "253:7 0");
    }

    #[test]
    fn the_error_table_keeps_the_image_size_and_carries_no_parameters() {
        let table = error_target(2048);
        assert_eq!(table.start, 0);
        assert_eq!(table.length, 2048);
        assert_eq!(table.kind, "error");
        assert_eq!(table.params, "");
    }
}
