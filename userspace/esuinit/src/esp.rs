//! ESP discovery and read-only mounting.
//!
//! There is no device-name allowlist: whole block disks are enumerated from
//! sysfs, their dev_t is read from sysfs, and EFI System Partitions are found
//! by parsing the GPT and matching the ESP type GUID. Devices may carry other
//! ESPs (for example, the firmware's own loader partition), so candidates are
//! mounted read-only and exactly one must contain a regular
//! `/esu/manifest.toml`. The selected block node is created from the
//! major/minor pair discovered through sysfs. PID 1 detaches its contextless
//! mount before Android handoff; loop backing files keep their own references.
//! Executables run from a separate tmpfs, also detached at handoff. Only a ROM
//! projecting writable `esp-file:` backends remounts the ESP read-write.

use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, FileType, Mode, makedev, mknodat};
use rustix::mount::{UnmountFlags, mount, mount_remount, unmount};

use crate::gpt;
use crate::receipt::{ESP_MOUNT_FLAGS_RO, ESP_MOUNT_FLAGS_RW, Failure, Stage};

/// Runtime ESP mount point from the layout contract.
pub const ESP_MOUNT_POINT: &str = "/debug_ramdisk/esp";

/// Directory holding block device nodes created by the loader.
const DEVICE_DIR: &str = "/dev/esu";

/// sysfs whole-disk directory.
const SYS_BLOCK: &str = "/sys/block";

/// Marker that identifies the managed payload among other firmware ESPs.
const PAYLOAD_MANIFEST: &str = "esu/manifest.toml";

/// Sectors are always 512 bytes for sysfs `start`/`size` units.
const SYSFS_SECTOR: u64 = 512;

/// Bound for a single GPT partition table read.
const MAX_TABLE_BYTES: usize = 1 << 20;

/// A discovered ESP block device.
struct EspDevice {
    major: u32,
    minor: u32,
    disk: String,
}

/// The ESP mount owned by PID 1 until real-init handoff.
pub struct Mount {
    // The node may disappear when Android replaces /dev; dev_t remains stable.
    major: u32,
    minor: u32,
    path: String,
    detached: bool,
    writable: bool,
}

impl Mount {
    /// Runtime mount point of the ESP payload tree.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Physical device number. The projection hide set excludes this mounted
    /// ESP so a post-APPLY failure can still remount it for its receipt.
    pub fn device(&self) -> (u32, u32) {
        (self.major, self.minor)
    }

    pub fn detach(&mut self) -> Result<(), Failure> {
        detach_owned(&self.path)?;
        self.detached = true;
        Ok(())
    }

    /// Recreate access only on failed exec, after the early /dev may be gone.
    pub fn reattach_for_receipt(&mut self) -> Result<(), Failure> {
        if self.detached {
            let node = create_block_node("esp", self.major, self.minor)
                .map_err(|detail| Failure::new(Stage::Storage, "EspDeviceNodeCreate", detail))?;
            let flags = if self.writable {
                ESP_MOUNT_FLAGS_RW
            } else {
                ESP_MOUNT_FLAGS_RO
            };
            mount(&node, &self.path, "vfat", flags, "")
                .map_err(|error| Failure::new(Stage::Storage, "EspMount", error.to_string()))?;
            self.detached = false;
        }
        Ok(())
    }
}

