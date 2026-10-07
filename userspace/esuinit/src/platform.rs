//! Publish boot mode and ESP module init RC before real-init handoff.
use crate::receipt::{Failure, Stage};
use anyhow::{Result, ensure};
use std::fs;
use std::io::Read;
use std::path::Path;

pub(crate) fn select_core_boot_mode() -> Result<(), Failure> {
    let mode = if crate::scripts::is_recovery() { 2 } else { 1 };
    let result = (|| -> Result<()> {
        crate::set_core_boot_mode(mode)?;
        ensure!(
            crate::query_core_info()?.boot_mode == mode,
            "core boot mode readback mismatch"
        );
        Ok(())
    })();
    result.map_err(|error| {
        Failure::new(
            Stage::ModuleCheck,
            "PlatformBootModeFailed",
            format!("{error:#}"),
        )
    })
}

pub const MAX_MODULE_RC: usize = 65536;

/// Validate every listed ESP module, including modules excluded in recovery.
pub fn validate_modules(payload: &Path, order: &[String]) -> Result<(), Failure> {
    let root = esu_platform::open_root(payload).map_err(rc_error)?;
    for id in order {
        let relative = format!("modules/{id}/module.prop");
        let file = esu_platform::open_file(&root, &relative).map_err(rc_error)?;
        let mut text = String::new();
        file.take(65537)
            .read_to_string(&mut text)
            .map_err(rc_error)?;
        let mut ids = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .filter(|(key, _)| *key == "id")
            .map(|(_, value)| value);
        if text.len() > 65536 || ids.next() != Some(id.as_str()) || ids.next().is_some() {
            return Err(Failure::new(
                Stage::Configuration,
                "ModuleIdMismatch",
                relative,
            ));
        }
    }
    Ok(())
}

fn rc_error(error: impl std::fmt::Display) -> Failure {
    Failure::new(
        Stage::Configuration,
        "ModuleRcUnreadable",
        error.to_string(),
    )
}

pub fn recovery_allowed(directory: &Path) -> Result<bool, Failure> {
    match fs::symlink_metadata(directory.join("recovery-ok")) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(rc_error("recovery-ok is not a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(rc_error(error)),
    }
}

/// Preserve manifest precedence and lexical filename order; never truncate RC.
pub fn module_rc(payload: &Path, order: &[String], recovery: bool) -> Result<Vec<u8>, Failure> {
    let root = esu_platform::open_root(payload).map_err(rc_error)?;
    let mut output = Vec::new();
    for id in order {
        let directory = payload.join("modules").join(id);
        if recovery && !recovery_allowed(&directory)? {
            continue;
        }
        let rc_dir = directory.join("initrc");
        match fs::symlink_metadata(&rc_dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(rc_error(error)),
            Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
                return Err(rc_error("initrc is not a directory"));
            }
            Ok(_) => (),
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(&rc_dir).map_err(rc_error)? {
            let entry = entry.map_err(rc_error)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| rc_error("non-UTF8 initrc filename"))?;
            if name.ends_with(".rc") {
                files.push(name);
            }
        }
        files.sort();
        for name in files {
            let relative = format!("modules/{id}/initrc/{name}");
            let file = esu_platform::open_file(&root, &relative).map_err(rc_error)?;
            let header = format!("# === {id}/initrc/{name} ===\n");
            let remaining = MAX_MODULE_RC.saturating_sub(output.len());
            if header.len() > remaining {
                return Err(too_large());
            }
            output.extend_from_slice(header.as_bytes());
            let start = output.len();
            file.take((remaining - header.len() + 1) as u64)
                .read_to_end(&mut output)
                .map_err(rc_error)?;
            if output.len() > MAX_MODULE_RC {
                return Err(too_large());
            }
            let contents = &output[start..];
            let text = std::str::from_utf8(contents).map_err(rc_error)?;
            if text.contains('\0') {
                return Err(rc_error("NUL in module RC"));
            }
            // Keep the next header on its own line, including for files without LF.
            if !contents.is_empty() && !contents.ends_with(b"\n") {
                if output.len() == MAX_MODULE_RC {
                    return Err(too_large());
                }
                output.push(b'\n');
            }
        }
    }
    Ok(output)
}

fn too_large() -> Failure {
    Failure::new(
        Stage::Configuration,
        "ModuleRcTooLarge",
        "module RC exceeds 65536 bytes",
    )
}

