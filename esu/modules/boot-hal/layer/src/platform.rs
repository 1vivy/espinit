//! The Android runtime surface the OTA transaction touches outside this process.
//!
//! [`crate::txn`] is the transaction logic and is host-tested against a
//! recorder; this module is the real thing behind it: the ESP mount and its
//! bounded read-write window, the read-only loop a promoted base image keeps
//! serving through, the static `lvm`/`esud` the staging set is created with, the
//! device-mapper control node the esd tree publishes, the kernel log and the
//! denial notification.
//!
//! Every path here is a runtime layout contract shared with the loader
//! (`esuinit::esp`) and the device-name tree (`esud`), so it is derived from
//! their constants where they exist and pinned by the tests in this module
//! where they do not.

use anyhow::{Context, Result, bail, ensure};
use dm::{DeviceMapper, DeviceNumber};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::ops::Range;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, LazyLock};

/// Runtime ESP mount point. The loader mounts the payload ESP read-only here and
/// the platform hands Android modules the same path, so this is the one place
/// the transaction remounts read-write.
pub const ESP_MOUNT: &str = "/debug_ramdisk/esp";

/// The ESP payload tree (`/esu`), where the payload lives, esud writes its
/// receipts and this HAL reads the module sets from. It is
/// `esuinit::esp::payload_root` of [`ESP_MOUNT`].
pub const PAYLOAD: &str = "/debug_ramdisk/esp/esu";

/// Executable tmpfs the loader copies the payload's `bin/` into. The `lvm` and
/// `esud` this HAL spawns are there, not on the ESP.
pub const PAYLOAD_BIN: &str = "/debug_ramdisk/esu/bin";

/// Tag of every line this HAL writes to the Android log.
pub const LOG_TAG: &str = "esu-bootctl";

/// `ANDROID_LOG_ERROR` from the NDK's `android/log.h`.
#[cfg(target_os = "android")]
const ANDROID_LOG_ERROR: i32 = 6;

/// Android's notification command, used for the denial notification.
const CMD: &str = "/system/bin/cmd";

/// LVM configuration text every invocation passes with `--config`.
///
/// It is the shipped `esu/bin/lvm.conf` verbatim (the payload copies
/// `tools/lvm2/lvm.conf` byte for byte), baked in so an invocation can never
/// depend on a readable confdir; LVM requires the option *after* the command
/// word, which [`lvm`] does. `include_str!` keeps the two copies from drifting.
pub const LVM_CONF: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../tools/lvm2/lvm.conf"
));

/// LVM records its argv in a quoted metadata description without escaping
/// newlines. Keep the shipped configuration's tokens, but pass one line.
fn lvm_config() -> &'static str {
    static CONFIG: LazyLock<String> = LazyLock::new(|| {
        let mut config = String::with_capacity(LVM_CONF.len());
        for line in LVM_CONF.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if !config.is_empty() {
                config.push(' ');
            }
            config.push_str(line);
        }
        config
    });
    CONFIG.as_str()
}

/// Bytes of a physical partition read for a probe.
///
/// Only `xbl_config` is read this way, and only for its anti-rollback triple:
/// `arb::MAX_SEGMENT` bounds the program segment the scan can accept, so a
/// longer read could not change the answer.
const PHYSICAL_LIMIT: u64 = 20 * 1024 * 1024;

/// Mount flags of the ESP mount. The same base set is used for the bounded
/// read-write window, so the remount only toggles `RDONLY` and cannot widen the
/// block device's exposure.
const ESP_MOUNT_FLAGS: libc::c_ulong =
    libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_RELATIME;

/// Loop-control node used to allocate a free loop device.
const LOOP_CONTROL: &str = "/dev/loop-control";

