//! Lab-only boot watchdog.
//!
//! `esud watchdog <seconds>` waits for Android's own `sys.boot_completed`. A
//! boot that never reaches it is not observable from outside the phone, so the
//! deadline is a controlled exit: the kernel-log tail becomes an ESP receipt,
//! the misc BCB carries a `bootonce-bootloader` command with the cause, and the
//! device restarts through the reboot syscall directly. init may already be
//! blocked in `mount_all` when the deadline fires, so `sys.powerctl` cannot be
//! used to reach this result.
//!
//! Every step after the deadline is best effort and runs once: the restart is
//! the point of the deadline, so no step may turn it into an unbounded wait.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

/// Kernel-log bytes kept for the deadline receipt.
pub const RECEIPT_BYTES: usize = 64 * 1024;

/// BCB status cause of the watchdog deadline.
pub const CAUSE: &str = "esu:watchdog:boot_completed";

/// ESP file receiving the kernel-log tail, below the ESP `esu` subtree.
const RECEIPT_NAME: &str = "watchdog.txt";

/// One kernel-log record read.
const RECORD_BYTES: usize = 8192;

/// Poll interval while awaiting boot completion.
const POLL: Duration = Duration::from_secs(1);

/// Kernel log device read for the receipt.
const KMSG: &str = "/dev/kmsg";

/// Android's own boot-completion property.
const BOOT_COMPLETED: &str = "sys.boot_completed";

/// Android's stable by-name link to the misc partition.
const BY_NAME_MISC: &str = "/dev/block/by-name/misc";

/// sysfs `PARTNAME` of the misc partition.
const MISC_PARTITION: &str = "misc";

/// Restart attempts before the watchdog gives up on the syscall.
const RESTART_ATTEMPTS: usize = 3;

/// Pause between restart attempts.
const RESTART_PAUSE: Duration = Duration::from_secs(1);

/// ESP mount flags with `RDONLY` cleared, mirroring
/// `esuinit::receipt::ESP_MOUNT_FLAGS_RW`. This crate and esuinit resolve
/// different rustix sources, so the libc values are written out and pinned by
/// `esp_remount_flags_match_the_esuinit_set`.
const ESP_MOUNT_FLAGS_RW: libc::c_ulong =
    libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_RELATIME;

/// Wait for Android's boot completion or the deadline, then expire.
#[cfg(target_os = "android")]
pub fn run(seconds: u64) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    if wait_for_boot(boot_completed, deadline) {
        note("sys.boot_completed=1; the deadline is cancelled");
        return Ok(());
    }

    expire()
}

