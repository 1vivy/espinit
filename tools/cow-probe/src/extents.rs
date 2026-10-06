//! `FS_IOC_FIEMAP` extents of the probe file, plus the alignment and
//! allocation rules the probe refuses to work around.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

use crate::ioctl::{self, IOC_READ, IOC_WRITE};

/// Filesystem block granularity shared with the pattern writer.
pub const BLOCK: u64 = 4096;

/// `_IOWR('f', 11, struct fiemap)` from `linux/fs.h`; the encoded size is
/// `sizeof(struct fiemap)` (32), the flexible extent array is not counted.
const FS_IOC_FIEMAP: u64 = ioctl::ioc(IOC_READ | IOC_WRITE, b'f', 11, 32);

const FIEMAP_EXTENT_LAST: u32 = 0x1;
const FIEMAP_EXTENT_UNKNOWN: u32 = 0x2;
const FIEMAP_EXTENT_DELALLOC: u32 = 0x4;

/// Extents requested per `FS_IOC_FIEMAP` call.
const MAX_EXTENTS: usize = 64;

/// One allocated range of the probe file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    /// Offset inside the file.
    pub logical: u64,
    /// Offset on the filesystem's block device.
    pub physical: u64,
    pub length: u64,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawExtent {
    logical: u64,
    physical: u64,
    length: u64,
    reserved64: [u64; 2],
    flags: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct RawFiemap {
    start: u64,
    length: u64,
    flags: u32,
    mapped: u32,
    count: u32,
    reserved: u32,
    extents: [RawExtent; MAX_EXTENTS],
}

/// Every allocated extent of `file`, in logical order, from `0` to `size`.
pub fn fiemap(file: &File, size: u64) -> io::Result<Vec<Extent>> {
    let mut extents = Vec::new();
    let mut start = 0u64;
    while start < size {
        let mut request = RawFiemap {
            start,
            length: size - start,
            flags: 0,
            mapped: 0,
            count: MAX_EXTENTS as u32,
            reserved: 0,
            extents: std::array::from_fn(|_| RawExtent::default()),
        };
        // SAFETY: `request` is a live `struct fiemap` with room for
        // `MAX_EXTENTS` extents and stays borrowed for the duration of the
        // call; the kernel writes only within `count` extents.
        let result = unsafe {
            ioctl::call(
                file.as_raw_fd(),
                FS_IOC_FIEMAP,
                std::ptr::from_mut(&mut request).cast(),
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if request.mapped == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("fiemap reported no extent at offset {start}"),
            ));
        }
        let mapped = request.mapped as usize;
        for raw in &request.extents[..mapped] {
            extents.push(Extent {
                logical: raw.logical,
                physical: raw.physical,
                length: raw.length,
                flags: raw.flags,
            });
        }
        let last = request.extents[mapped - 1];
        let end = last.logical + last.length;
        if last.flags & FIEMAP_EXTENT_LAST != 0 || end >= size {
            break;
        }
        if end <= start {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("fiemap made no progress past offset {start}"),
            ));
        }
        start = end;
    }
    Ok(extents)
}

/// Refuse a file whose extents cannot be compared raw: unallocated
/// (`DELALLOC`), unresolvable (`UNKNOWN`) or not 4 KiB aligned.
pub fn validate(extents: &[Extent], size: u64) -> Result<(), String> {
    if extents.is_empty() {
        return Err("fiemap returned no extents".to_string());
    }
    let mut expected = 0u64;
    for (index, extent) in extents.iter().enumerate() {
        if extent.length == 0 {
            return Err(format!("extent {index}: zero length"));
        }
        if extent.flags & (FIEMAP_EXTENT_UNKNOWN | FIEMAP_EXTENT_DELALLOC) != 0 {
            return Err(format!(
                "extent {index}: flags {:#x} include UNKNOWN or DELALLOC",
                extent.flags
            ));
        }
        if extent.logical % BLOCK != 0 || extent.physical % BLOCK != 0 || extent.length % BLOCK != 0
        {
            return Err(format!(
                "extent {index}: not {BLOCK}-aligned (logical {} physical {} length {})",
                extent.logical, extent.physical, extent.length
            ));
        }
        if extent.logical != expected {
            return Err(format!(
                "extent {index}: logical {} does not continue at {expected}",
                extent.logical
            ));
        }
        expected += extent.length;
    }
    if expected != size {
        return Err(format!("extents cover {expected} bytes, expected {size}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extent(logical: u64, physical: u64, length: u64, flags: u32) -> Extent {
        Extent {
            logical,
            physical,
            length,
            flags,
        }
    }

    #[test]
    fn ioctl_numbers_match_the_ndk_headers() {
        assert_eq!(FS_IOC_FIEMAP, 0xc020_660b);
    }

    #[test]
    fn accepts_a_contiguous_aligned_file() {
        let extents = [
            extent(0, 0x1000, 8192, FIEMAP_EXTENT_LAST),
            extent(8192, 0x8000, 4096, 0),
        ];
        assert_eq!(validate(&extents, 12288), Ok(()));
    }

    #[test]
    fn rejects_unaligned_or_unknown_extents() {
        assert!(validate(&[extent(0, 0x1001, 4096, 0)], 4096).is_err());
        assert!(validate(&[extent(0, 0x1000, 4097, 0)], 4096).is_err());
        assert!(validate(&[extent(0, 0x1000, 4096, FIEMAP_EXTENT_DELALLOC)], 4096).is_err());
        assert!(validate(&[extent(0, 0x1000, 4096, FIEMAP_EXTENT_UNKNOWN)], 4096).is_err());
    }

    #[test]
    fn rejects_holes_and_short_coverage() {
        assert!(validate(&[extent(0, 0x1000, 4096, 0)], 8192).is_err());
        assert!(validate(&[extent(4096, 0x1000, 4096, 0)], 8192).is_err());
        assert!(validate(&[], 0).is_err());
    }
}