/// Discover the payload ESP and mount it read-only at [`ESP_MOUNT_POINT`].
pub fn mount_esp() -> Result<Mount, Failure> {
    let devices = discover_esps()?;
    let mut selected = None;

    for (index, device) in devices.iter().enumerate() {
        let node = create_block_node(
            &format!("esp-{}-{}", device.major, device.minor),
            device.major,
            device.minor,
        )
        .map_err(|detail| Failure::new(Stage::Storage, "EspDeviceNodeCreate", detail))?;

        if let Err(error) = mount_read_only(&node) {
            log::warn!(
                "Skipping ESP candidate {}({}:{}): {}",
                device.disk,
                device.major,
                device.minor,
                error.detail
            );
            continue;
        }

        let marker = Path::new(ESP_MOUNT_POINT).join(PAYLOAD_MANIFEST);
        let payload = match fs::symlink_metadata(&marker) {
            Ok(metadata) => Ok(metadata.file_type().is_file()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(Failure::new(
                Stage::Storage,
                "EspPayloadProbe",
                format!("cannot examine {}: {error}", marker.display()),
            )),
        };
        let detached = unmount(ESP_MOUNT_POINT, UnmountFlags::empty()).map_err(|error| {
            Failure::new(
                Stage::Storage,
                "EspProbeUnmount",
                format!("cannot unmount ESP candidate {node}: {error}"),
            )
        });
        detached?;

        if payload? && selected.replace(index).is_some() {
            return Err(Failure::new(
                Stage::Storage,
                "EspAmbiguous",
                "multiple EFI System Partitions contain esu/manifest.toml",
            ));
        }
    }

    let Some(index) = selected else {
        return Err(Failure::new(
            Stage::Storage,
            "EspPayloadNotFound",
            format!(
                "{} EFI System Partition candidate(s), none containing a regular {PAYLOAD_MANIFEST}",
                devices.len()
            ),
        ));
    };
    let device = &devices[index];
    let node = create_block_node("esp", device.major, device.minor)
        .map_err(|detail| Failure::new(Stage::Storage, "EspDeviceNodeCreate", detail))?;
    mount_read_only(&node)?;

    log::info!(
        "Mounted payload ESP partition {}({}:{}) at {}",
        device.disk,
        device.major,
        device.minor,
        ESP_MOUNT_POINT
    );

    Ok(Mount {
        major: device.major,
        minor: device.minor,
        path: ESP_MOUNT_POINT.to_owned(),
        detached: false,
        writable: false,
    })
}

/// Remount the selected payload ESP read-write for a ROM that projects writable
/// `esp-file:` backends.
///
/// Loop files retain this mount even after detach; Android modules get a new
/// read-only bind view. The remount only toggles `RDONLY`.
pub fn make_payload_writable(mount: &mut Mount) -> Result<(), Failure> {
    mount_remount(&mount.path, ESP_MOUNT_FLAGS_RW, "").map_err(|error| {
        Failure::new(
            Stage::Storage,
            "EspMountWritable",
            format!(
                "cannot remount the ESP at {} read-write for writable ESP-file backends: {error}",
                mount.path
            ),
        )
    })?;
    mount.writable = true;
    Ok(())
}

/// Detach an early mount that esu created itself, in preparation for the
/// real init. Only mounts created by esu may be passed here.
pub fn detach_owned(mountpoint: &str) -> Result<(), Failure> {
    unmount(mountpoint, UnmountFlags::DETACH).map_err(|error| {
        Failure::at(
            Stage::Handoff,
            Some(mountpoint),
            "HandoffUnmountFailed",
            format!("cannot detach {mountpoint}: {error}"),
        )
    })
}

/// Whether `mountpoint` already carries a mount rather than being an ordinary
/// directory of the filesystem below it. The device id of the directory is
/// compared with its parent, which needs no `/proc` and therefore also works
/// for the `/proc` mountpoint itself.
pub fn is_mounted(mountpoint: &str) -> Result<bool, String> {
    use std::os::unix::fs::MetadataExt;

    let path = Path::new(mountpoint);

    let Ok(target) = fs::metadata(path) else {
        return Ok(false);
    };

    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("/"),
    };
    let parent = fs::metadata(parent)
        .map_err(|error| format!("cannot examine {}: {error}", parent.display()))?;

    Ok(target.dev() != parent.dev())
}

