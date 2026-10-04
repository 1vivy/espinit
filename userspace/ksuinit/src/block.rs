//! Resolution of projection backends.
//!
//! The selected ROM config names a backend in one of four documented forms. Resolution runs
//! immediately before the `gpt` entry, after every earlier ordered module and
//! its scripts have run, so a logical volume, mapper device, loop or ESP file
//! published by them is visible:
//!
//! * `/dev/block/by-name/<PARTNAME>` is resolved by scanning
//!   `/sys/class/block/*/uevent` for the exact, unique `PARTNAME`; an owned
//!   stable block node is created from the major/minor sysfs reports.
//! * `/dev/mapper/<name>` is resolved by scanning `/sys/class/block/dm-*/dm/name`
//!   for the exact, unique device-mapper name; an owned stable block node is
//!   created from the device-mapper device sysfs reports.
//! * an existing `/dev/loopN` is accepted as it is, after its sysfs device
//!   number and loop identity are verified.
//! * `esp-file:<relative-path>` names a preallocated regular file inside the
//!   already-mounted read-only ESP. It is attached read-only through the
//!   standard loop-control/loop ioctls with zero offset and no size limit while
//!   the ESP stays read-only; the loop and backing-file guards stay open until
//!   the projection has been applied.
//!
//! Nothing else is accepted: no whole logical unit, no arbitrary path, no
//! symlink, no offset or size slicing. Missing devices and sysfs entries are
//! [`is_pending`] so a caller may retry them within a bounded window; every
//! other failure stops boot immediately, and there is never a fallback to
//! another device.

use std::fs::{self, File};
use std::io;
use std::ops::Range;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, FileType, Mode, OFlags, makedev, mknodat};
use syscalls::{Sysno, syscall};

use crate::gpt_uapi::{GPT_MAX_HIDDEN, GptDevice};

/// Directory holding the stable block nodes owned by espinit.
const BACKEND_DIR: &str = "/dev/espinit/backends";

/// sysfs block-class directory.
const SYS_CLASS_BLOCK: &str = "/sys/class/block";

/// Documented by-name backend prefix.
const BY_NAME_PREFIX: &str = "/dev/block/by-name/";

/// Documented device-mapper backend prefix.
const MAPPER_PREFIX: &str = "/dev/mapper/";

/// Documented loop backend prefix.
const LOOP_PREFIX: &str = "/dev/loop";

/// Documented read-only ESP regular-file backend prefix.
const ESP_FILE_PREFIX: &str = "esp-file:";

/// Loop-control node used to allocate a free loop device.
const LOOP_CONTROL: &str = "/dev/loop-control";

/// Mount table read to prove that the ESP is mounted read-only.
const MOUNTS: &str = "/proc/self/mounts";

/// Bounds on an `esp-file:` relative path, matching the payload path limit.
const MAX_RELATIVE_PATH_BYTES: usize = 4096;

const LOOP_SET_FD: u32 = 0x4c00;
const LOOP_CLR_FD: u32 = 0x4c01;
const LOOP_SET_STATUS64: u32 = 0x4c04;
const LOOP_GET_STATUS64: u32 = 0x4c05;
const LOOP_CTL_GET_FREE: u32 = 0x4c82;

const LO_FLAGS_READ_ONLY: u32 = 1;
const LO_FLAGS_AUTOCLEAR: u32 = 4;

/// Size of `struct loop_info64` from `linux/loop.h`.
const LOOP_INFO64_SIZE: usize = 232;

/// Byte ranges of the `loop_info64` members the loader writes or reads back:
/// `lo_offset`, `lo_sizelimit` and `lo_flags`. Every other member stays zero,
/// which is what the kernel expects for an unencrypted, untagged loop device.
const LOOP_INFO64_OFFSET: Range<usize> = 24..32;
const LOOP_INFO64_SIZELIMIT: Range<usize> = 32..40;
const LOOP_INFO64_FLAGS: Range<usize> = 52..56;

/// A zeroed `struct loop_info64` status buffer, addressed by the fixed offsets
/// above. The size is asserted here and the offsets in the tests, so the buffer
/// cannot drift from the kernel layout.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct LoopInfo64 {
    bytes: [u8; LOOP_INFO64_SIZE],
}

impl Default for LoopInfo64 {
    fn default() -> Self {
        Self {
            bytes: [0; LOOP_INFO64_SIZE],
        }
    }
}

const _: () = assert!(std::mem::size_of::<LoopInfo64>() == LOOP_INFO64_SIZE);

