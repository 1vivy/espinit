// SPDX-License-Identifier: GPL-3.0-only
//! Shared device-mapper ioctl client.
//!
//! One `DeviceMapper` drives the raw `DM_*` ioctl ABI used by the ESP payload
//! helpers: `thin-activate` activates LVM2-derived tables and the Android-side
//! `fw-views` helper creates and removes independent thin devices. Tables are
//! only ever derived from checked metadata or built by the caller; nothing here
//! parses a user-supplied command string or evaluates a shell.
//!
//! [`DeviceMapper::message`] adds the `DM_TARGET_MSG` command so a thin-pool
//! target can be told to `create_thin`/`delete` a device id, which is how
//! `fw-views` owns the thin ids LVM2 metadata never names.
//!
//! [`DeviceMapper::create`] and [`DeviceMapper::reload`] publish one table and
//! swap one table respectively, which is what the OTA transaction needs: a
//! per-base switch device is created at PID 1 as an error target and later
//! reloaded to point at a staging LV, then back at a read-only loop of the ESP
//! image. Both accept a table that already equals the requested one, so a
//! re-run of a boot-time helper is idempotent instead of an error.

pub use lvm2_meta::DeviceNumber;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::fd::AsRawFd;
use std::thread;
use std::time::{Duration, Instant};

/// One device-mapper table target: `start length kind params`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub start: u64,
    pub length: u64,
    pub kind: String,
    pub params: String,
}

/// A device-mapper table publisher. Implemented by [`DeviceMapper`] on a real
/// system and by recorders in tests, so the table derivation stays testable
/// without a kernel.
pub trait Mapper {
    fn activate(&mut self, name: &str, targets: &[Target]) -> Result<DeviceNumber, String>;
}

/// A target message that the kernel refused.
#[derive(Debug)]
pub enum MessageError {
    /// The target reported that the requested object already exists. A thin-pool
    /// `create_thin` for an id created on an earlier boot is the expected case.
    AlreadyExists,
    /// Every other failure, including an absent device or an unknown message.
    Failed(io::Error),
}

const BUFFER_BYTES: usize = 16 * 1024;
const MAX_TARGETS: usize = 4096;
const DM_IOCTL_TYPE: u64 = 0xfd;
const DM_VERSION_CMD: u64 = 0;
const DM_DEV_CREATE_CMD: u64 = 3;
const DM_DEV_REMOVE_CMD: u64 = 4;
const DM_DEV_SUSPEND_CMD: u64 = 6;
const DM_TABLE_LOAD_CMD: u64 = 9;
const DM_TABLE_STATUS_CMD: u64 = 12;
const DM_TARGET_MSG_CMD: u64 = 14;
/// `DM_TABLE_LOAD` with this flag makes the table read-only.
const DM_READONLY_FLAG: u32 = 1 << 0;
/// `DM_DEV_SUSPEND` with this flag suspends; without it, resumes and swaps in
/// the table loaded since the last resume.
const DM_SUSPEND_FLAG: u32 = 1 << 1;
const DM_STATUS_TABLE_FLAG: u32 = 1 << 4;
const DM_ACTIVE_PRESENT_FLAG: u32 = 1 << 5;
const DM_BUFFER_FULL_FLAG: u32 = 1 << 8;

const fn ioctl(command: u64) -> libc::Ioctl {
    ((3_u64 << 30) | ((size_of::<DmIoctl>() as u64) << 16) | (DM_IOCTL_TYPE << 8) | command)
        as libc::Ioctl
}

const DM_VERSION: libc::Ioctl = ioctl(DM_VERSION_CMD);
const DM_DEV_CREATE: libc::Ioctl = ioctl(DM_DEV_CREATE_CMD);
const DM_DEV_REMOVE: libc::Ioctl = ioctl(DM_DEV_REMOVE_CMD);
const DM_DEV_SUSPEND: libc::Ioctl = ioctl(DM_DEV_SUSPEND_CMD);
const DM_TABLE_LOAD: libc::Ioctl = ioctl(DM_TABLE_LOAD_CMD);
const DM_TABLE_STATUS: libc::Ioctl = ioctl(DM_TABLE_STATUS_CMD);
const DM_TARGET_MSG: libc::Ioctl = ioctl(DM_TARGET_MSG_CMD);

