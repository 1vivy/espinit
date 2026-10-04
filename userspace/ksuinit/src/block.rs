//! Early resolution of projection backends.
//!
//! `rom.toml` names a backend as `/dev/block/by-name/<PARTNAME>` or as an
//! existing `/dev/loopN`. Before Android ueventd creates any node, a by-name
//! backend is resolved by scanning `/sys/class/block/*/uevent` for the exact,
//! unique `PARTNAME`, and an owned stable block node is created from the
//! major/minor that sysfs reports. Nothing else is accepted: no whole logical
//! unit, no regular file, no symlink, no other arbitrary path.

use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, FileType, Mode, makedev, mknodat};

/// Directory holding the stable block nodes owned by espinit.
const BACKEND_DIR: &str = "/dev/espinit/backends";

/// sysfs block-class directory.
const SYS_CLASS_BLOCK: &str = "/sys/class/block";

/// Documented by-name backend prefix.
const BY_NAME_PREFIX: &str = "/dev/block/by-name/";

/// Documented loop backend prefix.
const LOOP_PREFIX: &str = "/dev/loop";

/// One resolved backend: the stable block node to project and its device.
#[derive(Debug)]
pub struct ResolvedBackend {
    /// Stable block node created or accepted by the resolver.
    pub path: String,
    /// Resolved device number; the projection identity.
    pub rdev: u64,
}

/// Whether `path` is one of the two accepted backend forms. Only the shape is
/// checked here; the device itself is resolved by [`resolve`].
pub fn is_supported(path: &str) -> bool {
    if let Some(label) = path.strip_prefix(BY_NAME_PREFIX) {
        return is_name_component(label);
    }

    match path.strip_prefix(LOOP_PREFIX) {
        Some(number) => !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()),
        None => false,
    }
}

/// Only absent devices or sysfs entries can become available during enumeration.
/// Invalid metadata and all other I/O failures must stop boot immediately.
pub fn is_pending(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

/// Resolve one backend to a stable block node, creating the node for a
/// `/dev/block/by-name/<PARTNAME>` backend and reusing an existing loop device.
///
/// Missing, ambiguous, non-partition, malformed, or unsafe backends are errors;
/// there is never a fallback to another device. Only an absent device or sysfs
/// entry is [`is_pending`], so a caller may retry it within a bounded
/// enumeration window while every other failure stops boot immediately.
pub fn resolve(path: &str) -> io::Result<ResolvedBackend> {
    if let Some(label) = path.strip_prefix(BY_NAME_PREFIX) {
        if is_name_component(label) {
            return resolve_named(label);
        }
    } else if is_supported(path) {
        return resolve_loop(path);
    }

    Err(invalid(
        "backend must be /dev/block/by-name/<PARTNAME> or /dev/loopN",
    ))
}

/// Resolve a named partition through sysfs and create its stable node.
fn resolve_named(label: &str) -> io::Result<ResolvedBackend> {
    let mut found: Option<(PathBuf, String)> = None;

    for entry in fs::read_dir(SYS_CLASS_BLOCK)? {
        let directory = entry?.path();
        let uevent = match fs::read_to_string(directory.join("uevent")) {
            Ok(uevent) => uevent,
            Err(error) if is_pending(&error) => continue,
            Err(error) => return Err(error),
        };

        if field(&uevent, "PARTNAME") == Some(label) {
            if found.is_some() {
                return Err(invalid("multiple block devices share the backend PARTNAME"));
            }
            found = Some((directory, uevent));
        }
    }

    let (directory, uevent) = found.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no block device has the backend PARTNAME",
        )
    })?;

    if field(&uevent, "DEVTYPE") != Some("partition") {
        return Err(invalid("named backend is not a partition"));
    }

    let number = fs::read_to_string(directory.join("partition"))?;
    if decimal(number.trim())? == 0 {
        return Err(invalid("named backend has an invalid partition number"));
    }

    let rdev = read_device(&directory)?;
    let name = field(&uevent, "DEVNAME").ok_or_else(|| invalid("backend has no DEVNAME"))?;

    if !is_name_component(name) {
        return Err(invalid("backend DEVNAME is not a plain device name"));
    }

    let node = create_node(name, rdev)?;

    Ok(ResolvedBackend { path: node, rdev })
}

