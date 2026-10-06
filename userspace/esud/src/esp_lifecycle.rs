//! Validate PID 1's retained ESP and executable staging before module execution.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]
use anyhow::{Context, Result, ensure};
use esuinit::esp::{ESP_MOUNT_POINT, EXECUTABLE_BIN, EXECUTABLE_ROOT, verify_single_esp};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const VIEW: &str = "/dev/esp";
const VFAT_LABEL: &str = "u:object_r:vfat:s0";
const EXECUTABLE_LABEL: &str = "u:object_r:esu_file:s0";

fn check_label(path: &Path, expected: &str) -> Result<()> {
    // Reuse the no-follow SELinux metadata reader used for overlay attrs.
    ensure!(
        crate::overlay::file_context(path)? == expected,
        "ESP lifecycle: unexpected label on {} (expected {expected})",
        path.display()
    );
    Ok(())
}

fn check_staging(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() || metadata.is_file(),
        "invalid staging inode"
    );
    check_label(path, EXECUTABLE_LABEL)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            check_staging(&entry?.path())?;
        }
    }
    Ok(())
}

/// Admit only the original mount and, optionally, its data-only RO bind view.
fn check_mounts(text: &str, device: (u32, u32)) -> Result<bool> {
    let mut retained = String::new();
    let mut view = false;
    let device_text = format!("{}:{}", device.0, device.1);
    for line in text.lines() {
        let (left, right) = line.split_once(" - ").context("invalid mountinfo")?;
        let left: Vec<_> = left.split_whitespace().collect();
        let right: Vec<_> = right.split_whitespace().collect();
        ensure!(
            left.len() >= 6 && right.len() == 3,
            "invalid mountinfo fields"
        );
        if left[4] == VIEW {
            ensure!(!view, "duplicate ESP module view");
            ensure!(
                left[2] == device_text && left[3] == "/" && right[0] == "vfat",
                "ESP view is not the retained filesystem"
            );
            for flag in ["ro", "nosuid", "nodev", "noexec"] {
                ensure!(
                    left[5].split(',').any(|option| option == flag),
                    "ESP view lacks {flag}"
                );
            }
            ensure!(
                !right[2].split(',').any(|option| [
                    "context=",
                    "fscontext=",
                    "rootcontext=",
                    "defcontext="
                ]
                .iter()
                .any(|key| option.starts_with(key))),
                "ESP view has a context override"
            );
            view = true;
        } else {
            retained.push_str(line);
            retained.push('\n');
        }
    }
    verify_single_esp(&retained, device).map_err(anyhow::Error::msg)?;
    Ok(view)
}

fn check_staging_mount(text: &str) -> Result<()> {
    let mut count = 0;
    for line in text.lines() {
        let (left, right) = line.split_once(" - ").context("invalid mountinfo")?;
        let left: Vec<_> = left.split_whitespace().collect();
        let right: Vec<_> = right.split_whitespace().collect();
        ensure!(
            left.len() >= 6 && right.len() == 3,
            "invalid mountinfo fields"
        );
        if left[4] == EXECUTABLE_ROOT {
            count += 1;
            ensure!(
                left[3] == "/" && right[0] == "tmpfs",
                "staging is not a whole tmpfs"
            );
            let flags: Vec<_> = left[5].split(',').collect();
            ensure!(
                flags.contains(&"nosuid") && flags.contains(&"nodev") && !flags.contains(&"noexec"),
                "staging tmpfs flags differ"
            );
        }
    }
    ensure!(count == 1, "expected exactly one executable staging tmpfs");
    Ok(())
}

/// Fail before scripts, helpers or overlays when policy or retained mounts differ.
/// The Android early service's reboot_on_failure makes a failed gate fatal to
/// boot; recovery's RC omits it and init logs the failure.
pub fn prepare() -> Result<()> {
    let text = fs::read_to_string("/proc/self/mountinfo")?;
    check_staging_mount(&text)?;
    check_staging(Path::new(EXECUTABLE_ROOT))?;
    for binary in ["esud", "busybox", "thin-activate"] {
        ensure!(
            fs::symlink_metadata(Path::new(EXECUTABLE_BIN).join(binary))?.is_file(),
            "staged {binary} missing"
        );
    }
    let marker = fs::read_to_string(Path::new(EXECUTABLE_ROOT).join("esp-device"))?;
    let (major, minor) = marker
        .trim()
        .split_once(':')
        .context("invalid ESP device marker")?;
    let device = (major.parse::<u32>()?, minor.parse::<u32>()?);
    let actual = fs::symlink_metadata(ESP_MOUNT_POINT)?.dev();
    ensure!(
        i64::from(libc::major(actual)) == i64::from(device.0)
            && i64::from(libc::minor(actual)) == i64::from(device.1),
        "retained ESP device changed"
    );
    check_label(Path::new(ESP_MOUNT_POINT), VFAT_LABEL)?;
    if !check_mounts(&text, device)? {
        fs::create_dir_all(VIEW)?;
        // SAFETY: all pointers are static NUL-terminated strings; bind mounting
        // reuses the existing superblock and never supplies new SELinux options.
        let result = unsafe {
            libc::mount(
                c"/debug_ramdisk/esp".as_ptr(),
                c"/dev/esp".as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        };
        ensure!(
            result == 0,
            "ESP bind failed: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: static target, null unused source/type/data; MS_BIND limits
        // this RO remount to the view, not the writable loop-backing superblock.
        let result = unsafe {
            libc::mount(
                std::ptr::null(),
                c"/dev/esp".as_ptr(),
                std::ptr::null(),
                libc::MS_BIND
                    | libc::MS_REMOUNT
                    | libc::MS_RDONLY
                    | libc::MS_NOSUID
                    | libc::MS_NODEV
                    | libc::MS_NOEXEC,
                std::ptr::null(),
            )
        };
        ensure!(
            result == 0,
            "ESP view RO remount failed: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            check_mounts(&fs::read_to_string("/proc/self/mountinfo")?, device)?,
            "ESP view missing after bind"
        );
    }
    check_label(Path::new(VIEW), VFAT_LABEL)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Data-only module views must not change the superblock or inherit its RW
    /// state: reject another device, duplicates and missing per-mount RO/noexec.
    #[test]
    fn view_admission_is_fail_closed() {
        let original =
            "12 1 8:1 / /debug_ramdisk/esp rw,nosuid,nodev,noexec - vfat /dev/esu/esp rw\n";
        let view = "13 1 8:1 / /dev/esp ro,nosuid,nodev,noexec - vfat /dev/esu/esp rw\n";
        assert!(!check_mounts(original, (8, 1)).unwrap());
        assert!(check_mounts(&format!("{original}{view}"), (8, 1)).unwrap());
        for invalid in [
            view.replace("8:1", "8:2"),
            view.replace("ro,nosuid", "rw,nosuid"),
            view.replace(",noexec", ""),
            format!("{view}{view}"),
            view.replace(" rw\n", " rw,context=u:object_r:esu_file:s0\n"),
        ] {
            assert!(check_mounts(&format!("{original}{invalid}"), (8, 1)).is_err());
        }
    }
}