#[repr(C)]
struct DmIoctl {
    version: [u32; 3],
    data_size: u32,
    data_start: u32,
    target_count: u32,
    open_count: i32,
    flags: u32,
    event_nr: u32,
    padding: u32,
    dev: u64,
    name: [u8; 128],
    uuid: [u8; 129],
    data: [u8; 7],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DmTargetSpec {
    sector_start: u64,
    length: u64,
    status: i32,
    next: u32,
    target_type: [u8; 16],
}

const _: () = {
    assert!(size_of::<DmIoctl>() == 312);
    assert!(offset_of!(DmIoctl, name) == 48);
    assert!(offset_of!(DmIoctl, uuid) == 176);
    assert!(size_of::<DmTargetSpec>() == 40);
};

fn align8(value: usize) -> Option<usize> {
    value.checked_add(7).map(|value| value & !7)
}

fn trim_params(value: &str) -> &str {
    value.trim_end_matches(' ')
}

/// Whether an existing table already is the requested one. Trailing spaces in
/// the kernel's params are insignificant, everything else is compared exactly.
fn same_table(existing: &[Target], requested: &[Target]) -> bool {
    existing.len() == requested.len()
        && existing.iter().zip(requested).all(|(left, right)| {
            left.start == right.start
                && left.length == right.length
                && left.kind == right.kind
                && trim_params(&left.params) == trim_params(&right.params)
        })
}

/// Encode a target table into the ioctl buffer after its header and return the
/// offset one past the last target record.
///
/// The layout is the kernel's: one `struct dm_target_spec` per target followed by
/// its NUL-terminated params, each record padded to eight bytes, with `next`
/// holding the record's padded size. Nothing else writes into the buffer.
fn write_table(buffer: &mut [u8], targets: &[Target]) -> Result<usize, String> {
    let mut offset = size_of::<DmIoctl>();
    for target in targets {
        let params = target.params.as_bytes();
        if target.kind.is_empty()
            || target.kind.len() >= 16
            || target.kind.as_bytes().contains(&0)
            || params.contains(&0)
        {
            return Err(format!("invalid table target {:?}", target.kind));
        }
        let record_size = align8(
            size_of::<DmTargetSpec>()
                .checked_add(params.len() + 1)
                .ok_or("device-mapper table size overflow")?,
        )
        .ok_or("device-mapper table size overflow")?;
        let end = offset
            .checked_add(record_size)
            .filter(|end| *end <= buffer.len())
            .ok_or("device-mapper table exceeds 16 KiB")?;
        let mut spec = DmTargetSpec {
            sector_start: target.start,
            length: target.length,
            status: 0,
            next: record_size as u32,
            target_type: [0; 16],
        };
        spec.target_type[..target.kind.len()].copy_from_slice(target.kind.as_bytes());
        // SAFETY: `offset` is eight-byte aligned and the checked record fits.
        unsafe {
            buffer
                .as_mut_ptr()
                .add(offset)
                .cast::<DmTargetSpec>()
                .write(spec);
        }
        let params_start = offset + size_of::<DmTargetSpec>();
        buffer[params_start..params_start + params.len()].copy_from_slice(params);
        buffer[params_start + params.len()] = 0;
        offset = end;
    }
    Ok(offset)
}

/// Wrap a rejected request as a target-message failure.
fn message_failure(detail: impl Into<String>) -> MessageError {
    MessageError::Failed(io::Error::new(io::ErrorKind::InvalidInput, detail.into()))
}

/// Encode one target message at `data_start` inside the ioctl buffer.
///
/// The record is the kernel's `struct dm_target_msg`: a `u64` sector followed by
/// a NUL-terminated message. The kernel reads exactly `strlen + 1` bytes after
/// the sector, so the message must be nonempty and NUL-free, and the encoding is
/// bounded by the same 16 KiB buffer every other request uses.
fn write_target_message(
    value: &mut [u8],
    data_start: usize,
    sector: u64,
    text: &str,
) -> Result<(), String> {
    if text.is_empty() || text.as_bytes().contains(&0) {
        return Err("invalid device-mapper target message".to_owned());
    }

    let end = data_start
        .checked_add(size_of::<u64>() + text.len() + 1)
        .filter(|end| *end <= value.len())
        .ok_or("device-mapper target message exceeds the buffer")?;

    value[data_start..data_start + size_of::<u64>()].copy_from_slice(&sector.to_ne_bytes());
    value[data_start + size_of::<u64>()..end - 1].copy_from_slice(text.as_bytes());
    value[end - 1] = 0;

    Ok(())
}

fn ensure_control() -> io::Result<File> {
    if let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/mapper/control")
    {
        return Ok(file);
    }
    let misc = fs::read_to_string("/proc/misc")?;
    let minor = misc
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let minor = fields.next()?;
            (fields.next()? == "device-mapper" && fields.next().is_none()).then_some(minor)
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "device-mapper is absent"))?
        .parse::<u32>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid device-mapper minor"))?;
    fs::create_dir_all("/dev/mapper")?;
    let path = CString::new("/dev/mapper/control").expect("constant has no NUL");
    // SAFETY: `path` is NUL terminated and the misc-device major/minor came
    // from the kernel's `/proc/misc` table.
    let result = unsafe {
        libc::mknod(
            path.as_ptr(),
            libc::S_IFCHR | 0o600,
            libc::makedev(10, minor),
        )
    };
    if result != 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
        return Err(io::Error::last_os_error());
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/mapper/control")
}