/// Walk every whole disk and return all enumerated ESP partition devices.
fn discover_esps() -> Result<Vec<EspDevice>, Failure> {
    let disks = fs::read_dir(SYS_BLOCK).map_err(|error| {
        Failure::new(
            Stage::Storage,
            "EspSysfsUnavailable",
            format!("cannot enumerate {SYS_BLOCK}: {error}"),
        )
    })?;

    let mut found = Vec::new();

    for disk in disks {
        let Ok(disk) = disk else {
            continue;
        };
        let path = disk.path();
        let name = disk.file_name().to_string_lossy().into_owned();

        // `/sys/block` lists whole disks; skip anything that is itself a
        // partition if the kernel ever exposes one here.
        if path.join("partition").exists() {
            continue;
        }

        let Some((major, minor)) = read_dev(&path.join("dev")) else {
            continue;
        };

        let sector_size = read_u64(&path.join("queue/logical_block_size"))
            .filter(|size| *size >= SYSFS_SECTOR && size.is_power_of_two())
            .unwrap_or(SYSFS_SECTOR) as u32;
        let device_bytes =
            read_u64(&path.join("size")).map(|sectors| sectors.saturating_mul(SYSFS_SECTOR));

        let node = match create_block_node(&format!("{major}-{minor}"), major, minor) {
            Ok(node) => node,
            Err(error) => {
                log::warn!("Skipping disk {name}: {error}");
                continue;
            }
        };

        let entries = match probe_esp_entries(&node, &name, sector_size, device_bytes) {
            Ok(entries) => entries,
            Err(error) => {
                log::warn!("Skipping disk {name}: {error}");
                continue;
            }
        };

        for entry in &entries {
            let partition =
                partition_device(&path, entry.first_lba, sector_size).map_err(|error| {
                    Failure::new(
                        Stage::Storage,
                        "EspPartitionLookup",
                        format!("{name}: {error}"),
                    )
                })?;

            let Some((major, minor)) = partition else {
                return Err(Failure::new(
                    Stage::Storage,
                    "EspPartitionMissing",
                    format!(
                        "{name}: no partition device for GPT entry {} at LBA {}",
                        entry.index, entry.first_lba
                    ),
                ));
            };

            found.push(EspDevice {
                major,
                minor,
                disk: name.clone(),
            });
        }
    }

    if found.is_empty() {
        return Err(Failure::new(
            Stage::Storage,
            "EspNotFound",
            "no EFI System Partition found on any whole block disk",
        ));
    }

    Ok(found)
}

/// Read and parse the GPT of one disk, returning its ESP entries.
fn probe_esp_entries(
    node: &str,
    name: &str,
    sector_size: u32,
    device_bytes: Option<u64>,
) -> Result<Vec<gpt::Entry>, String> {
    let file = File::open(node).map_err(|error| format!("cannot open {node}: {error}"))?;

    let mut sector = [0u8; 512];
    file.read_exact_at(&mut sector, u64::from(sector_size))
        .map_err(|error| format!("cannot read the GPT header: {error}"))?;

    let Some(header) = gpt::parse_header(&sector) else {
        return Ok(Vec::new());
    };

    let table_bytes = u64::from(header.entry_count) * u64::from(header.entry_size);

    if table_bytes > MAX_TABLE_BYTES as u64 {
        return Err(format!("GPT partition table is {table_bytes} bytes"));
    }

    let table_offset = header
        .entries_lba
        .checked_mul(u64::from(sector_size))
        .ok_or("GPT entry table offset overflow")?;

    let mut table = vec![0u8; table_bytes as usize];
    file.read_exact_at(&mut table, table_offset)
        .map_err(|error| format!("cannot read the GPT entry table: {error}"))?;

    let entries = gpt::find_entries(&table, header.entry_size as usize, &gpt::ESP_TYPE_GUID);

    for entry in &entries {
        let end = entry
            .last_lba
            .checked_mul(u64::from(sector_size))
            .and_then(|last| last.checked_add(u64::from(sector_size)))
            .ok_or("GPT partition end overflow")?;

        if let Some(device_bytes) = device_bytes
            && end > device_bytes
        {
            return Err(format!(
                "GPT entry {} for {name} extends past the end of the disk",
                entry.index
            ));
        }
    }

    Ok(entries)
}

/// Locate the partition device for a GPT entry by matching the sysfs `start`
/// sector, so no partition-number assumption is made.
fn partition_device(
    disk: &Path,
    first_lba: u64,
    sector_size: u32,
) -> Result<Option<(u32, u32)>, String> {
    let expected_start = first_lba
        .checked_mul(u64::from(sector_size))
        .map(|bytes| bytes / SYSFS_SECTOR)
        .ok_or("partition start overflow")?;

    let mut matches = Vec::new();

    let entries =
        fs::read_dir(disk).map_err(|error| format!("cannot enumerate {disk:?}: {error}"))?;

    for entry in entries.flatten() {
        let path = entry.path();

        if !path.join("partition").exists() {
            continue;
        }

        let Some(start) = read_u64(&path.join("start")) else {
            continue;
        };

        if start == expected_start
            && let Some(device) = read_dev(&path.join("dev"))
        {
            matches.push(device);
        }
    }

    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        count => Err(format!(
            "{count} partition devices match start sector {expected_start}"
        )),
    }
}