/// Recreate the ESP and executable tmpfs after stock init's root switch.
/// Match the loop-pinned superblock's access mode; never add SELinux mount
/// options. This action precedes module early-init actions and the core's
/// on-init relabel/esu-early service.
fn bootstrap_rc(device: (u32, u32), writable: bool) -> String {
    let (major, minor) = device;
    let access = if writable { "rw" } else { "ro" };
    format!(
        "\non early-init\n\
         \x20   exec u:r:esu:s0 root -- /system/bin/toybox mknod /dev/esu-esp b {major} {minor}\n\
         \x20   mkdir /debug_ramdisk/esp 0700 root root\n\
         \x20   mount vfat /dev/esu-esp /debug_ramdisk/esp {access} nosuid nodev noexec\n\
         \x20   mkdir /debug_ramdisk/esu 0755 root root\n\
         \x20   mount tmpfs esu /debug_ramdisk/esu nosuid nodev mode=0755\n\
         \x20   exec u:r:esu:s0 root -- /system/bin/toybox cp -R /debug_ramdisk/esp/esu/bin /debug_ramdisk/esu/bin\n\
         \x20   exec u:r:esu:s0 root -- /system/bin/toybox chmod -R 0755 /debug_ramdisk/esu/bin\n\
         \x20   write /debug_ramdisk/esu/esp-device {major}:{minor}\n\n"
    )
}

pub fn publish_module_rc(
    payload: &Path,
    order: &[String],
    device: (u32, u32),
    writable: bool,
) -> Result<(), Failure> {
    let modules = module_rc(payload, order, crate::scripts::is_recovery())?;
    let mut rc = bootstrap_rc(device, writable).into_bytes();
    if rc.len() + modules.len() > MAX_MODULE_RC {
        return Err(too_large());
    }
    rc.extend_from_slice(&modules);
    crate::set_module_rc(&rc).map_err(|error| {
        Failure::new(
            Stage::ModuleCheck,
            "ModuleRcIoctlFailed",
            format!("{error:#}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "esu-rc-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn rc(&self, id: &str, name: &str, bytes: &[u8]) {
            let directory = self.0.join("modules").join(id).join("initrc");
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join(name), bytes).unwrap();
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[test]
    fn exact_headers_manifest_order_and_sorted_files() {
        let temp = Temp::new();
        temp.rc("b", "z.rc", b"z\n");
        temp.rc("b", "a.rc", b"a\n");
        temp.rc("a", "x.rc", b"x\n");
        assert_eq!(
            module_rc(&temp.0, &["b".into(), "a".into()], false).unwrap(),
            b"# === b/initrc/a.rc ===\na\n# === b/initrc/z.rc ===\nz\n# === a/initrc/x.rc ===\nx\n"
        );
    }
    #[test]
    fn exact_limit_and_overflow_are_fail_closed() {
        let temp = Temp::new();
        let header = b"# === a/initrc/a.rc ===\n";
        temp.rc("a", "a.rc", &vec![b'\n'; MAX_MODULE_RC - header.len()]);
        assert_eq!(
            module_rc(&temp.0, &["a".into()], false).unwrap().len(),
            MAX_MODULE_RC
        );
        temp.rc("a", "a.rc", &vec![b'\n'; MAX_MODULE_RC - header.len() + 1]);
        assert_eq!(
            module_rc(&temp.0, &["a".into()], false).unwrap_err().error,
            "ModuleRcTooLarge"
        );
    }
    #[test]
    fn recovery_marker_filters_and_malformed_rc_fails() {
        let temp = Temp::new();
        temp.rc("a", "a.rc", b"a\n");
        temp.rc("b", "b.rc", b"b\n");
        fs::write(temp.0.join("modules/b/recovery-ok"), b"").unwrap();
        assert_eq!(
            module_rc(&temp.0, &["a".into(), "b".into()], true).unwrap(),
            b"# === b/initrc/b.rc ===\nb\n"
        );
        temp.rc("b", "b.rc", b"\0");
        assert_eq!(
            module_rc(&temp.0, &["b".into()], false).unwrap_err().error,
            "ModuleRcUnreadable"
        );
        fs::remove_file(temp.0.join("modules/b/initrc/b.rc")).unwrap();
        std::os::unix::fs::symlink("missing", temp.0.join("modules/b/initrc/b.rc")).unwrap();
        assert!(module_rc(&temp.0, &["b".into()], false).is_err());
        assert!(module_rc(&temp.0, &[], false).unwrap().is_empty());
    }
}