impl LoopInfo64 {
    /// Set `lo_flags` before the kernel copies the buffer in.
    fn set_flags(&mut self, flags: u32) {
        self.bytes[LOOP_INFO64_FLAGS].copy_from_slice(&flags.to_ne_bytes());
    }

    /// `lo_flags` as read back from the kernel.
    fn flags(&self) -> u32 {
        u32::from_ne_bytes(fixed_field(&self.bytes, LOOP_INFO64_FLAGS))
    }

    /// `lo_offset` as read back from the kernel.
    fn offset(&self) -> u64 {
        u64::from_ne_bytes(fixed_field(&self.bytes, LOOP_INFO64_OFFSET))
    }

    /// `lo_sizelimit` as read back from the kernel.
    fn sizelimit(&self) -> u64 {
        u64::from_ne_bytes(fixed_field(&self.bytes, LOOP_INFO64_SIZELIMIT))
    }
}

/// Read one fixed-width field out of the status buffer at a checked offset.
fn fixed_field<const N: usize>(bytes: &[u8; LOOP_INFO64_SIZE], range: Range<usize>) -> [u8; N] {
    bytes[range]
        .try_into()
        .expect("loop_info64 field has a fixed width")
}

/// One resolved backend: the stable block node to project and its device.
#[derive(Debug)]
pub struct ResolvedBackend {
    /// Stable block node created or accepted by the resolver.
    pub path: String,
    /// Resolved device number; the projection identity.
    pub rdev: u64,
    /// Owned loop attachment for an `esp-file:` backend. It keeps the loop
    /// device and its read-only backing file guarded until the projection has
    /// been applied, and releases them afterwards.
    pub guard: Option<LoopAttachment>,
}

/// A read-only loop attachment created by the resolver for an ESP file.
///
/// Both file descriptors are opened with `O_CLOEXEC` and the loop device is
/// marked `LO_FLAGS_AUTOCLEAR`, so the attachment survives the `gpt` lower
/// opens and the real-init exec and detaches once the last opener closes. Drop
/// clears the loop explicitly so a failed APPLY releases it immediately.
#[derive(Debug)]
pub struct LoopAttachment {
    device: File,
    backing: File,
    /// Loop device node, e.g. `/dev/loop3`.
    pub node: String,
}

impl Drop for LoopAttachment {
    fn drop(&mut self) {
        log::debug!(
            "releasing ESP file loop attachment {} (backing fd {})",
            self.node,
            self.backing.as_raw_fd()
        );
        let _ = ioctl(self.device.as_raw_fd(), LOOP_CLR_FD, 0);
    }
}

/// Whether `path` is one of the four accepted backend forms. Only the shape is
/// checked here; the device itself is resolved by [`resolve`].
pub fn is_supported(path: &str) -> bool {
    if let Some(label) = path.strip_prefix(BY_NAME_PREFIX) {
        return is_name_component(label);
    }

    if let Some(name) = path.strip_prefix(MAPPER_PREFIX) {
        return is_name_component(name);
    }

    if let Some(relative) = path.strip_prefix(ESP_FILE_PREFIX) {
        return is_safe_relative_path(relative);
    }

    match path.strip_prefix(LOOP_PREFIX) {
        Some(number) => !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()),
        None => false,
    }
}

/// Whether `path` is the read-only ESP regular-file backend form.
pub fn is_esp_file(path: &str) -> bool {
    path.starts_with(ESP_FILE_PREFIX)
}