/// Create (or replace) a block device node for a discovered major/minor pair.
/// Failures are returned as details so discovery can skip an unrelated disk,
/// while the final ESP node remains a hard failure.
fn create_block_node(name: &str, major: u32, minor: u32) -> Result<String, String> {
    fs::create_dir_all(DEVICE_DIR)
        .map_err(|error| format!("cannot create {DEVICE_DIR}: {error}"))?;

    let path = format!("{DEVICE_DIR}/{name}");

    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("cannot replace {path}: {error}")),
    }

    mknodat(
        CWD,
        path.as_str(),
        FileType::BlockDevice,
        Mode::from_raw_mode(0o600),
        makedev(major, minor),
    )
    .map_err(|error| format!("cannot create block node {path}: {error}"))?;

    Ok(path)
}

/// Resolve one physical partition by its kernel `PARTNAME` and create the node
/// the caller opens. Android's `/dev/block/by-name` links do not exist during
/// early boot, so sysfs is the only name source; the node is created under the
/// private device directory exactly like the ESP node.
pub fn partition_node(name: &str) -> Result<String, String> {
    let device = esu_platform::block::partition_by_name(name)
        .map_err(|error| format!("cannot resolve partition {name}: {error}"))?;

    create_block_node(name, rustix::fs::major(device), rustix::fs::minor(device))
}

/// Mount the discovered ESP read-only at the runtime mount point.
fn mount_read_only(node: &str) -> Result<(), Failure> {
    fs::create_dir_all(ESP_MOUNT_POINT).map_err(|error| {
        Failure::new(
            Stage::Storage,
            "EspMountPointCreate",
            format!("cannot create {ESP_MOUNT_POINT}: {error}"),
        )
    })?;
    if is_mounted(ESP_MOUNT_POINT)
        .map_err(|detail| Failure::new(Stage::Storage, "EspMountOwnership", detail))?
    {
        return Err(Failure::new(
            Stage::Storage,
            "EspMountOwnership",
            "PID 1 must create the ESP mount, not reuse an inherited mount",
        ));
    }

    mount(node, ESP_MOUNT_POINT, "vfat", ESP_MOUNT_FLAGS_RO, "").map_err(|error| {
        Failure::new(
            Stage::Storage,
            "EspMount",
            format!("cannot mount {node} at {ESP_MOUNT_POINT} read-only: {error}"),
        )
    })
}

/// Mount a filesystem that supports the new mount API, reusing the ramdisk
/// cleaning path for the minimal early mounts.
pub fn mount_kernel_fs(name: &str, mountpoint: &str) -> Result<(), String> {
    use rustix::{
        fd::AsFd,
        fs::mkdir,
        mount::{
            FsMountFlags, FsOpenFlags, MountAttrFlags, MoveMountFlags, fsconfig_create, fsmount,
            fsopen, move_mount,
        },
    };

    mkdir(mountpoint, Mode::from_raw_mode(0o755)).ok();

    let filesystem = fsopen(name, FsOpenFlags::FSOPEN_CLOEXEC)
        .map_err(|error| format!("cannot open {name}: {error}"))?;
    fsconfig_create(filesystem.as_fd())
        .map_err(|error| format!("cannot configure {name}: {error}"))?;
    let context = fsmount(
        filesystem.as_fd(),
        FsMountFlags::FSMOUNT_CLOEXEC,
        MountAttrFlags::empty(),
    )
    .map_err(|error| format!("cannot create {name} mount: {error}"))?;
    move_mount(
        context.as_fd(),
        "",
        CWD,
        mountpoint,
        MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
    )
    .map_err(|error| format!("cannot attach {name} at {mountpoint}: {error}"))?;

    Ok(())
}

