// SPDX-License-Identifier: GPL-3.0-only
//! Physical partition lookup by the kernel's sysfs `PARTNAME`.
//!
//! The kernel names a physical partition exactly once, in the `PARTNAME` line
//! of `/sys/class/block/<name>/uevent`. PID 1 resolves its projection backends
//! that way, and the Android-side firmware views resolve their origin
//! partitions the same way, so the lookup lives here and both see the same
//! device for the same name. There is no device-name allowlist and no
//! `/dev/block/by-name` symlink trust: only sysfs is read, and a missing or
//! ambiguous match is an error, never a fallback to another device.

use std::fs;
use std::io;

/// sysfs block-class directory.
pub const SYS_CLASS_BLOCK: &str = "/sys/class/block";

/// Resolve the exact, unique physical partition whose sysfs `PARTNAME` is
/// `name` and return its device number.
///
/// The caller creates whatever node, loop or mapping it needs from that number.
/// An absent partition is [`io::ErrorKind::NotFound`], which PID 1 classifies as
/// pending and retries within its bounded window; an ambiguous, malformed or
/// non-partition match is [`io::ErrorKind::InvalidInput`] and stops boot.
pub fn partition_by_name(name: &str) -> io::Result<libc::dev_t> {
    partition_in(name, &sources()?)
}

/// Every block device's `(uevent, dev)` text, in sysfs order. A device that
/// disappears mid-enumeration is skipped like any other absent entry.
fn sources() -> io::Result<Vec<(String, String)>> {
    let mut sources = Vec::new();

    for entry in fs::read_dir(SYS_CLASS_BLOCK)? {
        let directory = entry?.path();
        let uevent = match fs::read_to_string(directory.join("uevent")) {
            Ok(uevent) => uevent,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let dev = match fs::read_to_string(directory.join("dev")) {
            Ok(dev) => dev,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };

        sources.push((uevent, dev));
    }

    Ok(sources)
}

/// Pure form of [`partition_by_name`] over `(uevent, dev)` pairs.
pub fn partition_in(name: &str, sources: &[(String, String)]) -> io::Result<libc::dev_t> {
    let mut found = None;

    for source in sources {
        if field(&source.0, "PARTNAME") != Some(name) {
            continue;
        }
        if found.replace(source).is_some() {
            return Err(invalid("multiple block devices share the backend PARTNAME"));
        }
    }

    let (uevent, dev) = found.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no block device has the backend PARTNAME",
        )
    })?;

    // The kernel's own statement that this device is a partition, not a whole
    // logical unit; only partitions may be named backends.
    if field(uevent, "DEVTYPE") != Some("partition") {
        return Err(invalid("named backend is not a partition"));
    }

    let (major, minor) = device_pair(dev)?;

    Ok(libc::makedev(major, minor))
}

/// One `KEY=VALUE` line of a sysfs `uevent` file.
fn field<'a>(uevent: &'a str, key: &str) -> Option<&'a str> {
    uevent
        .lines()
        .filter_map(|line| line.split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value)
}

/// Parse one `major:minor` sysfs value.
fn device_pair(value: &str) -> io::Result<(u32, u32)> {
    let (major, minor) = value
        .trim()
        .split_once(':')
        .ok_or_else(|| invalid("sysfs device number is malformed"))?;

    Ok((decimal(major)?, decimal(minor)?))
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

    fn source(uevent: &str, dev: &str) -> (String, String) {
        (uevent.to_owned(), dev.to_owned())
    }

    #[test]
    fn an_exact_unique_partname_wins_and_absence_is_pending() {
        let sources = [
            source(
                "DEVTYPE=partition\nDEVNAME=sda1\nPARTNAME=metadata\n",
                "8:1\n",
            ),
            source(
                "DEVTYPE=partition\nDEVNAME=sdb1\nPARTNAME=bdsvars\n",
                "65:1\n",
            ),
            source("DEVTYPE=disk\nDEVNAME=sda\n", "8:0\n"),
        ];

        assert_eq!(
            partition_in("metadata", &sources).unwrap(),
            libc::makedev(8, 1)
        );
        assert_eq!(
            partition_in("bdsvars", &sources).unwrap(),
            libc::makedev(65, 1)
        );

        let absent = partition_in("esp", &sources).unwrap_err();
        assert_eq!(absent.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn ambiguity_non_partitions_and_malformed_numbers_fail_closed() {
        let duplicated = [
            source("DEVTYPE=partition\nPARTNAME=metadata\n", "8:1\n"),
            source("DEVTYPE=partition\nPARTNAME=metadata\n", "8:10\n"),
        ];
        assert_eq!(
            partition_in("metadata", &duplicated).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        let whole_unit = [source("DEVTYPE=disk\nPARTNAME=userdata\n", "8:0\n")];
        assert_eq!(
            partition_in("userdata", &whole_unit).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        for malformed in ["8\n", "eight:1\n", "8:one\n", "4294967296:0\n", "-8:1\n"] {
            let sources = [source("DEVTYPE=partition\nPARTNAME=metadata\n", malformed)];
            assert_eq!(
                partition_in("metadata", &sources).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{malformed}"
            );
        }
    }
}
