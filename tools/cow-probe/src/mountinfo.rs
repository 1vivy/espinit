//! `/proc/self/mountinfo` parsing, restricted to the fields the probe needs.
//!
//! The mount table is untrusted input: malformed lines are skipped rather than
//! guessed, and escaped characters are decoded before comparison.

use std::fs;
use std::path::{Path, PathBuf};

/// One parsed mountinfo line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub major: u32,
    pub minor: u32,
    pub mount_point: String,
    pub fstype: String,
    pub source: String,
}

/// Parse every well-formed line of a mountinfo table.
pub fn parse(text: &str) -> Vec<MountEntry> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 10 {
            continue;
        }
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            continue;
        };
        if separator < 5 || separator + 2 >= fields.len() {
            continue;
        }
        let Some((major, minor)) = parse_device(fields[2]) else {
            continue;
        };
        entries.push(MountEntry {
            major,
            minor,
            mount_point: unescape(fields[4]),
            fstype: fields[separator + 1].to_string(),
            source: unescape(fields[separator + 2]),
        });
    }
    entries
}

/// The `/data` mount the probe runs against. When several entries share the
/// mount point, the f2fs projection is the one under test.
pub fn data_mount(entries: &[MountEntry]) -> Option<&MountEntry> {
    entries
        .iter()
        .find(|entry| entry.mount_point == "/data" && entry.fstype == "f2fs")
        .or_else(|| entries.iter().find(|entry| entry.mount_point == "/data"))
}

/// The block device behind a mount entry: its mountinfo source when that is an
/// absolute device path, otherwise the name of `/sys/dev/block/<major>:<minor>`.
pub fn resolve_bdev(entry: &MountEntry) -> Option<PathBuf> {
    if entry.source.starts_with("/dev/") {
        return Some(PathBuf::from(&entry.source));
    }
    let link = Path::new("/sys/dev/block").join(format!("{}:{}", entry.major, entry.minor));
    let target = fs::read_link(link).ok()?;
    Some(Path::new("/dev/block").join(target.file_name()?))
}

fn parse_device(field: &str) -> Option<(u32, u32)> {
    let (major, minor) = field.split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Decode the `\ooo` octal escapes mountinfo uses for space, tab, newline and
/// backslash. Bytes are reassembled as UTF-8 afterwards.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let octal = index + 3 < bytes.len()
            && bytes[index] == b'\\'
            && bytes[index + 1..=index + 3]
                .iter()
                .all(|byte| (b'0'..=b'7').contains(byte));
        if octal {
            let value = (bytes[index + 1] - b'0') * 64
                + (bytes[index + 2] - b'0') * 8
                + (bytes[index + 3] - b'0');
            out.push(value);
            index += 4;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "\
36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
25 0 253:87 / /data rw,seclabel,noatime - f2fs /dev/block/dm-87 rw,fsync_mode=nobarrier
30 25 259:0 / /mnt/with\\040space ro - vfat /dev/block/sda17 ro
malformed line
";

    #[test]
    fn parses_entries_and_separator() {
        let entries = parse(TABLE);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].major, 98);
        assert_eq!(entries[0].minor, 0);
        assert_eq!(entries[0].mount_point, "/mnt2");
        assert_eq!(entries[0].fstype, "ext3");
        assert_eq!(entries[0].source, "/dev/root");
    }

    #[test]
    fn decodes_escaped_mount_points() {
        let entries = parse(TABLE);
        assert_eq!(entries[2].mount_point, "/mnt/with space");
    }

    #[test]
    fn selects_the_f2fs_data_mount() {
        let entries = parse(TABLE);
        let data = data_mount(&entries).expect("/data");
        assert_eq!(data.fstype, "f2fs");
        assert_eq!(resolve_bdev(data), Some(PathBuf::from("/dev/block/dm-87")));
    }

    #[test]
    fn skips_lines_without_device_or_separator() {
        assert!(parse("1 2 not-a-device / / rw - tmpfs tmpfs rw\n").is_empty());
        assert!(parse("1 2 8:0 / / rw\n").is_empty());
    }

    #[test]
    fn unescape_keeps_utf8_and_unknown_escapes() {
        assert_eq!(unescape("/data/\\040x"), "/data/ x");
        assert_eq!(unescape("/data/\\134x"), "/data/\\x");
        assert_eq!(unescape("/data/é"), "/data/é");
        assert_eq!(unescape("/data/\\9"), "/data/\\9");
    }
}
