//! The `ioctl(2)` request numbers and the thin FFI shim the probe needs.
//!
//! Request numbers are computed with the `_IOC()` encoding the NDK sysroot's
//! `asm-generic/ioctl.h` defines, so the literals here stay auditable against
//! `linux/f2fs.h` and `linux/fs.h`.

use std::os::raw::{c_int, c_ulong, c_void};

/// `_IOC_WRITE`.
pub const IOC_WRITE: u32 = 1;
/// `_IOC_READ`.
pub const IOC_READ: u32 = 2;

/// `_IOC()` from `asm-generic/ioctl.h`.
pub const fn ioc(direction: u32, kind: u8, number: u8, size: u32) -> u64 {
    ((direction as u64) << 30) | ((size as u64) << 16) | ((kind as u64) << 8) | (number as u64)
}

unsafe extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn fallocate(fd: c_int, mode: c_int, offset: i64, len: i64) -> c_int;
    fn posix_fadvise(fd: c_int, offset: i64, len: i64, advice: c_int) -> c_int;
}

/// `ioctl(2)` with one pointer argument.
///
/// # Safety
/// `argument` must point to a live value of the type the request expects, for
/// the whole duration of the call.
pub unsafe fn call(fd: c_int, request: u64, argument: *mut c_void) -> c_int {
    // SAFETY: the caller upholds the request-specific argument contract and
    // the descriptor is an open file.
    unsafe { ioctl(fd, request as c_ulong, argument) }
}

/// `fallocate(2)` mode 0 over the whole file.
///
/// # Safety
/// `fd` must be an open file descriptor and `length` must fit `off_t`.
pub unsafe fn allocate(fd: c_int, length: u64) -> c_int {
    // SAFETY: the caller supplies an open descriptor; mode 0 allocates without
    // reading the length back.
    unsafe { fallocate(fd, 0, 0, length as i64) }
}

/// `POSIX_FADV_DONTNEED` from `linux/fadvise.h`.
const POSIX_FADV_DONTNEED: c_int = 4;

/// Drop the clean page-cache pages of the whole file or block device, so the
/// next read comes from the media instead of an earlier cached read.
///
/// # Safety
/// `fd` must be an open file descriptor.
pub unsafe fn drop_cache(fd: c_int) -> c_int {
    // SAFETY: the caller supplies an open descriptor; offset 0 with length 0
    // covers the whole object and the advice only discards clean pages.
    unsafe { posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED) }
}
