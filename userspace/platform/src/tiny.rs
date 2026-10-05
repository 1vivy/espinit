// SPDX-License-Identifier: GPL-3.0-only
//! Fixed boot-HAL inode labelling and bind only. No command dispatch, shell,
//! policy loader, su credentials, socket, manager or configurable target paths.
use crate::{HAL, ROOT, generation::generation, open_file, open_root, staging::label};
use anyhow::{Context, Result, ensure};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

const TARGET: &str = "vendor/bin/hw/android.hardware.boot-service.qti";
const VARS: &str = "/dev/block/by-name/bdsvars";

#[cfg(target_os = "android")]
fn property(name: &std::ffi::CStr) -> Result<String> {
    unsafe extern "C" {
        fn __system_property_get(
            name: *const libc::c_char,
            value: *mut libc::c_char,
        ) -> libc::c_int;
    }
    let mut bytes = [0u8; 92];
    // SAFETY: bionic writes at most PROP_VALUE_MAX bytes; name is terminated.
    let size = unsafe { __system_property_get(name.as_ptr(), bytes.as_mut_ptr().cast()) };
    ensure!(
        size >= 0 && (size as usize) < bytes.len(),
        "invalid property length"
    );
    Ok(std::str::from_utf8(&bytes[..size as usize])?.to_owned())
}

#[cfg(not(target_os = "android"))]
fn property(_: &std::ffi::CStr) -> Result<String> {
    anyhow::bail!("tiny-espsu only runs on Android")
}

pub fn run() -> Result<()> {
    ensure!(
        std::env::args_os().len() == 1,
        "tiny-espsu accepts no arguments"
    );
    // SAFETY: these calls take no pointers and return process credentials.
    ensure!(
        unsafe { libc::getuid() == 0 && libc::geteuid() == 0 },
        "init root is required"
    );
    let selected = property(c"ro.boot.espinit.rom")?;
    ensure!(selected.len() <= 59, "ROM ID exceeds 59 bytes");
    crate::identifier(&selected)?;
    ensure!(
        crate::core::query_inherited_core_info()?.boot_mode == 1,
        "normal HAL requires PID1's Android boot selection"
    );
    ensure!(
        matches!(property(c"ro.boot.slot_suffix")?.as_str(), "_a" | "_b"),
        "invalid current slot"
    );
    let ours = fs::metadata("/proc/self/ns/mnt")?;
    let init = fs::metadata("/proc/1/ns/mnt")?;
    ensure!(
        ours.dev() == init.dev() && ours.ino() == init.ino(),
        "bind must run in init's mount namespace"
    );
    let root = open_root(Path::new(ROOT))?;
    let mut installed = String::new();
    open_file(&root, "rom.toml")?.read_to_string(&mut installed)?;
    let rom: toml::Value = toml::from_str(&installed)?;
    ensure!(
        rom.get("schema_version").and_then(toml::Value::as_integer) == Some(1)
            && rom.get("id").and_then(toml::Value::as_str) == Some(selected.as_str())
            && rom.get("generation").and_then(toml::Value::as_str) == Some(generation())
            && rom.get("managed").and_then(toml::Value::as_bool) == Some(true),
        "installed ROM does not match the selected managed ROM"
    );
    let mut hal = open_file(&root, HAL)?;
    crate::check_artifact(&mut hal, generation())?;
    let info = hal.metadata()?;
    ensure!(
        info.uid() == 0 && info.mode() & 0o7777 == 0o755,
        "HAL ownership/mode mismatch"
    );
    let system = open_root(Path::new("/"))?;
    let target = open_file(&system, TARGET)?;

    // by-name is intentionally a symlink. Resolve it once, then hold and label
    // the actual block inode with O_NOFOLLOW, never the symlink's xattr.
    let device = fs::canonicalize(VARS).context("missing projected bdsvars")?;
    ensure!(
        device.starts_with("/dev/block"),
        "bdsvars resolves outside /dev/block"
    );
    let device_parent = open_root(device.parent().context("bdsvars parent")?)?;
    let name = CString::new(
        device
            .file_name()
            .context("bdsvars name")?
            .as_encoded_bytes(),
    )?;
    // SAFETY: live parent fd/name, no creation or arbitrary device write.
    let fd = unsafe {
        libc::openat(
            device_parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    ensure!(fd >= 0, "bdsvars open: {}", std::io::Error::last_os_error());
    use std::os::fd::FromRawFd;
    // SAFETY: owns the fd returned by openat above.
    let vars = unsafe { File::from_raw_fd(fd) };
    let actual = vars.metadata()?;
    let named = fs::metadata(VARS)?;
    ensure!(
        actual.file_type().is_block_device()
            && actual.rdev() == named.rdev()
            && actual.dev() == named.dev()
            && actual.ino() == named.ino(),
        "bdsvars inode changed"
    );
    // Never relabel a stale physical by-name alias left by vendor init.
    let sysfs = fs::canonicalize(format!(
        "/sys/dev/block/{}:{}",
        libc::major(actual.rdev()),
        libc::minor(actual.rdev())
    ))?;
    ensure!(
        sysfs
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "espinit-gpt"),
        "bdsvars is not a projected partition"
    );
    ensure!(
        fs::read_to_string(sysfs.join("uevent"))?
            .lines()
            .any(|line| line == "PARTNAME=bdsvars"),
        "projected bdsvars label mismatch"
    );
    label(&vars, "u:object_r:gblbds_bdsvars_block_device:s0")?;
    label(&hal, "u:object_r:gblbds_hal_exec:s0")?;
    // Holding both inodes prevents a path replacement between validation and
    // mount. Bind only the fixed stock service target; no rc bind is needed:
    // PID1 has already generated the override read by the core init.rc hook.
    let source = CString::new(format!("/proc/self/fd/{}", hal.as_raw_fd()))?;
    let destination = CString::new(format!("/proc/self/fd/{}", target.as_raw_fd()))?;
    // SAFETY: both proc fd paths refer to live, validated regular files.
    let result = unsafe {
        libc::mount(
            source.as_ptr(),
            destination.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    ensure!(
        result == 0,
        "HAL bind failed: {}",
        std::io::Error::last_os_error()
    );
    let bound = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(format!("/{TARGET}"))?;
    let bound = bound.metadata()?;
    ensure!(
        bound.dev() == info.dev() && bound.ino() == info.ino(),
        "HAL bind inode mismatch"
    );
    Ok(())
}