/// Loop ioctls and status-buffer layout from `linux/loop.h`, pinned by the
/// tests below. `libc` is deliberately not asked: the values are the kernel's
/// ABI, not a libc detail.
const LOOP_SET_FD: libc::Ioctl = 0x4c00;
const LOOP_CLR_FD: libc::Ioctl = 0x4c01;
const LOOP_SET_STATUS64: libc::Ioctl = 0x4c04;
const LOOP_GET_STATUS64: libc::Ioctl = 0x4c05;
const LOOP_CTL_GET_FREE: libc::Ioctl = 0x4c82;
const LO_FLAGS_READ_ONLY: u32 = 1;
const LO_FLAGS_AUTOCLEAR: u32 = 4;
/// Size of `struct loop_info64`.
const LOOP_INFO64_SIZE: usize = 232;
/// Byte ranges of the `loop_info64` members this module writes or reads back:
/// `lo_offset`, `lo_sizelimit` and `lo_flags`. Every other member stays zero,
/// which is what the kernel expects for an unencrypted, untagged loop device.
const LOOP_INFO64_OFFSET: Range<usize> = 24..32;
const LOOP_INFO64_SIZELIMIT: Range<usize> = 32..40;
const LOOP_INFO64_FLAGS: Range<usize> = 52..56;

/// A zeroed `struct loop_info64` addressed by the fixed offsets above.
#[repr(C, align(8))]
struct LoopInfo64 {
    bytes: [u8; LOOP_INFO64_SIZE],
}

const _: () = assert!(std::mem::size_of::<LoopInfo64>() == LOOP_INFO64_SIZE);

impl LoopInfo64 {
    fn new(flags: u32) -> Self {
        let mut info = Self {
            bytes: [0; LOOP_INFO64_SIZE],
        };
        info.bytes[LOOP_INFO64_FLAGS].copy_from_slice(&flags.to_ne_bytes());
        info
    }

    fn field(&self, range: Range<usize>) -> u64 {
        u64::from_ne_bytes(self.bytes[range].try_into().expect("fixed field width"))
    }

    fn flags(&self) -> u32 {
        u32::from_ne_bytes(
            self.bytes[LOOP_INFO64_FLAGS]
                .try_into()
                .expect("fixed field width"),
        )
    }
}