/// Only absent devices or sysfs entries can become available during
/// enumeration. Invalid metadata and all other I/O failures must stop boot
/// immediately.
pub fn is_pending(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

/// Resolve one backend to a stable block node. A by-name or mapper backend gets
/// an owned node created from sysfs, an existing loop device is reused, and an
/// `esp-file:` backend is attached read-only to a fresh loop device.
///
/// Missing, ambiguous, non-partition, malformed, unsafe or non-read-only
/// backends are errors; there is never a fallback to another device. Only an
/// absent device or sysfs entry is [`is_pending`], so a caller may retry it
/// within a bounded enumeration window while every other failure stops boot
/// immediately.
pub fn resolve(path: &str, esp_mount: &str) -> io::Result<ResolvedBackend> {
    if let Some(label) = path.strip_prefix(BY_NAME_PREFIX) {
        if is_name_component(label) {
            return resolve_named(label);
        }
    } else if let Some(name) = path.strip_prefix(MAPPER_PREFIX) {
        if is_name_component(name) {
            return resolve_mapper(name);
        }
    } else if let Some(relative) = path.strip_prefix(ESP_FILE_PREFIX) {
        if is_safe_relative_path(relative) {
            return attach_esp_file(esp_mount, relative);
        }
    } else if is_supported(path) {
        return resolve_loop(path);
    }

    Err(invalid(
        "backend must be /dev/block/by-name/<PARTNAME>, /dev/mapper/<name>, /dev/loopN, or esp-file:<relative-path>",
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

    Ok(ResolvedBackend {
        path: node,
        rdev,
        guard: None,
    })
}

/// Resolve an exact device-mapper name through the `dm/name` attribute of the
/// device-mapper block devices and create its stable node.
fn resolve_mapper(name: &str) -> io::Result<ResolvedBackend> {
    let mut found: Option<PathBuf> = None;

    for entry in fs::read_dir(SYS_CLASS_BLOCK)? {
        let directory = entry?.path();
        let mapped = match fs::read_to_string(directory.join("dm/name")) {
            Ok(mapped) => mapped,
            Err(error) if is_pending(&error) => continue,
            Err(error) => return Err(error),
        };

        if mapped.trim() == name {
            if found.is_some() {
                return Err(invalid(
                    "multiple device-mapper devices share the backend name",
                ));
            }
            found = Some(directory);
        }
    }

    let directory = found.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no device-mapper device has the backend name",
        )
    })?;

    let uevent = fs::read_to_string(directory.join("uevent"))?;

    if field(&uevent, "DEVTYPE") != Some("disk") {
        return Err(invalid(
            "mapper backend is not a device-mapper logical device",
        ));
    }

    let rdev = read_device(&directory)?;
    let devname = field(&uevent, "DEVNAME").ok_or_else(|| invalid("backend has no DEVNAME"))?;

    if !is_name_component(devname) {
        return Err(invalid("backend DEVNAME is not a plain device name"));
    }

    let node = create_node(devname, rdev)?;

    Ok(ResolvedBackend {
        path: node,
        rdev,
        guard: None,
    })
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
    let directory = Path::new(SYS_CLASS_BLOCK).join(name);

    if !directory.join("loop").exists() {
        return Err(invalid("loop backend is not a loop device"));
    }

    let rdev = read_device(&directory)?;

    if rdev != metadata.rdev() {
        return Err(invalid("loop backend does not match its sysfs device"));
    }

    Ok(ResolvedBackend {
        path: path.to_owned(),
        rdev,
        guard: None,
    })
}

/// Attach a preallocated ESP file to a fresh loop device, read-only, with zero
/// offset and no size limit. The ESP must already be mounted read-only: a
/// writable ESP is fatal and never a normal-boot window.
fn attach_esp_file(esp_mount: &str, relative: &str) -> io::Result<ResolvedBackend> {
    let path = esp_file_path(esp_mount, relative)?;

    let metadata = fs::metadata(&path)?;
    usable_backing(&metadata)?;

    if !mount_is_read_only(esp_mount)? {
        return Err(invalid("ESP file backends require a read-only ESP mount"));
    }

    let backing = File::open(&path)?;
    let control = loop_open(LOOP_CONTROL)?;
    let number = ioctl(control.as_raw_fd(), LOOP_CTL_GET_FREE, 0)?;

    if number > u32::MAX as usize {
        return Err(invalid("loop-control returned an unusable device number"));
    }

    let node = format!("{LOOP_PREFIX}{number}");
    let device = loop_open(&node)?;

    ioctl(
        device.as_raw_fd(),
        LOOP_SET_FD,
        backing.as_raw_fd() as usize,
    )?;

    let mut info = LoopInfo64::default();
    info.set_flags(LO_FLAGS_READ_ONLY | LO_FLAGS_AUTOCLEAR);

    if let Err(error) = ioctl(
        device.as_raw_fd(),
        LOOP_SET_STATUS64,
        std::ptr::addr_of!(info) as usize,
    ) {
        let _ = ioctl(device.as_raw_fd(), LOOP_CLR_FD, 0);
        return Err(error);
    }

    // Read the status back: the projection must be a read-only loop with zero
    // offset and no size limit, never a slice of the backing file.
    let mut applied = LoopInfo64::default();

    if let Err(error) = ioctl(
        device.as_raw_fd(),
        LOOP_GET_STATUS64,
        std::ptr::addr_of_mut!(applied) as usize,
    ) {
        let _ = ioctl(device.as_raw_fd(), LOOP_CLR_FD, 0);
        return Err(error);
    }

    if applied.offset() != 0 || applied.sizelimit() != 0 {
        let _ = ioctl(device.as_raw_fd(), LOOP_CLR_FD, 0);
        return Err(invalid(
            "ESP file backend loop has a nonzero offset or size limit",
        ));
    }

    if applied.flags() & LO_FLAGS_READ_ONLY == 0 {
        let _ = ioctl(device.as_raw_fd(), LOOP_CLR_FD, 0);
        return Err(invalid("ESP file backend loop is not read-only"));
    }

    let directory = Path::new(SYS_CLASS_BLOCK).join(format!("loop{number}"));

    if !directory.join("loop").exists() {
        let _ = ioctl(device.as_raw_fd(), LOOP_CLR_FD, 0);
        return Err(invalid("attached ESP file backend is not a loop device"));
    }

    let rdev = match read_device(&directory) {
        Ok(rdev) => rdev,
        Err(error) => {
            let _ = ioctl(device.as_raw_fd(), LOOP_CLR_FD, 0);
            return Err(error);
        }
    };

    log::info!("Attached read-only ESP file backend {relative} as {node}");

    Ok(ResolvedBackend {
        path: node.clone(),
        rdev,
        guard: Some(LoopAttachment {
            device,
            backing,
            node,
        }),
    })
}