fn read_dev(path: &Path) -> Option<(u32, u32)> {
    let value = fs::read_to_string(path).ok()?;
    let (major, minor) = value.trim().split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn read_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Absolute runtime path of the ESP payload subtree.
pub fn payload_root(mount: &str) -> PathBuf {
    Path::new(mount).join("esu")
}

/// Executable tmpfs recreated by second-stage init after PID 1 detaches it.
pub const EXECUTABLE_ROOT: &str = "/debug_ramdisk/esu";
pub const EXECUTABLE_BIN: &str = "/debug_ramdisk/esu/bin";

/// Admit exactly one contextless, whole-filesystem ESP mount before projection.
/// Reject duplicate/bind mounts and all SELinux superblock context overrides.
pub fn verify_single_esp(text: &str, device: (u32, u32)) -> Result<(), String> {
    let device = format!("{}:{}", device.0, device.1);
    let mut count = 0;
    for line in text.lines() {
        let (left, right) = line.split_once(" - ").ok_or("invalid mountinfo")?;
        let left: Vec<_> = left.split_whitespace().collect();
        let right: Vec<_> = right.split_whitespace().collect();
        if left.len() < 6 || right.len() != 3 {
            return Err("invalid mountinfo fields".into());
        }
        if left[2] != device {
            continue;
        }
        count += 1;
        if left[3] != "/" || left[4] != ESP_MOUNT_POINT || right[0] != "vfat" {
            return Err("ESP must have one whole-filesystem vfat mount at its staging path".into());
        }
        for options in [left[5], right[2]] {
            if options.split(',').any(|option| {
                ["context=", "fscontext=", "rootcontext=", "defcontext="]
                    .iter()
                    .any(|key| option.starts_with(key))
            }) {
                return Err("ESP SELinux mount context override is forbidden".into());
            }
        }
    }
    if count != 1 {
        return Err(format!("expected one visible ESP mount, found {count}"));
    }
    Ok(())
}

impl Mount {
    pub fn verify_retained(&self) -> Result<(), Failure> {
        let result = fs::read_to_string("/proc/self/mountinfo")
            .map_err(|error| error.to_string())
            .and_then(|text| verify_single_esp(&text, self.device()));
        result.map_err(|detail| Failure::new(Stage::Storage, "EspMountLifecycle", detail))
    }
}

/// Stage the complete bin tree, including helpers called by module scripts.
/// No symlinks or special inodes may escape into the executable tmpfs.
pub fn stage_executables(payload: &Path, device: (u32, u32)) -> Result<(), Failure> {
    let result = (|| -> std::io::Result<()> {
        for name in ["esud", "busybox", "thin-activate"] {
            if !fs::symlink_metadata(payload.join("bin").join(name))?.is_file() {
                return Err(std::io::Error::other(format!(
                    "bin/{name} is not a regular file"
                )));
            }
        }
        if is_mounted(EXECUTABLE_ROOT).map_err(std::io::Error::other)? {
            return Err(std::io::Error::other("executable tmpfs already mounted"));
        }
        fs::create_dir_all(EXECUTABLE_ROOT)?;
        mount(
            "esu",
            EXECUTABLE_ROOT,
            "tmpfs",
            rustix::mount::MountFlags::NOSUID | rustix::mount::MountFlags::NODEV,
            "mode=0755",
        )?;
        copy_bin(&payload.join("bin"), Path::new(EXECUTABLE_BIN))?;
        fs::write(
            Path::new(EXECUTABLE_ROOT).join("esp-device"),
            format!("{}:{}\n", device.0, device.1),
        )?;
        Ok(())
    })();
    result.map_err(|error| Failure::new(Stage::Storage, "ExecutableStageFailed", error.to_string()))
}

fn copy_bin(source: &Path, target: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        fs::create_dir(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_bin(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        fs::copy(source, target)?;
    } else {
        return Err(std::io::Error::other(format!(
            "unsupported executable inode: {}",
            source.display()
        )));
    }
    fs::set_permissions(target, fs::Permissions::from_mode(0o755))
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    /// The projection admission contract rejects any second mount or context
    /// override, rather than permitting a loop-pinned conflicting superblock.
    #[test]
    fn only_one_contextless_owned_esp_is_admitted() {
        let valid = "12 1 8:1 / /debug_ramdisk/esp ro,nosuid,nodev,noexec - vfat /dev/esu/esp ro\n";
        assert!(verify_single_esp(valid, (8, 1)).is_ok());
        for invalid in [
            String::new(),
            format!("{valid}{valid}"),
            valid.replace(" - vfat", " - ext4"),
            valid.replace(" /debug_ramdisk/esp ", " /dev/esp "),
            valid.replace(" ro\n", " ro,context=u:object_r:esu_file:s0\n"),
            valid.replace(" ro\n", " ro,rootcontext=u:object_r:esu_file:s0\n"),
            valid.replace("8:1", "8:2"),
        ] {
            assert!(verify_single_esp(&invalid, (8, 1)).is_err());
        }
    }
}