pub struct DeviceMapper {
    control: File,
    buffer: Vec<u64>,
    created: Vec<String>,
    committed: bool,
}

impl DeviceMapper {
    pub fn open() -> Result<Self, String> {
        let mut result = Self {
            control: ensure_control()
                .map_err(|error| format!("cannot open device-mapper: {error}"))?,
            buffer: vec![0; BUFFER_BYTES / size_of::<u64>()],
            created: Vec::new(),
            committed: false,
        };
        result.prepare(None)?;
        result
            .call(DM_VERSION)
            .map_err(|error| format!("DM_VERSION failed: {error}"))?;
        let version = result.header().version;
        if version[0] != 4 {
            return Err(format!("device-mapper ABI {}.x is not 4.x", version[0]));
        }
        Ok(result)
    }

    pub fn commit(mut self) {
        self.committed = true;
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: `u8` has alignment one and the byte length is exactly the
        // initialized backing allocation.
        unsafe {
            std::slice::from_raw_parts(
                self.buffer.as_ptr().cast::<u8>(),
                self.buffer.len() * size_of::<u64>(),
            )
        }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: same allocation as `bytes`; this borrow is exclusive.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.buffer.as_mut_ptr().cast::<u8>(),
                self.buffer.len() * size_of::<u64>(),
            )
        }
    }

    fn header(&self) -> &DmIoctl {
        // SAFETY: the `u64` vector provides at least eight-byte alignment and
        // `DmIoctl` fits at its beginning.
        unsafe { &*self.buffer.as_ptr().cast::<DmIoctl>() }
    }

    fn header_mut(&mut self) -> &mut DmIoctl {
        // SAFETY: same allocation as `header`; this borrow is exclusive.
        unsafe { &mut *self.buffer.as_mut_ptr().cast::<DmIoctl>() }
    }

    fn prepare(&mut self, name: Option<&str>) -> Result<(), String> {
        self.buffer.fill(0);
        let size = u32::try_from(BUFFER_BYTES).expect("buffer size fits u32");
        let start = u32::try_from(size_of::<DmIoctl>()).expect("header size fits u32");
        let header = self.header_mut();
        header.version = [4, 0, 0];
        header.data_size = size;
        header.data_start = start;
        if let Some(name) = name {
            if name.is_empty() || name.len() >= header.name.len() || name.as_bytes().contains(&0) {
                return Err("invalid device-mapper name".to_owned());
            }
            header.name[..name.len()].copy_from_slice(name.as_bytes());
        }
        Ok(())
    }

    fn call(&mut self, request: libc::Ioctl) -> io::Result<()> {
        // SAFETY: the request is one of the DM ioctls above and points at a
        // correctly aligned 16 KiB buffer beginning with `DmIoctl`.
        if unsafe { libc::ioctl(self.control.as_raw_fd(), request, self.buffer.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if self.header().flags & DM_BUFFER_FULL_FLAG != 0 {
            return Err(io::Error::other("16 KiB device-mapper buffer is too small"));
        }
        Ok(())
    }

    fn table_status(&mut self, name: &str) -> Result<Option<Vec<Target>>, String> {
        self.prepare(Some(name))?;
        self.header_mut().flags = DM_STATUS_TABLE_FLAG;
        match self.call(DM_TABLE_STATUS) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => return Ok(None),
            Err(error) => return Err(format!("DM_TABLE_STATUS for {name} failed: {error}")),
        }
        let header = self.header();
        if header.flags & DM_ACTIVE_PRESENT_FLAG == 0 || header.target_count == 0 {
            return Ok(None);
        }
        let data_start = header.data_start as usize;
        let data_size = header.data_size as usize;
        let count = header.target_count as usize;
        if data_start < size_of::<DmIoctl>() || data_start >= data_size || data_size > BUFFER_BYTES
        {
            return Err(format!(
                "DM_TABLE_STATUS for {name} returned invalid bounds"
            ));
        }
        let bytes = self.bytes();
        let mut offset = data_start;
        let mut result = Vec::with_capacity(count);
        for index in 0..count {
            if offset
                .checked_add(size_of::<DmTargetSpec>())
                .is_none_or(|end| end > data_size)
            {
                return Err(format!(
                    "DM_TABLE_STATUS for {name} returned a truncated target"
                ));
            }
            // SAFETY: bounds were checked and every kernel target record is
            // eight-byte aligned by the DM ABI.
            let spec = unsafe { &*bytes.as_ptr().add(offset).cast::<DmTargetSpec>() };
            let kind_end = spec
                .target_type
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| format!("DM_TABLE_STATUS for {name} returned an invalid target"))?;
            let kind = std::str::from_utf8(&spec.target_type[..kind_end])
                .map_err(|_| format!("DM_TABLE_STATUS for {name} returned a non-ASCII target"))?
                .to_owned();
            let record_end = if spec.next == 0 {
                data_size
            } else {
                let next = spec.next as usize;
                if next < size_of::<DmTargetSpec>() || !next.is_multiple_of(8) {
                    return Err(format!(
                        "DM_TABLE_STATUS for {name} returned an invalid next offset"
                    ));
                }
                offset
                    .checked_add(next)
                    .filter(|end| *end <= data_size)
                    .ok_or_else(|| format!("DM_TABLE_STATUS for {name} exceeded its buffer"))?
            };
            let params_start = offset + size_of::<DmTargetSpec>();
            let params_bytes = &bytes[params_start..record_end];
            let params_end = params_bytes
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| {
                    format!("DM_TABLE_STATUS for {name} returned unterminated params")
                })?;
            let params = std::str::from_utf8(&params_bytes[..params_end])
                .map_err(|_| format!("DM_TABLE_STATUS for {name} returned non-UTF-8 params"))?
                .trim_end()
                .to_owned();
            result.push(Target {
                start: spec.sector_start,
                length: spec.length,
                kind,
                params,
            });
            if index + 1 < count {
                if spec.next == 0 {
                    return Err(format!(
                        "DM_TABLE_STATUS for {name} ended before target {count}"
                    ));
                }
                offset = record_end;
            }
        }
        Ok(Some(result))
    }

    /// Encode and load a table without resuming the device: the new table stays
    /// inactive until the next resume, which is what makes [`Self::reload`] a
    /// single swap.
    fn load_table(
        &mut self,
        name: &str,
        targets: &[Target],
        read_only: bool,
    ) -> Result<(), String> {
        if targets.is_empty() || targets.len() > MAX_TARGETS {
            return Err("invalid device-mapper target count".to_owned());
        }
        self.prepare(Some(name))?;
        self.header_mut().target_count = targets.len() as u32;
        if read_only {
            self.header_mut().flags |= DM_READONLY_FLAG;
        }
        write_table(self.bytes_mut(), targets).map_err(|error| format!("{name}: {error}"))?;
        self.call(DM_TABLE_LOAD)
            .map_err(|error| format!("DM_TABLE_LOAD for {name} failed: {error}"))
    }

    /// Load a table and resume the device, making it the live one.
    fn load(&mut self, name: &str, targets: &[Target], read_only: bool) -> Result<(), String> {
        self.load_table(name, targets, read_only)?;
        self.prepare(Some(name))?;
        self.call(DM_DEV_SUSPEND)
            .map_err(|error| format!("DM resume for {name} failed: {error}"))
    }

    /// Replace the table of the device `name` and resume it.
    ///
    /// The table is loaded first, then the device is suspended and resumed, so
    /// the swap happens at the resume and the device is never live with a half
    /// written table. A table that already equals the requested one is left
    /// alone: reloading it would stall I/O for no change, and the boot-time
    /// helper is re-run on every boot.
    pub fn reload(
        &mut self,
        name: &str,
        targets: &[Target],
        read_only: bool,
    ) -> Result<(), String> {
        match self.table_status(name)? {
            None => return Err(format!("device-mapper device {name} does not exist")),
            Some(existing) if same_table(&existing, targets) => return Ok(()),
            Some(_) => {}
        }
        self.load_table(name, targets, read_only)?;
        self.prepare(Some(name))?;
        self.header_mut().flags = DM_SUSPEND_FLAG;
        self.call(DM_DEV_SUSPEND)
            .map_err(|error| format!("DM suspend for {name} failed: {error}"))?;
        self.prepare(Some(name))?;
        self.call(DM_DEV_SUSPEND)
            .map_err(|error| format!("DM resume for {name} failed: {error}"))
    }

    /// Create the device `name` with the given table.
    ///
    /// An existing device whose table is already the requested one is accepted,
    /// so a re-run of the boot-time helper is idempotent. An existing device with
    /// a *different* table is an error: changing a live table is [`Self::reload`]'s
    /// job, and a silent create would hide which of the two happened.
    pub fn create(
        &mut self,
        name: &str,
        targets: &[Target],
        read_only: bool,
    ) -> Result<(), String> {
        if let Some(existing) = self.table_status(name)? {
            if same_table(&existing, targets) {
                return Ok(());
            }
            return Err(format!("existing device-mapper table for {name} differs"));
        }
        self.prepare(Some(name))?;
        match self.call(DM_DEV_CREATE) {
            Ok(()) => self.created.push(name.to_owned()),
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                return Err(format!(
                    "device-mapper device {name} exists without an active table"
                ));
            }
            Err(error) => return Err(format!("DM_DEV_CREATE for {name} failed: {error}")),
        }
        self.load(name, targets, read_only)
    }

    /// Resolve one active device-mapper name to its device number through the
    /// `dm/name` sysfs attribute. No `/dev/mapper` node is required, which is
    /// what makes this usable before Android's ueventd has run.
    pub fn device_number(name: &str) -> Result<DeviceNumber, String> {
        match Self::find(name)? {
            Some((number, _)) => Ok(number),
            None => Err(format!("device-mapper device {name} does not exist")),
        }
    }

    /// One sysfs scan for `name`: its device number and size in sectors. An
    /// absent device is `None`; two devices with the same name are an error.
    fn find(name: &str) -> Result<Option<(DeviceNumber, u64)>, String> {
        let mut found = None;

        for entry in fs::read_dir("/sys/class/block")
            .map_err(|error| format!("cannot scan device-mapper sysfs: {error}"))?
        {
            let path = entry
                .map_err(|error| format!("cannot scan device-mapper sysfs: {error}"))?
                .path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !file_name.starts_with("dm-") {
                continue;
            }
            let mapped = match fs::read_to_string(path.join("dm/name")) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if mapped.trim_end() != name {
                continue;
            }
            if found.is_some() {
                return Err(format!("multiple device-mapper devices are named {name}"));
            }
            let dev = fs::read_to_string(path.join("dev"))
                .map_err(|error| format!("cannot read device number for {name}: {error}"))?;
            let (major, minor) = dev
                .trim()
                .split_once(':')
                .ok_or_else(|| format!("invalid device number for {name}"))?;
            let number = DeviceNumber {
                major: major
                    .parse()
                    .map_err(|_| format!("invalid major for {name}"))?,
                minor: minor
                    .parse()
                    .map_err(|_| format!("invalid minor for {name}"))?,
            };
            let sectors: u64 = fs::read_to_string(path.join("size"))
                .map_err(|error| format!("cannot read size for {name}: {error}"))?
                .trim()
                .parse()
                .map_err(|_| format!("invalid size for {name}"))?;
            found = Some((number, sectors));
        }

        Ok(found)
    }

    fn lookup(name: &str, expected_sectors: u64) -> Result<DeviceNumber, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some((number, sectors)) = Self::find(name)? {
                if sectors != expected_sectors {
                    return Err(format!(
                        "device-mapper device {name} has {sectors} sectors, expected {expected_sectors}"
                    ));
                }
                return Ok(number);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "device-mapper device {name} did not appear within 10 seconds"
                ));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Send one target message to the device named `name`.
    ///
    /// A target message is the only way to ask a thin-pool to create or delete a
    /// thin device id, because that id has no LVM2 metadata entry. The message is
    /// one `struct dm_target_msg` record: a `u64` sector followed by the
    /// NUL-terminated text.
    pub fn message(&mut self, name: &str, sector: u64, text: &str) -> Result<(), MessageError> {
        self.prepare(Some(name)).map_err(message_failure)?;
        self.header_mut().target_count = 1;

        let data_start = self.header().data_start as usize;

        write_target_message(self.bytes_mut(), data_start, sector, text)
            .map_err(message_failure)?;

        match self.call(DM_TARGET_MSG) {
            Ok(()) => Ok(()),
            // The kernel reports an already-created thin id as `EEXIST`, which
            // is the normal outcome of a boot that re-runs the creation.
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                Err(MessageError::AlreadyExists)
            }
            Err(error) => Err(MessageError::Failed(error)),
        }
    }

    /// Remove the device `name`, ignoring an absent device. A device created by
    /// this instance is dropped from the rollback list, so `Drop` does not try
    /// to remove it again.
    pub fn remove(&mut self, name: &str) {
        self.created.retain(|created| created.as_str() != name);

        if self.prepare(Some(name)).is_ok() {
            let _ = self.call(DM_DEV_REMOVE);
        }
    }
}