/// Issue one ioctl and translate the raw error.
fn ioctl(fd: RawFd, request: libc::Ioctl, argument: usize) -> io::Result<usize> {
    // SAFETY: `request` is one of the loop ioctls above and `argument` is the
    // integer or the pointer to the correctly sized buffer the kernel expects.
    let result = unsafe { libc::ioctl(fd, request, argument) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

/// Write one line to the Android log (stderr on the host).
#[cfg(target_os = "android")]
pub fn log(message: &str) {
    use std::ffi::c_char;

    unsafe extern "C" {
        fn __android_log_print(
            priority: i32,
            tag: *const c_char,
            format: *const c_char,
            ...
        ) -> i32;
    }

    let Ok(message) = CString::new(message) else {
        return;
    };
    // SAFETY: the tag and the format are NUL-terminated constants and the single
    // `%s` conversion reads exactly the NUL-terminated message.
    unsafe {
        __android_log_print(
            ANDROID_LOG_ERROR,
            c"esu-bootctl".as_ptr(),
            c"%s".as_ptr(),
            message.as_ptr(),
        );
    }
}

/// Write one line to stderr, the host stand-in for the Android log.
#[cfg(not(target_os = "android"))]
pub fn log(message: &str) {
    eprintln!("{LOG_TAG}: {message}");
}

/// Post a denial to the notification shade through `cmd`.
///
/// The spawn runs off the serving thread and a refusal is only logged: under an
/// enforcing policy the command may not be reachable, and the ota module's
/// `boot-completed.sh` posts from the receipt in that case.
pub fn notify(reason: &str) {
    let reason = reason.to_owned();
    let spawned = std::thread::Builder::new()
        .name("esu-notify".to_owned())
        .spawn(move || {
            let result = Command::new(CMD)
                .args([
                    "notification",
                    "post",
                    "-S",
                    "bigtext",
                    "-t",
                    "esu OTA",
                    "esu.ota",
                    &reason,
                ])
                .status();
            if let Err(error) = result {
                log(&format!("notification: {error}"));
            }
        });
    if let Err(error) = spawned {
        log(&format!("notification thread: {error}"));
    }
}

/// Flush every filesystem's dirty data, as the receipt windows do.
pub fn sync() {
    // SAFETY: `sync` takes no arguments and cannot fail.
    unsafe { libc::sync() };
}

/// Flags of the remount request that toggles `RDONLY` on the existing ESP mount.
const fn remount_flags(read_only: bool) -> libc::c_ulong {
    let flags = libc::MS_REMOUNT | ESP_MOUNT_FLAGS;
    if read_only {
        flags | libc::MS_RDONLY
    } else {
        flags
    }
}

/// Toggle `RDONLY` on the ESP mount.
///
/// Without `MS_REMOUNT` a null-source request is a fresh mount that fails with
/// `EINVAL`, so the flag is not optional.
fn remount(read_only: bool) -> Result<()> {
    let target = CString::new(ESP_MOUNT).context("ESP mount point")?;
    // SAFETY: the target is a live NUL-terminated string, a remount ignores the
    // unused source and filesystem type, and no data option is supplied.
    let result = unsafe {
        libc::mount(
            std::ptr::null(),
            target.as_ptr(),
            std::ptr::null(),
            remount_flags(read_only),
            std::ptr::null(),
        )
    };
    ensure!(
        result == 0,
        "cannot remount {ESP_MOUNT} {}: {}",
        if read_only { "read-only" } else { "read-write" },
        io::Error::last_os_error()
    );
    Ok(())
}

/// Run `write` inside one bounded ESP read-write window.
///
/// The ESP is read-only during a normal boot, so every write of the transaction
/// — the staged payload, the denial receipt, the promoted base images — happens
/// between a read-write and a read-only remount of the existing mount. The
/// read-only mount is restored even when the write failed, and the failure of
/// the restore is logged rather than masked.
pub fn writable<T>(write: impl FnOnce() -> Result<T>) -> Result<T> {
    remount(false).context("open the ESP read-write window")?;
    let result = write();
    if let Err(error) = remount(true) {
        log(&format!("ESP read-only remount: {error:#}"));
    }
    sync();
    result
}

/// Create the parent directories of `path` and replace `path` with `bytes`
/// through a temporary file, one `fsync` and a rename.
///
/// The temporary file is what makes the replacement atomic on the ESP's FAT:
/// a power cut leaves either the previous file or the new one, never a
/// truncated mixture.
pub fn write_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = std::path::PathBuf::from(temporary);

    let mut file =
        File::create(&temporary).with_context(|| format!("create {}", temporary.display()))?;
    std::io::Write::write_all(&mut file, bytes)
        .with_context(|| format!("write {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", temporary.display()))?;
    drop(file);

    fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))?;
    let directory = File::open(parent).with_context(|| format!("open {}", parent.display()))?;
    directory
        .sync_all()
        .with_context(|| format!("sync {}", parent.display()))?;
    Ok(())
}