/// Accept an existing loop device node without creating a new one.
fn resolve_loop(path: &str) -> io::Result<ResolvedBackend> {
    let metadata = fs::symlink_metadata(path)?;

    if !metadata.file_type().is_block_device() {
        return Err(invalid("loop backend is not a block device"));
    }

    let name = Path::new(path)
        .file_name()
        .ok_or_else(|| invalid("loop backend has no device name"))?;

    let rdev = read_device(&Path::new(SYS_CLASS_BLOCK).join(name))?;

    if rdev != metadata.rdev() {
        return Err(invalid("loop backend does not match its sysfs device"));
    }

    Ok(ResolvedBackend {
        path: path.to_owned(),
        rdev,
    })
}

/// Create the espinit-owned stable node for `name`, replacing any leftover
/// entry, then verify that the node is the expected block device.
fn create_node(name: &str, rdev: u64) -> io::Result<String> {
    ensure_backend_dir()?;

    let path = format!("{BACKEND_DIR}/{name}");

    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    mknodat(
        CWD,
        path.as_str(),
        FileType::BlockDevice,
        Mode::from_raw_mode(0o600),
        rdev,
    )?;

    let metadata = fs::symlink_metadata(&path)?;

    if !metadata.file_type().is_block_device()
        || metadata.rdev() != rdev
        || metadata.uid() != 0
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(invalid("unusable owned backend block node"));
    }

    Ok(path)
}

/// Create or adopt `/dev/espinit/backends`, rejecting symlinked components.
fn ensure_backend_dir() -> io::Result<()> {
    reject_symlinks(Path::new(BACKEND_DIR))?;

    fs::create_dir_all(BACKEND_DIR)?;
    fs::set_permissions(BACKEND_DIR, fs::Permissions::from_mode(0o700))?;

    let metadata = fs::symlink_metadata(BACKEND_DIR)?;

    if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
        return Err(invalid("unsafe owned backend directory"));
    }

    Ok(())
}

/// Reject any symlink among the existing components of `path`.
fn reject_symlinks(path: &Path) -> io::Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid("backend path contains a symlink"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

/// Read the sysfs device number of one block device.
fn read_device(directory: &Path) -> io::Result<u64> {
    let dev = fs::read_to_string(directory.join("dev"))?;
    let (major, minor) = dev
        .trim()
        .split_once(':')
        .ok_or_else(|| invalid("sysfs device number is malformed"))?;

    Ok(makedev(decimal(major)?, decimal(minor)?))
}

/// One `KEY=VALUE` line of a sysfs `uevent` file.
fn field<'a>(uevent: &'a str, key: &str) -> Option<&'a str> {
    uevent
        .lines()
        .filter_map(|line| line.split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value)
}

/// A single safe path component: nonempty, not `.`/`..`, no separators.
fn is_name_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'/' && byte != b'\\')
}

/// Parse an unsigned decimal sysfs value.
fn decimal(value: &str) -> io::Result<u32> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid("sysfs value is not an unsigned decimal"));
    }

    value
        .parse()
        .map_err(|_| invalid("sysfs value does not fit a device number"))
}

fn invalid(detail: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_absence_is_pending() {
        assert!(is_pending(&io::Error::new(
            io::ErrorKind::NotFound,
            "no block device has the backend PARTNAME",
        )));
        assert!(is_pending(&io::Error::from_raw_os_error(2)));
        for kind in [
            io::ErrorKind::InvalidInput,
            io::ErrorKind::InvalidData,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::AlreadyExists,
            io::ErrorKind::Interrupted,
        ] {
            assert!(!is_pending(&io::Error::new(kind, "fatal")));
        }
    }

    #[test]
    fn malformed_metadata_and_unsupported_paths_are_permanent() {
        for value in ["", "-1", "1:2", "4294967296"] {
            assert!(!is_pending(&decimal(value).unwrap_err()));
        }
        for path in ["/dev/block/sda", "/dev/loopx", "/dev/block/by-name/.."] {
            assert!(!is_pending(&resolve(path).unwrap_err()));
        }
        assert!(!is_pending(&invalid(
            "multiple block devices share the backend PARTNAME",
        )));
        assert!(!is_pending(&invalid("loop backend is not a block device")));
    }
}
