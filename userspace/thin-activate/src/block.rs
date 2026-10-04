// SPDX-License-Identifier: GPL-3.0-only
use lvm2_meta::DeviceNumber;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

const SYS_CLASS_BLOCK: &str = "/sys/class/block";
const NODE: &str = "/dev/espinit/lvm-pv";

pub struct PhysicalVolume {
    pub file: File,
    pub number: DeviceNumber,
    pub sectors: u64,
}

fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
}

fn decimal(value: &str) -> io::Result<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid decimal sysfs value",
        ));
    }
    value
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "sysfs integer overflow"))
}

fn device_number(value: &str) -> io::Result<DeviceNumber> {
    let (major, minor) = value
        .trim()
        .split_once(':')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid sysfs device number"))?;
    Ok(DeviceNumber {
        major: u32::try_from(decimal(major)?)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "device major exceeds u32"))?,
        minor: u32::try_from(decimal(minor)?)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "device minor exceeds u32"))?,
    })
}

fn ensure_node(number: DeviceNumber) -> io::Result<File> {
    fs::create_dir_all("/dev/espinit")?;
    let path = Path::new(NODE);
    let expected = libc::makedev(number.major, number.minor);
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_block_device() || metadata.rdev() != expected {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "existing LVM PV node has the wrong identity",
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let path = CString::new(NODE).expect("constant path has no NUL");
            // SAFETY: `path` is a valid NUL-terminated pathname and `expected`
            // came from the exact sysfs major/minor pair selected below.
            if unsafe { libc::mknod(path.as_ptr(), libc::S_IFBLK | 0o600, expected) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Err(error) => return Err(error),
    }
    OpenOptions::new().read(true).open(path)
}

pub fn open_userdata() -> io::Result<PhysicalVolume> {
    let mut found: Option<(PathBuf, String)> = None;
    for entry in fs::read_dir(SYS_CLASS_BLOCK)? {
        let path = entry?.path();
        let uevent = match fs::read_to_string(path.join("uevent")) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if field(&uevent, "PARTNAME") != Some("userdata") {
            continue;
        }
        if field(&uevent, "DEVTYPE") != Some("partition") || !path.join("partition").is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "userdata sysfs match is not a partition",
            ));
        }
        if found.replace((path, uevent)).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "multiple physical partitions are named userdata",
            ));
        }
    }
    let (path, _) = found.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "physical userdata partition is absent",
        )
    })?;
    let number = device_number(&fs::read_to_string(path.join("dev"))?)?;
    let sectors = decimal(fs::read_to_string(path.join("size"))?.trim())?;
    let file = ensure_node(number)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_block_device()
        || metadata.rdev() != libc::makedev(number.major, number.minor)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "opened LVM PV node changed identity",
        ));
    }
    Ok(PhysicalVolume {
        file,
        number,
        sectors,
    })
}