/// Remove `path`, accepting an absent file.
pub fn remove_absent_ok(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

/// Rename one ESP file onto another, both inside the caller's read-write window.
pub fn rename(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to)
        .with_context(|| format!("rename {} to {}", from.display(), to.display()))?;
    let parent = to
        .parent()
        .with_context(|| format!("{} has no directory", to.display()))?;
    File::open(parent)
        .with_context(|| format!("open {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("sync {}", parent.display()))?;
    Ok(())
}

/// Read a block device or file from its start, up to `limit` bytes.
///
/// A block device reports a zero length to `stat`, so the read runs until the
/// device or the limit ends it rather than trusting `metadata().len()`.
pub fn read_node(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut bytes = Vec::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    while u64::try_from(bytes.len()).expect("read length fits u64") < limit {
        let remaining = limit - bytes.len() as u64;
        let take = usize::try_from(remaining.min(buffer.len() as u64)).expect("chunk fits usize");
        let read = file
            .read(&mut buffer[..take])
            .with_context(|| format!("read {}", path.display()))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    ensure!(!bytes.is_empty(), "{} is empty", path.display());
    Ok(bytes)
}

/// The size limit [`read_node`] uses for a physical partition probe.
pub const fn physical_limit() -> u64 {
    PHYSICAL_LIMIT
}

/// Open the device-mapper control node.
///
/// The esd tree publishes the canonical node at `/dev/block/esd/mapper/control`
/// while the shared client opens `/dev/mapper/control`. Android's ueventd may
/// have created that path already; when it has not, the published node is
/// pointed at it once instead of mknod-ing a second control device.
pub fn mapper() -> Result<DeviceMapper> {
    let control = Path::new("/dev/mapper/control");
    if fs::symlink_metadata(control).is_err() {
        let published = Path::new(ota_core::ESD_MAPPER).join("control");
        if fs::symlink_metadata(&published).is_ok() {
            fs::create_dir_all("/dev/mapper").context("create /dev/mapper")?;
            std::os::unix::fs::symlink(&published, control).with_context(|| {
                format!("point {} at {}", control.display(), published.display())
            })?;
        }
    }
    DeviceMapper::open().map_err(anyhow::Error::msg)
}

/// Whether an active device-mapper device of this name exists.
pub fn mapper_exists(name: &str) -> bool {
    DeviceMapper::device_number(name).is_ok()
}

/// Device number of an active device-mapper device.
pub fn mapper_number(name: &str) -> Result<DeviceNumber> {
    DeviceMapper::device_number(name).map_err(anyhow::Error::msg)
}

/// Run one payload binary and require success.
fn run(program: &str, arguments: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .with_context(|| format!("run {program}"))?;
    ensure!(
        output.status.success(),
        "{program} {} failed: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Run one `lvm` command of the `rom` volume group.
///
/// The command word comes first and `--config` after it, which is the only
/// order LVM accepts for a global option on a subcommand.
pub fn lvm(arguments: &[&str]) -> Result<()> {
    let mut full = Vec::with_capacity(arguments.len() + 2);
    full.push(arguments[0]);
    full.push("--config");
    full.push(lvm_config());
    full.extend_from_slice(&arguments[1..]);
    run(&format!("{PAYLOAD_BIN}/lvm"), &full)
}

/// Every logical volume of the `rom` group with its size in bytes.
pub fn lvm_sizes() -> Result<std::collections::BTreeMap<String, u64>> {
    let output = Command::new(format!("{PAYLOAD_BIN}/lvm"))
        .args([
            "lvs",
            "--config",
            lvm_config(),
            "--noheadings",
            "--units",
            "b",
            "--nosuffix",
            "-o",
            "lv_name,lv_size",
            "rom",
        ])
        .output()
        .context("run lvm lvs")?;
    ensure!(
        output.status.success(),
        "lvm lvs failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    let mut sizes = std::collections::BTreeMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let (Some(name), Some(size)) = (fields.next(), fields.next()) else {
            continue;
        };
        sizes.insert(name.to_owned(), size.parse::<u64>().unwrap_or(0));
    }
    Ok(sizes)
}

/// Re-publish the esd device-name tree after a staging LV changed.
pub fn esd_refresh() -> Result<()> {
    run(&format!("{PAYLOAD_BIN}/esud"), &["esd", "refresh"])
}

/// An autoclear loop kept open until the switch acquires its own reference.
pub struct LoopAttachment {
    _device: File,
    /// Kernel block device to use in the switch's linear target.
    pub number: DeviceNumber,
}

fn loop_number(sysfs: &str) -> Result<DeviceNumber> {
    let (major, minor) = sysfs
        .trim()
        .split_once(':')
        .context("loop sysfs device number")?;
    let number = DeviceNumber {
        major: major.parse()?,
        minor: minor.parse()?,
    };
    ensure!(number.major == 7, "allocated loop device number mismatch");
    Ok(number)
}

fn loop_node(node: &Path, number: DeviceNumber) -> Result<File> {
    if !node.exists() {
        let name = CString::new(node.as_os_str().as_encoded_bytes())?;
        // SAFETY: name is terminated; the mode and device number are the
        // allocated block device checked against the kernel's sysfs identity.
        let result = unsafe {
            libc::mknod(
                name.as_ptr(),
                libc::S_IFBLK | 0o600,
                libc::makedev(number.major, number.minor),
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error).with_context(|| format!("create {}", node.display()));
            }
        }
    }
    let device = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(node)?;
    let metadata = device.metadata()?;
    ensure!(
        metadata.file_type().is_block_device()
            && libc::major(metadata.rdev()) as u32 == number.major
            && libc::minor(metadata.rdev()) as u32 == number.minor,
        "invalid loop node {}",
        node.display()
    );
    Ok(device)
}

/// Attach a read-only loop device to `path` and retain its open descriptor.
///
/// The promoted base image has to keep serving the letter that is already
/// running from the staging LV, and a device-mapper linear target can only
/// point at a block device, so the ESP file is attached through the standard
/// loop-control ioctls. The loop is read-only and `LO_FLAGS_AUTOCLEAR`: it
/// detaches when the switch device that references it stops opening it, so no
/// loop is leaked across boots.
pub fn attach_read_only(path: &Path) -> Result<LoopAttachment> {
    let backing = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let control = OpenOptions::new()
        .read(true)
        .write(true)
        .open(LOOP_CONTROL)
        .context("open loop-control")?;
    let number = ioctl(control.as_raw_fd(), LOOP_CTL_GET_FREE, 0).context("loop-control")?;
    ensure!(
        u32::try_from(number).is_ok(),
        "loop-control returned an unusable device number"
    );

    let index = u32::try_from(number)?;
    let identity = fs::read_to_string(format!("/sys/block/loop{index}/dev"))?;
    let number = loop_number(&identity)?;
    let node = format!("/dev/block/loop{index}");
    let device = loop_node(Path::new(&node), number).with_context(|| format!("open {node}"))?;

    ioctl(
        device.as_raw_fd(),
        LOOP_SET_FD,
        backing.as_raw_fd() as usize,
    )
    .with_context(|| format!("attach {} to {node}", path.display()))?;

    let info = LoopInfo64::new(LO_FLAGS_READ_ONLY | LO_FLAGS_AUTOCLEAR);
    if let Err(error) = ioctl(
        device.as_raw_fd(),
        LOOP_SET_STATUS64,
        std::ptr::addr_of!(info) as usize,
    ) {
        let _ = ioctl(device.as_raw_fd(), LOOP_CLR_FD, 0);
        return Err(error).with_context(|| format!("set loop status on {node}"));
    }

    let mut applied = LoopInfo64::new(0);
    if let Err(error) = ioctl(
        device.as_raw_fd(),
        LOOP_GET_STATUS64,
        std::ptr::addr_of_mut!(applied) as usize,
    ) {
        let _ = ioctl(device.as_raw_fd(), LOOP_CLR_FD, 0);
        return Err(error).with_context(|| format!("read loop status of {node}"));
    }
    let read_only = applied.flags() & LO_FLAGS_READ_ONLY != 0;
    if applied.field(LOOP_INFO64_OFFSET) != 0
        || applied.field(LOOP_INFO64_SIZELIMIT) != 0
        || !read_only
    {
        let _ = ioctl(device.as_raw_fd(), LOOP_CLR_FD, 0);
        bail!("{node} is not the read-only, whole-file loop this ROM needs");
    }

    Ok(LoopAttachment {
        _device: device,
        number,
    })
}

/// Whether the esd tree exists, so a failure can name the missing tree.
pub fn esd_tree() -> &'static str {
    ota_core::ESD_ROOT
}

/// The payload path of one ROM's staged takeover archive, ESP-relative.
pub fn stage_payload(id: &str) -> std::path::PathBuf {
    Path::new(ESP_MOUNT).join(ota_core::stage_payload(id))
}

/// The payload path of one ROM's committed takeover archive, ESP-relative.
pub fn committed_payload(id: &str) -> std::path::PathBuf {
    Path::new(ESP_MOUNT).join(ota_core::committed_payload(id))
}

/// The payload path of one ROM's base image, ESP-relative.
pub fn base_image(id: &str, base: &str) -> std::path::PathBuf {
    Path::new(ESP_MOUNT).join(esu_config::base_image_path(id, base))
}

/// Shared handle of the platform, as [`crate::txn::Env`] consumers hold it.
pub type Shared = Arc<dyn crate::txn::Env>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runtime_layout_matches_the_loader() {
        // `esuinit::esp::payload_root` and `EXECUTABLE_BIN` are the loader's
        // constants for the same paths; a change there must change this module.
        assert_eq!(PAYLOAD, format!("{ESP_MOUNT}/esu"));
        assert_eq!(PAYLOAD_BIN, "/debug_ramdisk/esu/bin");
        assert_eq!(esd_tree(), "/dev/block/esd");
    }

    #[test]
    fn loop_identity_and_attachment_lifetime_are_checked() {
        let number = loop_number("7:89\n").unwrap();
        assert_eq!((number.major, number.minor), (7, 89));
        for identity in ["8:89", "7", "7:89:1", "-1:89"] {
            assert!(loop_number(identity).is_err());
        }
        let partitioned = loop_number("7:712\n").unwrap();
        assert_eq!(partitioned.minor, 712);
        assert!(loop_node(Path::new("/dev/null"), number).is_err());
        let file = File::open("/dev/null").unwrap();
        let fd = file.as_raw_fd();
        let attachment = LoopAttachment {
            _device: file,
            number,
        };
        assert_eq!(attachment.number.minor, 89);
        // SAFETY: F_GETFD has no pointer argument and observes this owned fd.
        assert!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0);
        drop(attachment);
        // SAFETY: F_GETFD takes no pointer and safely rejects the closed fd.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
    }

    #[test]
    fn the_remount_only_toggles_read_only() {
        assert_ne!(remount_flags(false) & libc::MS_REMOUNT, 0);
        for read_only in [false, true] {
            let flags = remount_flags(read_only);
            assert_eq!(flags & libc::MS_RDONLY != 0, read_only);
            assert_eq!(
                flags & !(libc::MS_REMOUNT | libc::MS_RDONLY),
                ESP_MOUNT_FLAGS
            );
        }
    }

    #[test]
    fn the_loop_status_buffer_matches_the_kernel_layout() {
        assert_eq!(std::mem::size_of::<LoopInfo64>(), LOOP_INFO64_SIZE);
        assert_eq!(LOOP_INFO64_OFFSET, 24..32);
        assert_eq!(LOOP_INFO64_SIZELIMIT, 32..40);
        assert_eq!(LOOP_INFO64_FLAGS, 52..56);

        let info = LoopInfo64::new(LO_FLAGS_READ_ONLY | LO_FLAGS_AUTOCLEAR);
        assert_eq!(info.flags(), 5);
        assert_eq!(info.field(LOOP_INFO64_OFFSET), 0);
        assert_eq!(info.field(LOOP_INFO64_SIZELIMIT), 0);
        assert!(
            info.bytes[..LOOP_INFO64_OFFSET.start]
                .iter()
                .all(|byte| *byte == 0),
            "an unencrypted loop keeps every earlier member zero"
        );
    }

    #[test]
    fn the_lvm_configuration_is_the_shipped_file() {
        assert!(LVM_CONF.contains("dir = \"/dev/block/esd\""));
        assert!(LVM_CONF.contains("udev_sync = 0"));
        assert!(LVM_CONF.contains("use_lvmlockd = 0"));
        assert!(LVM_CONF.ends_with("}\n"));
    }

    #[test]
    fn the_lvm_argument_cannot_inject_multiline_metadata_descriptions() {
        let config = lvm_config();
        assert!(!config.contains(['\n', '\r', '#']));
        assert_eq!(
            config,
            "devices { use_devicesfile = 0 dir = \"/dev/block/esd\" \
             scan = [\"/dev/block/esd/pv\"] \
             filter = [\"a|^/dev/block/esd/pv/a$|\",\"r|.*|\"] } \
             activation { udev_rules = 0 udev_sync = 0 \
             verify_udev_operations = 0 monitoring = 0 } \
             global { use_lvmlockd = 0 }"
        );
        assert!(std::ptr::eq(config.as_ptr(), lvm_config().as_ptr()));
    }

    #[test]
    fn the_esp_payload_paths_are_rom_relative() {
        assert_eq!(
            stage_payload("rom2"),
            Path::new("/debug_ramdisk/esp/rom/rom2/esu.stage.cpio")
        );
        assert_eq!(
            committed_payload("rom2"),
            Path::new("/debug_ramdisk/esp/rom/rom2/esu.cpio")
        );
        assert_eq!(
            base_image("rom2", "boot"),
            Path::new("/debug_ramdisk/esp/rom/rom2/boot.img")
        );
    }
}
