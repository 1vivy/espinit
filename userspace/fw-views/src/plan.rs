// SPDX-License-Identifier: GPL-3.0-only
//! Pure derivation of the per-ROM firmware views.
//!
//! Every view is one thin device of the shared pool whose origin is a physical
//! firmware partition: unwritten blocks read that partition's exact bytes and
//! this ROM's writes provision private blocks in the pool. Nothing here touches
//! a device; the ids, names and table text are decided from the validated ROM
//! config so they can be tested without a kernel.

use dm::{DeviceNumber, Target};
use esuinit::config::RomConfig;

/// Device-mapper name of the pool layer `thin-activate` creates from the VG
/// `rom`'s own LVM2 metadata before the `gpt` entry resolves its backends.
pub const POOL: &str = "rom-pool-tpool";

/// Public device-mapper name of one view, which is also the projection backend
/// the ROM config must name for it.
pub fn device(rom_number: u32, name: &str) -> String {
    format!("rom{rom_number}-fw-{name}")
}

/// The pool message that creates one view's reserved thin id. Re-running it on
/// a later boot is expected and reported as "already exists".
pub fn create_thin(thin_id: u32) -> String {
    format!("create_thin {thin_id}")
}

/// The pool message that drops one view's thin id and every block it
/// provisioned, restoring the physical bytes on the next creation.
pub fn delete_thin(thin_id: u32) -> String {
    format!("delete {thin_id}")
}

/// One view of the selected ROM, in config order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// Physical sysfs `PARTNAME` the view reads through to.
    pub origin: String,
    /// Device-mapper name the ROM's projection expects.
    pub device: String,
    /// Reserved thin id `(rom_number << 16) | index`.
    pub thin_id: u32,
}

/// Every firmware view the selected ROM projects.
pub fn views(rom: &RomConfig) -> Vec<View> {
    rom.firmware_views
        .iter()
        .map(|view| View {
            origin: view.name.clone(),
            device: device(rom.rom_number, &view.name),
            thin_id: view.thin_id,
        })
        .collect()
}

/// The single-target table of one view: `thin <pool> <id> <origin>`, the
/// external-origin form. The kernel opens the origin read-only, so a view can
/// never write through to the physical firmware partition, and the projection
/// keeps the whole physical partition as the view's size.
pub fn table(origin: DeviceNumber, sectors: u64, thin_id: u32, pool: DeviceNumber) -> Target {
    Target {
        start: 0,
        length: sectors,
        kind: "thin".to_owned(),
        params: format!(
            "{}:{} {thin_id} {}:{}",
            pool.major, pool.minor, origin.major, origin.minor
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use esuinit::config::parse_rom;

    const ROM: &str = r#"
schema_version = 1
generation = "release-1"
id = "android-b"
rom_number = 2
managed = true
[[firmware_views]]
name = "xbl_a"
thin_id = 131073
[[firmware_views]]
name = "modem_b"
thin_id = 131074
[[partitions]]
name = "xbl_a"
backend = "/dev/mapper/rom2-fw-xbl_a"
read_only = false
[[partitions]]
name = "modem_b"
backend = "/dev/mapper/rom2-fw-modem_b"
read_only = false
"#;

    #[test]
    fn view_names_and_ids_come_from_the_rom_number_and_config_order() {
        let rom = parse_rom(ROM, "release-1").unwrap();

        assert_eq!(
            views(&rom),
            [
                View {
                    origin: "xbl_a".to_owned(),
                    device: "rom2-fw-xbl_a".to_owned(),
                    thin_id: 131073,
                },
                View {
                    origin: "modem_b".to_owned(),
                    device: "rom2-fw-modem_b".to_owned(),
                    thin_id: 131074,
                },
            ]
        );
        assert_eq!(device(5, "xbl_a"), "rom5-fw-xbl_a");
        assert_eq!(create_thin(131073), "create_thin 131073");
        assert_eq!(delete_thin(131073), "delete 131073");
    }

    #[test]
    fn a_rom_without_views_plans_nothing() {
        let rom = parse_rom(
            "schema_version = 1\ngeneration = \"release-1\"\nid = \"android-a\"\nmanaged = true\n\
             [[partitions]]\nname = \"system\"\nbackend = \"/dev/block/by-name/system\"\nread_only = true\n",
            "release-1",
        )
        .unwrap();

        assert!(views(&rom).is_empty());
    }

    #[test]
    fn the_table_is_the_external_origin_thin_target_over_the_pool() {
        let target = table(
            DeviceNumber {
                major: 8,
                minor: 17,
            },
            16_384,
            131073,
            DeviceNumber {
                major: 253,
                minor: 4,
            },
        );

        assert_eq!(target.start, 0);
        assert_eq!(target.length, 16_384);
        assert_eq!(target.kind, "thin");
        // `<pool> <id> <origin>`: the kernel reports exactly this string back.
        assert_eq!(target.params, "253:4 131073 8:17");
    }
}
