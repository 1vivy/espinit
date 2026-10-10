//! GPT ESP discovery, explicit selection and one owned RW/noexec raw mount.
use crate::gpt;
use crate::receipt::{Failure, Stage};
use anyhow::{Context, Result, ensure};
use egysk_runtime::{context::*, fsutil as fsu};
use rustix::fs::{CWD, FileType, Mode, makedev, mknodat};
use std::fs::{self, File};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt};
use std::path::Path;
const SYS_BLOCK: &str = "/sys/block";
const SYSFS_SECTOR: u64 = 512;
const MAX_TABLE_BYTES: usize = 1 << 20;
struct EspDevice {
    major: u32,
    minor: u32,
    disk: String,
}

pub fn mount_esp(explicit: Option<&str>) -> Result<(u32, u32)> {
    ensure!(
        !fsu::mounted(Path::new(ESP))?,
        "ESP already mounted by another owner"
    );
    if let Some(source) = explicit {
        egysk_runtime::context::absolute(source)?;
        let metadata = fs::metadata(source)?;
        ensure!(
            metadata.file_type().is_block_device(),
            "explicit ESP is not a block device"
        );
        mount_selected(source)?;
        ensure!(
            Path::new(PACKAGE_CONFIG).is_file(),
            "explicit ESP lacks product config"
        );
        let major = u32::try_from(i64::from(libc::major(metadata.rdev())))
            .context("invalid explicit ESP major")?;
        let minor = u32::try_from(i64::from(libc::minor(metadata.rdev())))
            .context("invalid explicit ESP minor")?;
        return Ok((major, minor));
    }
    let devices = discover_esps()?;
    let mut selected = None;
    for device in devices {
        let node = create_block_node(
            &format!("{}-{}", device.major, device.minor),
            device.major,
            device.minor,
        )
        .map_err(anyhow::Error::msg)?;
        // Probe candidates read-only; the selected final mount is always RW.
        if let Err(error) = fsu::mount(
            &node,
            Path::new(ESP),
            ESP_MOUNT.fs_type,
            libc::MS_RDONLY | ESP_MOUNT.flags,
            ESP_MOUNT.data,
        ) {
            log::warn!("ESP probe {}: {error:#}", device.disk);
            continue;
        }
        let found = fsu::optional_text(Path::new(PACKAGE_CONFIG), 65536);
        fsu::unmount(Path::new(ESP))?;
        if found?.is_some() {
            ensure!(selected.is_none(), "ambiguous product ESP");
            selected = Some(device);
        }
    }
    let selected = selected.context("no GPT ESP contains egysk/egysk.toml")?;
    let node = create_block_node("selected", selected.major, selected.minor)
        .map_err(anyhow::Error::msg)?;
    mount_selected(&node)?;
    Ok((selected.major, selected.minor))
}
fn mount_selected(source: &str) -> Result<()> {
    fsu::mount(
        source,
        Path::new(ESP),
        ESP_MOUNT.fs_type,
        ESP_MOUNT.flags,
        ESP_MOUNT.data,
    )
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
fn read_dev(path: &Path) -> Option<(u32, u32)> {
    let value = fs::read_to_string(path).ok()?;
    let (major, minor) = value.trim().split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn read_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}
