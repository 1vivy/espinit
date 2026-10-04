//! ESP discovery and read-only mounting.
//!
//! There is no device-name allowlist: whole block disks are enumerated from
//! sysfs, their dev_t is read from sysfs, and the EFI System Partition is found
//! by parsing the GPT and matching the ESP type GUID. Exactly one such
//! partition may exist. The block node is created from the major/minor pair
//! discovered through sysfs, and the filesystem is mounted read-only. The mount
//! stays executable so the payload busybox can run from it, and it is detached
//! again before the real init is executed; the block device identity is kept so
//! a failed handoff can re-attach it for the failure receipt.

use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, FileType, Mode, makedev, mknodat};
use rustix::mount::{UnmountFlags, mount, unmount};

use crate::gpt;
use crate::receipt::{ESP_MOUNT_FLAGS_RO, ESP_MOUNT_FLAGS_RW, Failure, Stage};

/// Runtime ESP mount point from the layout contract.
pub const ESP_MOUNT_POINT: &str = "/debug_ramdisk/esp";

/// Directory holding block device nodes created by the loader.
const DEVICE_DIR: &str = "/dev/espinit";

/// sysfs whole-disk directory.
const SYS_BLOCK: &str = "/sys/block";

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

/// The mounted ESP and the information needed to tear it down before the real
/// init runs, or to re-attach it for a failure receipt afterwards.
///
/// The block node lives under `/dev`, which the handoff teardown detaches too,
/// so the remembered major/minor pair is what makes a receipt possible after a
/// failed handoff exec.
pub struct Mount {
    node: String,
    major: u32,
    minor: u32,
    path: String,
    detached: bool,
}

impl Mount {
    /// Runtime mount point of the ESP payload tree.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Whether the ESP has already been detached from the mount namespace.
    pub fn is_detached(&self) -> bool {
        self.detached
    }

    /// Detach the ESP from the mount namespace before the real init is
    /// executed. Android's first-stage init must not inherit an espinit mount,
    /// and the lazy detach keeps the mount alive until nothing references it.
    pub fn detach(&mut self) -> Result<(), Failure> {
        unmount(&self.path, UnmountFlags::DETACH).map_err(|error| {
            Failure::at(
                Stage::Handoff,
                Some(&self.path),
                "HandoffUnmountFailed",
                format!(
                    "cannot detach the ESP {} ({}:{}) at {}: {error}",
                    self.node, self.major, self.minor, self.path
                ),
            )
        })?;

        self.detached = true;

        Ok(())
    }

    /// Best-effort re-attach of a detached ESP so a failed handoff can still
    /// persist its receipt. The block node is recreated from the remembered
    /// major/minor pair because the `/dev` holding the original is gone.
    pub fn reattach_for_receipt(&mut self) -> Result<(), Failure> {
        fs::create_dir_all(&self.path).map_err(|error| {
            Failure::new(
                Stage::Storage,
                "EspMountPointCreate",
                format!("cannot create {}: {error}", self.path),
            )
        })?;

        let node = create_block_node("esp", self.major, self.minor)
            .map_err(|detail| Failure::new(Stage::Storage, "EspDeviceNodeCreate", detail))?;

        mount(&node, &self.path, "vfat", ESP_MOUNT_FLAGS_RW, "").map_err(|error| {
            Failure::new(
                Stage::Storage,
                "EspMount",
                format!(
                    "cannot re-attach {node} at {} for the failure receipt: {error}",
                    self.path
                ),
            )
        })?;

        self.detached = false;

        Ok(())
    }
}

/// Discover the single ESP and mount it read-only at [`ESP_MOUNT_POINT`].
pub fn mount_esp() -> Result<Mount, Failure> {
    let device = discover_esp()?;
    let node = create_block_node("esp", device.major, device.minor)
        .map_err(|detail| Failure::new(Stage::Storage, "EspDeviceNodeCreate", detail))?;
    mount_read_only(&node)?;

    log::info!(
        "Mounted ESP partition {}({}:{}) at {}",
        device.disk,
        device.major,
        device.minor,
        ESP_MOUNT_POINT
    );

    Ok(Mount {
        node,
        major: device.major,
        minor: device.minor,
        path: ESP_MOUNT_POINT.to_owned(),
        detached: false,
    })
}

/// Detach an early mount that espinit created itself, in preparation for the
/// real init. Only mounts created by espinit may be passed here.
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

/// Walk every whole block disk and return the one ESP partition.
fn discover_esp() -> Result<EspDevice, Failure> {
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

    match found.len() {
        0 => Err(Failure::new(
            Stage::Storage,
            "EspNotFound",
            "no EFI System Partition found on any whole block disk",
        )),
        1 => Ok(found.remove(0)),
        count => Err(Failure::new(
            Stage::Storage,
            "EspAmbiguous",
            format!("found {count} EFI System Partitions; exactly one is required"),
        )),
    }
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

/// Mount the discovered ESP read-only at the runtime mount point.
fn mount_read_only(node: &str) -> Result<(), Failure> {
    fs::create_dir_all(ESP_MOUNT_POINT).map_err(|error| {
        Failure::new(
            Stage::Storage,
            "EspMountPointCreate",
            format!("cannot create {ESP_MOUNT_POINT}: {error}"),
        )
    })?;

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
    Path::new(mount).join("espinit")
}
