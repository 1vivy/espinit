//! Shared installed-configuration fixtures for the esu-config contract tests.
//!
//! The manifest text is a valid installed manifest (strict schema plus the
//! fixed cpio identity bootstrap), and the ROM texts are the installed
//! configurations of a physical ROM 1 and of a ROM 2 with a base image set.
#![allow(dead_code)] // Each test binary uses a subset of these fixtures.

/// A valid installed manifest: strict schema plus the identity bootstrap, with
/// `kernelesp` first, the efivarfs frontend and an explicitly targeted EFVS backend.
pub const MANIFEST: &str = r#"
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

/// The same manifest without the `efivarfs` identity module.
pub const MANIFEST_NO_EFIVARFS: &str = r#"
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
"#;

/// The `gpt` manifest entry, for surgery that must keep the bootstrap intact.
pub const GPT_ENTRY: &str = "[[modules]]
name = \"gpt\"
path = \"lib/gpt.ko\"
params = \"debug=0\"
";

/// ROM 1: a managed ROM with one read-only physical projection.
pub const ROM1: &str = r#"
schema_version = 1
id = "rom1"
managed = true
[[partitions]]
name = "system"
backend = "/dev/block/by-name/system"
read_only = true
"#;

/// One managed ROM 2 view of the physical `xbl_a` firmware partition.
pub const FW_ROM: &str = r#"
schema_version = 1
id = "android-b"
managed = true
[[firmware_views]]
name = "xbl_a"
thin_id = 131073
[[partitions]]
name = "xbl_a"
backend = "/dev/mapper/rom2-fw-xbl_a"
read_only = false
"#;

/// The two bases most ROM `>= 2` tests declare, in [`IMAGE_BASES`] order.
///
/// [`IMAGE_BASES`]: esu_config::IMAGE_BASES
pub const DECLARED_BASES: [&str; 2] = ["boot", "vbmeta"];

/// Partition names of a two-base ROM `>= 2` image set: [`DECLARED_BASES`] order
/// with slot A before slot B.
pub const IMAGE_PARTITIONS: [&str; 4] = ["boot_a", "boot_b", "vbmeta_a", "vbmeta_b"];

/// ESP-root-relative image path admitted for one base of ROM 2.
pub fn image_path(base: &str) -> String {
    format!("rom/rom2/{base}.img")
}

/// A managed ROM with the given id and exactly the given partitions, all
/// read-only.
pub fn rom_with<'a>(id: &str, partitions: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let mut text = format!("schema_version = 1\nid = \"{id}\"\nmanaged = true\n");

    for (name, backend) in partitions {
        text.push_str(&format!(
            "[[partitions]]\nname = \"{name}\"\nbackend = \"{backend}\"\nread_only = true\n"
        ));
    }

    text
}

/// ROM 2 text built from the image set: `keep` selects the partitions and
/// `backend` overrides one partition's backend.
fn rom2_build(keep: impl Fn(&str) -> bool, backend: impl Fn(&str) -> Option<String>) -> String {
    let mut text = String::from("schema_version = 1\nid = \"rom2\"\nmanaged = true\n");

    for partition in IMAGE_PARTITIONS {
        if !keep(partition) {
            continue;
        }

        let base = partition
            .strip_suffix("_a")
            .or_else(|| partition.strip_suffix("_b"))
            .expect("image partitions carry a slot suffix");
        let value = backend(partition).unwrap_or_else(|| format!("rom-image:{base}"));
        text.push_str(&format!(
            "[[partitions]]\nname = \"{partition}\"\nbackend = \"{value}\"\nread_only = false\n"
        ));
    }

    text
}

/// ROM 2 with a complete two-base image set.
pub fn rom2() -> String {
    rom2_build(|_| true, |_| None)
}

/// ROM 2 without one image partition.
pub fn rom2_without(missing: &str) -> String {
    rom2_build(|partition| partition != missing, |_| None)
}

/// ROM 2 with one image partition's backend replaced.
pub fn rom2_replacing(partition: &str, backend: &str) -> String {
    rom2_build(
        |_| true,
        |name| (name == partition).then(|| backend.to_owned()),
    )
}

/// The installed manifest without the `gpt` module, bootstrap intact.
pub fn manifest_without_gpt() -> String {
    MANIFEST.replace(GPT_ENTRY, "")
}