/// Open a loop device or the loop-control node with `O_CLOEXEC`; the guards
/// must not leak into the real init.
fn loop_open(path: &str) -> io::Result<File> {
    File::options()
        .read(true)
        .write(true)
        .custom_flags(OFlags::CLOEXEC.bits() as i32)
        .open(path)
}

/// Resolve an `esp-file:` path below the ESP mount, rejecting any symlink
/// component and any component that is not a plain path element.
fn esp_file_path(esp_mount: &str, relative: &str) -> io::Result<PathBuf> {
    if !is_safe_relative_path(relative) {
        return Err(invalid("ESP file backend path is not a safe relative path"));
    }

    reject_symlinks(Path::new(esp_mount))?;

    let mut path = PathBuf::from(esp_mount);

    for component in relative.split('/') {
        path.push(component);

        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid("ESP file backend path contains a symlink"));
            }
            Ok(_) => {}
            Err(error) => return Err(error),
        }
    }

    Ok(path)
}

/// A usable ESP backing file: regular, non-empty and fully allocated. A sparse
/// file would read as zeroes over unallocated regions, so it is not a
/// preallocated image and is rejected.
fn usable_backing(metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.file_type().is_file() {
        return Err(invalid("ESP file backend is not a regular file"));
    }

    usable_backing_shape(metadata.len(), metadata.blocks())
}

fn usable_backing_shape(len: u64, blocks: u64) -> io::Result<()> {
    if len == 0 {
        return Err(invalid("ESP file backend is empty"));
    }

    if blocks.saturating_mul(512) < len {
        return Err(invalid("ESP file backend is sparse"));
    }

    Ok(())
}

/// Whether `mountpoint` is mounted read-only, from mount-table text.
fn parse_mounts_read_only(mounts: &str, mountpoint: &str) -> bool {
    mounts.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next();

        if fields.next() != Some(mountpoint) {
            return false;
        }

        fields.next();

        fields
            .next()
            .is_some_and(|options| options.split(',').any(|option| option == "ro"))
    })
}

/// Prove from the live mount table that the ESP is mounted read-only.
fn mount_is_read_only(mountpoint: &str) -> io::Result<bool> {
    let mounts = fs::read_to_string(MOUNTS)?;

    Ok(parse_mounts_read_only(&mounts, mountpoint))
}

/// Enumerate physical partitions shadowed by projected names as the sorted,
/// unique `hide` set. The mounted ESP is always retained so failure receipts
/// remain writable after APPLY. Unrelated physical partitions, loop,
/// device-mapper, whole-LU, and non-partition devices remain visible.
pub fn hidden_partitions<F>(exclude: GptDevice, is_projected: F) -> io::Result<Vec<GptDevice>>
where
    F: Fn(&str) -> bool,
{
    let mut sources = Vec::new();

    for entry in fs::read_dir(SYS_CLASS_BLOCK)? {
        let directory = entry?.path();
        let uevent = match fs::read_to_string(directory.join("uevent")) {
            Ok(uevent) => uevent,
            Err(error) if is_pending(&error) => continue,
            Err(error) => return Err(error),
        };
        let dev = match fs::read_to_string(directory.join("dev")) {
            Ok(dev) => dev,
            Err(error) if is_pending(&error) => continue,
            Err(error) => return Err(error),
        };

        sources.push((uevent, dev));
    }

    collect_hidden(&sources, exclude, is_projected)
}

