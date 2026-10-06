// SPDX-License-Identifier: GPL-3.0-only
use crate::plan::{Mapper, Target};
use lvm2_meta::DeviceNumber;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::fd::AsRawFd;
use std::thread;
use std::time::{Duration, Instant};

const BUFFER_BYTES: usize = 16 * 1024;
const DM_IOCTL_TYPE: u64 = 0xfd;
const DM_VERSION_CMD: u64 = 0;
const DM_DEV_CREATE_CMD: u64 = 3;
const DM_DEV_REMOVE_CMD: u64 = 4;
const DM_DEV_SUSPEND_CMD: u64 = 6;
const DM_TABLE_LOAD_CMD: u64 = 9;
const DM_TABLE_STATUS_CMD: u64 = 12;
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

    fn load(&mut self, name: &str, targets: &[Target]) -> Result<(), String> {
        if targets.is_empty() || targets.len() > 4096 {
            return Err("invalid device-mapper target count".to_owned());
        }
        self.prepare(Some(name))?;
        self.header_mut().target_count = targets.len() as u32;
        let mut offset = size_of::<DmIoctl>();
        for target in targets {
            let params = target.params.as_bytes();
            if target.kind.is_empty()
                || target.kind.len() >= 16
                || target.kind.as_bytes().contains(&0)
                || params.contains(&0)
            {
                return Err(format!("invalid table target for {name}"));
            }
            let record_size = align8(
                size_of::<DmTargetSpec>()
                    .checked_add(params.len() + 1)
                    .ok_or("device-mapper table size overflow")?,
            )
            .ok_or("device-mapper table size overflow")?;
            let end = offset
                .checked_add(record_size)
                .filter(|end| *end <= BUFFER_BYTES)
                .ok_or_else(|| format!("device-mapper table for {name} exceeds 16 KiB"))?;
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
                self.bytes_mut()
                    .as_mut_ptr()
                    .add(offset)
                    .cast::<DmTargetSpec>()
                    .write(spec);
            }
            let params_start = offset + size_of::<DmTargetSpec>();
            self.bytes_mut()[params_start..params_start + params.len()].copy_from_slice(params);
            self.bytes_mut()[params_start + params.len()] = 0;
            offset = end;
        }
        self.call(DM_TABLE_LOAD)
            .map_err(|error| format!("DM_TABLE_LOAD for {name} failed: {error}"))?;
        self.prepare(Some(name))?;
        self.call(DM_DEV_SUSPEND)
            .map_err(|error| format!("DM resume for {name} failed: {error}"))
    }

    fn lookup(name: &str, expected_sectors: u64) -> Result<DeviceNumber, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
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
                if sectors != expected_sectors {
                    return Err(format!(
                        "device-mapper device {name} has {sectors} sectors, expected {expected_sectors}"
                    ));
                }
                found = Some(number);
            }
            if let Some(number) = found {
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

    fn remove(&mut self, name: &str) {
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
            if existing.len() != targets.len()
                || !existing.iter().zip(targets).all(|(left, right)| {
                    left.start == right.start
                        && left.length == right.length
                        && left.kind == right.kind
                        && trim_params(&left.params) == trim_params(&right.params)
                })
            {
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
        self.load(name, targets)?;
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
    }

    #[test]
    fn parameter_comparison_ignores_only_trailing_spaces() {
        assert_eq!(trim_params("1:2 128 "), "1:2 128");
        assert_ne!(trim_params("1:2 128"), "1:2  128");
    }
}