impl Mapper for DeviceMapper {
    fn activate(&mut self, name: &str, targets: &[Target]) -> Result<DeviceNumber, String> {
        let expected_sectors = targets
            .iter()
            .try_fold(0_u64, |end, target| {
                target
                    .start
                    .checked_add(target.length)
                    .map(|next| end.max(next))
            })
            .ok_or_else(|| format!("device-mapper size overflow for {name}"))?;
        if let Some(existing) = self.table_status(name)? {
            if !same_table(&existing, targets) {
                return Err(format!("existing device-mapper table for {name} differs"));
            }
            return Self::lookup(name, expected_sectors);
        }

        self.prepare(Some(name))?;
        match self.call(DM_DEV_CREATE) {
            Ok(()) => self.created.push(name.to_owned()),
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                return Err(format!(
                    "device-mapper device {name} exists without an active table"
                ));
            }
            Err(error) => return Err(format!("DM_DEV_CREATE for {name} failed: {error}")),
        }
        self.load(name, targets, false)?;
        Self::lookup(name, expected_sectors)
    }
}

impl Drop for DeviceMapper {
    fn drop(&mut self) {
        if !self.committed {
            while let Some(name) = self.created.pop() {
                self.remove(&name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_layout_matches_linux_uapi() {
        assert_eq!(DM_VERSION, 0xc138fd00_u32 as libc::Ioctl);
        assert_eq!(DM_TABLE_LOAD, 0xc138fd09_u32 as libc::Ioctl);
        assert_eq!(DM_TABLE_STATUS, 0xc138fd0c_u32 as libc::Ioctl);
        assert_eq!(DM_TARGET_MSG, 0xc138fd0e_u32 as libc::Ioctl);
    }

    #[test]
    fn target_message_is_a_sector_followed_by_nul_terminated_text() {
        let mut buffer = vec![0_u8; BUFFER_BYTES];
        let start = size_of::<DmIoctl>();

        write_target_message(&mut buffer, start, 7, "create_thin 131073").unwrap();

        assert_eq!(
            u64::from_ne_bytes(buffer[start..start + 8].try_into().unwrap()),
            7
        );
        let text = &buffer[start + 8..start + 8 + "create_thin 131073".len() + 1];
        assert_eq!(text, b"create_thin 131073\0");
        // Nothing beyond the record is touched.
        assert!(
            buffer[start + 8 + "create_thin 131073".len() + 1..]
                .iter()
                .all(|byte| *byte == 0)
        );
    }

    #[test]
    fn malformed_or_unbounded_target_messages_are_refused() {
        let mut buffer = vec![0_u8; BUFFER_BYTES];
        let start = size_of::<DmIoctl>();

        for text in ["", "create\0thin"] {
            assert!(
                write_target_message(&mut buffer, start, 0, text).is_err(),
                "{text}"
            );
        }

        // The record must fit inside the 16 KiB request buffer.
        let oversized = "m".repeat(BUFFER_BYTES - start);
        assert!(write_target_message(&mut buffer, start, 0, &oversized).is_err());
        assert!(write_target_message(&mut buffer, BUFFER_BYTES, 0, "delete 1").is_err());
    }

    #[test]
    fn parameter_comparison_ignores_only_trailing_spaces() {
        assert_eq!(trim_params("1:2 128 "), "1:2 128");
        assert_ne!(trim_params("1:2 128"), "1:2  128");
    }

    #[test]
    fn flag_bits_match_linux_dm_ioctl_h() {
        assert_eq!(DM_READONLY_FLAG, 1);
        assert_eq!(DM_SUSPEND_FLAG, 2);
        assert_eq!(DM_STATUS_TABLE_FLAG, 1 << 4);
        assert_eq!(DM_ACTIVE_PRESENT_FLAG, 1 << 5);
        assert_eq!(DM_BUFFER_FULL_FLAG, 1 << 8);
    }

    /// A 16 KiB request buffer with the alignment `DeviceMapper` guarantees: the
    /// kernel writes `struct dm_target_spec` records into it.
    fn aligned_buffer() -> Vec<u64> {
        vec![0; BUFFER_BYTES / size_of::<u64>()]
    }

    /// The byte view of that buffer, exactly as `DeviceMapper::bytes_mut` takes it.
    fn as_bytes(words: &mut [u64]) -> &mut [u8] {
        let length = std::mem::size_of_val(words);
        // SAFETY: `u8` has alignment one and the length is the backing allocation.
        unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), length) }
    }

    #[test]
    fn the_table_is_encoded_as_the_kernel_reads_it() {
        let targets = [
            Target {
                start: 0,
                length: 2048,
                kind: "linear".to_owned(),
                params: "8:1 0".to_owned(),
            },
            Target {
                start: 2048,
                length: 1,
                kind: "error".to_owned(),
                params: String::new(),
            },
        ];
        let mut words = aligned_buffer();
        let end = write_table(as_bytes(&mut words), &targets).unwrap();

        let start = size_of::<DmIoctl>();
        let buffer = as_bytes(&mut words);
        // SAFETY: the records were written at eight-byte aligned offsets of an
        // eight-byte aligned allocation.
        let spec = |at: usize| unsafe { &*buffer.as_ptr().add(at).cast::<DmTargetSpec>() };
        // First record: 40 bytes of spec, 6 bytes of params, padded to 48.
        let first = spec(start);
        assert_eq!(first.sector_start, 0);
        assert_eq!(first.length, 2048);
        assert_eq!(&first.target_type[..6], b"linear");
        assert_eq!(first.next, 48);
        assert_eq!(&buffer[start + 40..start + 47], b"8:1 0\0\0");
        // Second record: 40 bytes of spec, 1 byte of params, padded to 48.
        let second = spec(start + 48);
        assert_eq!(second.sector_start, 2048);
        assert_eq!(second.length, 1);
        assert_eq!(&second.target_type[..5], b"error");
        assert_eq!(second.next, 48);
        assert_eq!(buffer[start + 88], 0);
        assert_eq!(end, start + 96);
        // Nothing beyond the table is touched.
        assert!(buffer[end..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn unbounded_or_invalid_targets_are_refused() {
        let mut words = aligned_buffer();
        let target = |kind: &str, params: &str| Target {
            start: 0,
            length: 1,
            kind: kind.to_owned(),
            params: params.to_owned(),
        };

        for target in [
            target("", "1:0 0"),
            target("linear", "1\0:0 0"),
            target(&"k".repeat(16), "1:0 0"),
            target("linear", &"p".repeat(BUFFER_BYTES)),
        ] {
            assert!(write_table(as_bytes(&mut words), &[target]).is_err());
        }

        // The padded records must fit in the 16 KiB request buffer.
        let filler = target("linear", &"p".repeat(400));
        let many = vec![filler; 41];
        assert!(write_table(as_bytes(&mut words), &many).is_err());
    }

    #[test]
    fn an_identical_table_is_recognized_ignoring_trailing_spaces() {
        let target = |params: &str| Target {
            start: 0,
            length: 8,
            kind: "linear".to_owned(),
            params: params.to_owned(),
        };
        assert!(same_table(&[target("1:0 0 ")], &[target("1:0 0")]));
        assert!(!same_table(&[target("1:0 0")], &[target("1:0 1")]));
        assert!(!same_table(
            &[target("1:0 0")],
            &[target("1:0 0"), target("1:0 0")]
        ));
    }
}