/// Pure form of [`hidden_partitions`]: hide only physical partitions whose
/// `PARTNAME` collides with a projected name.
fn collect_hidden<F>(
    sources: &[(String, String)],
    exclude: GptDevice,
    is_projected: F,
) -> io::Result<Vec<GptDevice>>
where
    F: Fn(&str) -> bool,
{
    let mut devices = Vec::new();

    for (uevent, dev) in sources {
        if field(uevent, "DEVTYPE") != Some("partition") {
            continue;
        }
        let Some(partname) = field(uevent, "PARTNAME") else {
            continue;
        };
        if !is_projected(partname) {
            continue;
        }

        let (major, minor) = device_pair(dev)?;
        let device = GptDevice { major, minor };
        if device != exclude {
            devices.push(device);
        }
    }

    devices.sort_unstable();
    devices.dedup();

    if devices.len() > GPT_MAX_HIDDEN {
        return Err(invalid(
            "more colliding physical partitions than the gpt ABI can hide",
        ));
    }

    Ok(devices)
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
    let (major, minor) = device_pair(&dev)?;

    Ok(makedev(major, minor))
}

/// Parse one `major:minor` sysfs value.
fn device_pair(value: &str) -> io::Result<(u32, u32)> {
    let (major, minor) = value
        .trim()
        .split_once(':')
        .ok_or_else(|| invalid("sysfs device number is malformed"))?;

    Ok((decimal(major)?, decimal(minor)?))
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

/// A relative path rooted at the ESP mount: bounded, no absolute or trailing
/// separator, no empty/`.`/`..` component, and no separator inside a component.
pub fn is_safe_relative_path(path: &str) -> bool {
    if path.is_empty() || path.len() > MAX_RELATIVE_PATH_BYTES || path.ends_with('/') {
        return false;
    }

    path.split('/').all(|component| {
        !component.is_empty()
            && component != "."
            && component != ".."
            && component
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && byte != b'\\')
    })
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

/// Issue one ioctl and translate the raw error.
fn ioctl(fd: i32, request: u32, argument: usize) -> io::Result<usize> {
    unsafe { syscall!(Sysno::ioctl, fd, request, argument) }
        .map_err(|errno| io::Error::from_raw_os_error(errno.into_raw()))
}

fn invalid(detail: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESP: &str = "/debug_ramdisk/esp";

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
            assert!(!is_pending(&resolve(path, ESP).unwrap_err()));
        }
        assert!(!is_pending(&invalid(
            "multiple block devices share the backend PARTNAME",
        )));
        assert!(!is_pending(&invalid("loop backend is not a block device")));
    }

    #[test]
    fn supported_forms_are_exact_and_bounded() {
        for path in [
            "/dev/block/by-name/system_1",
            "/dev/mapper/lv-system",
            "/dev/loop0",
            "/dev/loop127",
            "esp-file:espinit/backing.img",
            "esp-file:images/super.img",
        ] {
            assert!(is_supported(path), "{path}");
        }

        for path in [
            "/dev/block/sda",
            "/dev/block/by-name/",
            "/dev/block/by-name/../system",
            "/dev/mapper/",
            "/dev/mapper/a/b",
            "/dev/mapper/..",
            "/dev/loop",
            "/dev/loopx",
            "esp-file:",
            "esp-file:/absolute",
            "esp-file:../escape",
            "esp-file:a//b",
            "esp-file:a/",
            "esp-file:a\\b",
        ] {
            assert!(!is_supported(path), "{path}");
        }
    }

    #[test]
    fn safe_relative_paths_reject_traversal_and_separators() {
        for path in ["a", "a/b", "espinit/backing.img", "images/super.img"] {
            assert!(is_safe_relative_path(path), "{path}");
        }

        for path in ["", "/a", "a/", "a//b", ".", "..", "a/../b", "a/./b", "a\\b"] {
            assert!(!is_safe_relative_path(path), "{path}");
        }

        assert!(!is_safe_relative_path(
            &"p".repeat(MAX_RELATIVE_PATH_BYTES + 1)
        ));
    }

    #[test]
    fn esp_backing_files_must_be_regular_nonempty_and_non_sparse() {
        usable_backing_shape(4096, 8).unwrap();
        usable_backing_shape(4096, 9).unwrap();

        for (len, blocks) in [(0, 0), (4096, 7), (1, 0)] {
            assert_eq!(
                usable_backing_shape(len, blocks).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{len} bytes in {blocks} blocks"
            );
        }
    }

    #[test]
    fn mount_table_proves_the_esp_is_read_only() {
        let mounts = "\
/dev/block/sda1 /debug_ramdisk/esp vfat ro,relatime 0 0
/dev/block/sda2 /data ext4 rw,relatime 0 0
tmpfs /tmp tmpfs rw,nosuid 0 0
";

        assert!(parse_mounts_read_only(mounts, ESP));
        assert!(!parse_mounts_read_only(mounts, "/data"));
        assert!(!parse_mounts_read_only(mounts, "/tmp"));
        assert!(!parse_mounts_read_only(mounts, "/missing"));
        // A read-only device does not make a writable mount read-only.
        assert!(!parse_mounts_read_only(
            "/dev/block/sda2 /debug_ramdisk/esp ext4 rw,relatime 0 0\n",
            ESP
        ));
    }

    #[test]
    fn hidden_set_contains_only_shadowed_names_and_is_bounded() {
        fn source(uevent: &str, dev: &str) -> (String, String) {
            (uevent.to_owned(), dev.to_owned())
        }

        let sources = vec![
            source(
                "DEVTYPE=partition\nDEVNAME=sda1\nPARTNAME=metadata\n",
                "8:1",
            ),
            source(
                "DEVTYPE=partition\nDEVNAME=sda2\nPARTNAME=userdata\n",
                "8:2",
            ),
            source("DEVTYPE=partition\nDEVNAME=sda3\nPARTNAME=esp\n", "8:3"),
            source("DEVTYPE=partition\nDEVNAME=sda4\nPARTNAME=vendor\n", "8:4"),
            source(
                "DEVTYPE=partition\nDEVNAME=sdb1\nPARTNAME=metadata\n",
                "8:1",
            ),
            source("DEVTYPE=partition\nDEVNAME=sda5\n", "8:5"),
            source("DEVTYPE=disk\nDEVNAME=sda\n", "8:0"),
            source("DEVTYPE=disk\nDEVNAME=loop0\n", "7:0"),
            source("DEVTYPE=disk\nDEVNAME=dm-0\n", "253:0"),
        ];

        let projected = ["metadata", "userdata", "esp"];
        assert_eq!(
            collect_hidden(&sources, GptDevice { major: 8, minor: 3 }, |name| projected
                .contains(&name),)
            .unwrap(),
            [
                GptDevice { major: 8, minor: 1 },
                GptDevice { major: 8, minor: 2 },
            ]
        );

        let mut oversized = Vec::new();
        for minor in 0..=(GPT_MAX_HIDDEN as u32) {
            oversized.push(source(
                "DEVTYPE=partition\nPARTNAME=metadata\n",
                &format!("8:{minor}\n"),
            ));
        }

        assert!(
            collect_hidden(&oversized, GptDevice { major: 1, minor: 1 }, |name| name
                == "metadata",)
            .is_err()
        );
    }

    #[test]
    fn device_pairs_reject_malformed_sysfs_values() {
        assert_eq!(device_pair("8:1\n").unwrap(), (8, 1));
        assert_eq!(device_pair("253:12").unwrap(), (253, 12));

        for value in ["", "8", "8:", ":1", "8:1:2", "-1:2", "8:1x"] {
            assert!(device_pair(value).is_err(), "{value}");
        }
    }

    #[test]
    fn loop_status_buffer_matches_the_kernel_layout() {
        assert_eq!(std::mem::size_of::<LoopInfo64>(), 232);
        assert_eq!(LOOP_INFO64_OFFSET, 24..32);
        assert_eq!(LOOP_INFO64_SIZELIMIT, 32..40);
        assert_eq!(LOOP_INFO64_FLAGS, 52..56);

        let mut info = LoopInfo64::default();
        assert_eq!(info.flags(), 0);
        assert_eq!(info.offset(), 0);
        assert_eq!(info.sizelimit(), 0);

        info.set_flags(LO_FLAGS_READ_ONLY | LO_FLAGS_AUTOCLEAR);
        assert_eq!(info.flags(), 5);
        assert_eq!(info.offset(), 0);
        assert_eq!(info.sizelimit(), 0);
        // A zeroed buffer keeps every unread member zero, as the kernel
        // expects for an unencrypted loop device.
        assert!(
            info.bytes[..LOOP_INFO64_OFFSET.start]
                .iter()
                .all(|byte| *byte == 0)
        );
    }
}