/// Poll `completed` once per second and report whether boot completed first.
fn wait_for_boot(mut completed: impl FnMut() -> bool, deadline: Instant) -> bool {
    loop {
        if completed() {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(POLL.min(remaining));
    }
}

/// Leave the evidence and restart: the kernel-log receipt, the one-shot
/// bootloader request, then the restart itself.
#[cfg(target_os = "android")]
fn expire() -> anyhow::Result<()> {
    note("deadline reached; Android did not report sys.boot_completed");
    capture_receipt();
    request_bootloader();
    restart()
}

/// Best-effort kernel-log tail on the ESP, inside one bounded RW remount.
#[cfg(target_os = "android")]
fn capture_receipt() {
    let mount = esuinit::esp::ESP_MOUNT_POINT;
    let receipts = Path::new(mount).join("esu/receipts");
    let receipt = receipts.join(RECEIPT_NAME);

    if !crate::overlay::mounted(mount, "vfat").unwrap_or(false) {
        note(&format!(
            "ESP {mount} is not mounted; the kernel log was not persisted"
        ));
        return;
    }

    note(&format!(
        "capturing the kernel log for {}",
        receipt.display()
    ));
    let tail = match kernel_log_tail(Path::new(KMSG)) {
        Ok(tail) => tail,
        Err(error) => {
            note(&format!("cannot read the kernel log: {error}"));
            return;
        }
    };

    let written = remount(mount, true).and_then(|()| store_receipt(&receipts, &tail));
    let restored = remount(mount, false);
    rustix::fs::sync();

    match written {
        Ok(()) => note(&format!(
            "wrote {} bytes of the kernel log to {}",
            tail.len(),
            receipt.display()
        )),
        Err(error) => note(&format!("cannot persist the kernel log: {error}")),
    }
    if let Err(error) = restored {
        note(&format!("cannot restore the read-only ESP mount: {error}"));
    }
}

/// Best-effort one-shot bootloader request with the watchdog cause.
#[cfg(target_os = "android")]
fn request_bootloader() {
    let misc = match misc_device() {
        Ok(misc) => misc,
        Err(detail) => {
            note(&format!("cannot record the bootloader command: {detail}"));
            return;
        }
    };

    match esu_platform::bcb::request_bootloader(&misc, CAUSE) {
        Ok(()) => note(&format!("recorded {CAUSE} in {}", misc.display())),
        Err(error) => note(&format!(
            "cannot write the bootloader command to {}: {error}",
            misc.display()
        )),
    }
}

/// Restart through the reboot syscall. A successful call does not return.
#[cfg(target_os = "android")]
fn restart() -> anyhow::Result<()> {
    for attempt in 1..=RESTART_ATTEMPTS {
        note(&format!(
            "restarting through the reboot syscall ({attempt})"
        ));

        rustix::fs::sync();
        // A successful restart does not return; any return is a failure. The
        // syscall is issued directly because init may be blocked in `mount_all`
        // and cannot process a `sys.powerctl` request, and because bionic does
        // not expose `reboot(2)` as a library call.
        // SAFETY: the syscall takes four integer arguments and no pointer.
        let result = unsafe {
            libc::syscall(
                libc::SYS_reboot,
                libc::c_long::from(libc::LINUX_REBOOT_MAGIC1),
                libc::c_long::from(libc::LINUX_REBOOT_MAGIC2),
                libc::c_long::from(libc::LINUX_REBOOT_CMD_RESTART),
                0 as libc::c_long,
            )
        };
        if result == 0 {
            note("the reboot syscall returned without restarting");
        } else {
            note(&format!(
                "cannot restart the device: {}",
                io::Error::last_os_error()
            ));
        }

        if attempt < RESTART_ATTEMPTS {
            std::thread::sleep(RESTART_PAUSE);
        }
    }

    anyhow::bail!("the watchdog deadline could not restart the device")
}

/// Resolve the misc partition: Android's by-name link first, then the sysfs
/// `PARTNAME` node, which also works before init publishes the links.
#[cfg(target_os = "android")]
fn misc_device() -> Result<std::path::PathBuf, String> {
    let linked = Path::new(BY_NAME_MISC);
    if is_block_device(linked) {
        return Ok(linked.to_owned());
    }

    esuinit::esp::partition_node(MISC_PARTITION).map(std::path::PathBuf::from)
}

/// Android's own boot-completion property.
#[cfg(target_os = "android")]
fn boot_completed() -> bool {
    crate::utils::getprop(BOOT_COMPLETED).is_some_and(|value| value.trim() == "1")
}

/// Report one step on the kernel log with the watchdog prefix.
fn note(message: &str) {
    log::info!("esu watchdog: {message}");

    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open(KMSG) {
        let _ = writeln!(kmsg, "<6>esu watchdog: {message}");
    }
}

/// Toggle `RDONLY` on the ESP mount for the single write window.
fn remount(mount: &str, read_only: bool) -> io::Result<()> {
    let mut flags = ESP_MOUNT_FLAGS_RW;
    if read_only {
        flags |= libc::MS_RDONLY;
    }

    let target = std::ffi::CString::new(mount).map_err(io::Error::other)?;

    // SAFETY: the target is a live NUL-terminated string, a remount ignores the
    // unused source and filesystem type, and no data option is supplied.
    let result = unsafe {
        libc::mount(
            std::ptr::null(),
            target.as_ptr(),
            std::ptr::null(),
            flags,
            std::ptr::null(),
        )
    };
    if result == 0 {
        return Ok(());
    }

    Err(io::Error::last_os_error())
}

/// Create the receipts directory and replace the receipt with `bytes`.
fn store_receipt(receipts: &Path, bytes: &[u8]) -> io::Result<()> {
    std::fs::create_dir_all(receipts)?;

    let mut file = std::fs::File::create(receipts.join(RECEIPT_NAME))?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Read the kernel log and keep only its last [`RECEIPT_BYTES`].
fn kernel_log_tail(device: &Path) -> io::Result<Vec<u8>> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(device)?;

    retain_tail(|record| file.read(record), RECEIPT_BYTES)
}

/// Read every available record and retain only the last `limit` bytes.
///
/// The kernel log device is read non-blocking, so the loop ends with the last
/// buffered record instead of waiting for the next one, and the retained tail
/// stays bounded however long the ring is.
fn retain_tail(
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
    limit: usize,
) -> io::Result<Vec<u8>> {
    let mut record = vec![0u8; RECORD_BYTES];
    let mut tail = Vec::new();

    loop {
        match read(&mut record) {
            Ok(0) => break,
            Ok(length) => {
                tail.extend_from_slice(&record[..length]);
                if tail.len() > limit {
                    tail.drain(..tail.len() - limit);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => return Err(error),
        }
    }

    Ok(tail)
}

/// Whether `path` resolves to an existing block device node.
///
/// The by-name link is a symlink to the node, so the lookup follows it; an
/// absent path, a regular file or a directory is not a usable device.
fn is_block_device(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_block_device())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temporary(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "esu-watchdog-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// The remount set must stay the ro/nosuid/nodev/noexec ESP contract that
    /// esuinit's receipt window uses, with only `RDONLY` toggled.
    #[test]
    fn esp_remount_flags_match_the_esuinit_set() {
        assert_eq!(
            ESP_MOUNT_FLAGS_RW,
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_RELATIME
        );
        assert_eq!(ESP_MOUNT_FLAGS_RW & libc::MS_RDONLY, 0);

        let read_only = ESP_MOUNT_FLAGS_RW | libc::MS_RDONLY;
        assert_ne!(read_only, ESP_MOUNT_FLAGS_RW);
        assert_eq!(read_only & !libc::MS_RDONLY, ESP_MOUNT_FLAGS_RW);
    }

    /// Boot completion cancels the deadline without touching the device.
    #[test]
    fn boot_completion_cancels_the_deadline() {
        let mut polls = 0;
        let completed = wait_for_boot(
            || {
                polls += 1;
                polls == 3
            },
            Instant::now() + Duration::from_secs(30),
        );
        assert!(completed);
        assert_eq!(polls, 3);
    }

    /// An already expired window polls once and reports the deadline.
    #[test]
    fn expired_deadline_is_reported() {
        let mut polls = 0;
        let completed = wait_for_boot(
            || {
                polls += 1;
                false
            },
            Instant::now(),
        );
        assert!(!completed);
        assert_eq!(polls, 1);
    }

    /// The receipt keeps only the tail, whatever the ring length, and stops at
    /// the end of the buffered records.
    #[test]
    fn the_receipt_retains_the_last_bytes_of_the_ring() {
        let payload: Vec<u8> = (0..40_000u32).map(|value| value as u8).collect();
        let mut offset = 0;
        let tail = retain_tail(
            |record| {
                let length = record.len().min(payload.len() - offset);
                record[..length].copy_from_slice(&payload[offset..offset + length]);
                offset += length;
                Ok(length)
            },
            1024,
        )
        .unwrap();
        assert_eq!(tail, payload[payload.len() - 1024..]);

        let mut writes = 0;
        let tail = retain_tail(
            |_| {
                writes += 1;
                if writes < 3 {
                    Ok(RECORD_BYTES)
                } else {
                    Err(io::ErrorKind::WouldBlock.into())
                }
            },
            4096,
        )
        .unwrap();
        assert_eq!(tail.len(), 4096);
        assert_eq!(writes, 3);

        let empty: Vec<u8> = retain_tail(|_| Ok(0), 4096).unwrap();
        assert_eq!(empty, Vec::<u8>::new());
    }

    /// A by-name link is a symlink, so a regular file, an absent path and a
    /// link to a regular file are all rejected; a link to a real block device
    /// is accepted when the host has one to point at.
    #[test]
    fn the_by_name_link_must_resolve_to_a_block_device() {
        let file = temporary("regular");
        std::fs::write(&file, b"x").unwrap();
        assert!(!is_block_device(&file));

        let link = temporary("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(!is_block_device(&link));

        std::fs::remove_file(&link).unwrap();
        std::fs::remove_file(&file).unwrap();
        assert!(!is_block_device(Path::new("/dev/esu/absent")));

        for device in ["/dev/loop0", "/dev/nvme0n1", "/dev/sda"] {
            let device = Path::new(device);
            if !device.exists() {
                continue;
            }
            std::os::unix::fs::symlink(device, &link).unwrap();
            assert!(is_block_device(&link), "{}", device.display());
            std::fs::remove_file(&link).unwrap();
            break;
        }
    }

    /// The receipt replaces its file and creates the receipts directory.
    #[test]
    fn the_receipt_file_is_replaced_in_place() {
        let root = temporary("receipts");
        let receipts = root.join("esu/receipts");
        store_receipt(&receipts, b"first\n").unwrap();
        store_receipt(&receipts, b"second\n").unwrap();
        assert_eq!(
            std::fs::read(receipts.join(RECEIPT_NAME)).unwrap(),
            b"second\n"
        );
        assert!(std::fs::symlink_metadata(receipts).unwrap().is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }
}
