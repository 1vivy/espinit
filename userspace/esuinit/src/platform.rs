//! Publish boot mode and ESP module init RC before real-init handoff.
//!
//! Every ESP module is optional unless it carries an empty `critical` marker,
//! and `disable`/`remove` skip a module outright. An optional module that
//! cannot be validated, read, or initialized is logged and skipped while the
//! remaining modules still run; a critical one rejects the handoff, and the
//! final required-backend and projection validation rejects an unusable
//! configuration either way.

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

/// Empty marker that admits a module in recovery.
pub const RECOVERY_OK_MARKER: &str = "recovery-ok";

/// Empty markers that skip a module in every boot mode.
pub const DISABLE_MARKER: &str = "disable";
pub const REMOVE_MARKER: &str = "remove";

/// Empty marker that makes an admitted module's failure reject the handoff.
pub const CRITICAL_MARKER: &str = "critical";

/// Admission and escalation policy of one ESP module.
///
/// Every marker is an admission filter: presence decides the disposition, and
/// a module a filter removed is never escalated, because a deliberate skip is
/// caught by the final required-backend and projection validation instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModulePolicy {
    /// Admitted for this boot; failures reject the handoff when critical.
    Admitted { critical: bool },
    /// Deliberately skipped by a module marker or the recovery filter.
    Skipped,
}

/// Whether one marker file is present.
///
/// Markers are empty regular files. A marker that exists but is not one is
/// ignored with a warning, so a malformed admission marker can never turn a
/// module's optional work into a boot stop.
fn marker(directory: &Path, name: &str) -> Result<bool, Failure> {
    match fs::symlink_metadata(directory.join(name)) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => {
            log::warn!("Ignoring non-regular module marker {name}");
            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(rc_error(error)),
    }
}

/// Read one module's admission and escalation policy.
///
/// `disable` and `remove` skip the module first, the recovery filter then
/// admits only `recovery-ok` modules, and only an admitted module's `critical`
/// marker decides whether its failure rejects the handoff. A missing module
/// directory carries no marker and stays optional.
pub fn module_policy(directory: &Path, recovery: bool) -> Result<ModulePolicy, Failure> {
    if marker(directory, DISABLE_MARKER)? || marker(directory, REMOVE_MARKER)? {
        return Ok(ModulePolicy::Skipped);
    }
    if recovery && !marker(directory, RECOVERY_OK_MARKER)? {
        return Ok(ModulePolicy::Skipped);
    }
    Ok(ModulePolicy::Admitted {
        critical: is_critical(directory)?,
    })
}

/// Whether one module carries the `critical` escalation marker.
pub fn is_critical(directory: &Path) -> Result<bool, Failure> {
    marker(directory, CRITICAL_MARKER)
}

/// Retain admitted modules with valid identities in manifest order.
///
/// Removing rejected optional entries from the in-memory order prevents their
/// scripts and init RC from running later. Critical errors reject handoff;
/// required backend and projection validation remain independent.
pub fn validate_modules(
    payload: &Path,
    order: &mut Vec<String>,
    recovery: bool,
) -> Result<(), Failure> {
    let root = esu_platform::open_root(payload).map_err(rc_error)?;
    let mut failure = None;
    order.retain(|id| {
        if failure.is_some() {
            return false;
        }
        let directory = payload.join("modules").join(id);
        let critical = match module_policy(&directory, recovery) {
            Ok(ModulePolicy::Admitted { critical }) => critical,
            Ok(ModulePolicy::Skipped) => return false,
            Err(error) => {
                failure = Some(error);
                return false;
            }
        };
        match validate_module_prop(&root, id) {
            Ok(()) => true,
            Err(error) => {
                if critical {
                    failure = Some(error);
                } else {
                    log::warn!(
                        "Skipping optional module {id}: {}: {}",
                        error.error,
                        error.detail
                    );
                }
                false
            }
        }
    });
    failure.map_or(Ok(()), Err)
}

/// Validate one module's `module.prop` identity, bounded at 64 KiB.
fn validate_module_prop(root: &fs::File, id: &str) -> Result<(), Failure> {
    let relative = format!("modules/{id}/module.prop");
    let file = esu_platform::open_file(root, &relative).map_err(rc_error)?;
    let mut text = String::new();
    file.take(65537)
        .read_to_string(&mut text)
        .map_err(rc_error)?;
    let mut ids = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| *key == "id")
        .map(|(_, value)| value);
    if text.len() > 65536 || ids.next() != Some(id) || ids.next().is_some() {
        return Err(Failure::new(
            Stage::Configuration,
            "ModuleIdMismatch",
            relative,
        ));
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

/// Preserve manifest precedence and lexical filename order; never truncate RC.
///
/// An optional module whose fragments cannot be read whole is logged and
/// skipped without contributing a byte, while a critical module rejects the
/// handoff.
pub fn module_rc(payload: &Path, order: &[String], recovery: bool) -> Result<Vec<u8>, Failure> {
    module_rc_with_limit(payload, order, recovery, MAX_MODULE_RC)
}

fn module_rc_with_limit(
    payload: &Path,
    order: &[String],
    recovery: bool,
    limit: usize,
) -> Result<Vec<u8>, Failure> {
    let root = esu_platform::open_root(payload).map_err(rc_error)?;
    let mut output = Vec::new();
    for id in order {
        let directory = payload.join("modules").join(id);
        let ModulePolicy::Admitted { critical } = module_policy(&directory, recovery)? else {
            continue;
        };
        let remaining = limit.saturating_sub(output.len());
        match module_rc_fragment(&root, &directory, id, remaining) {
            Ok(fragment) => output.extend_from_slice(&fragment),
            Err(failure) if critical => return Err(failure),
            Err(failure) => log::warn!(
                "Skipping optional module {id} init RC: {}: {}",
                failure.error,
                failure.detail
            ),
        }
    }
    Ok(output)
}

/// Build one module's complete init RC fragment and contribute nothing on a
/// failure, so a bad module can never publish a partial fragment.
fn module_rc_fragment(
    root: &fs::File,
    directory: &Path,
    id: &str,
    limit: usize,
) -> Result<Vec<u8>, Failure> {
    let rc_dir = directory.join("initrc");
    match fs::symlink_metadata(&rc_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
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
    let mut fragment = Vec::new();
    for name in files {
        let relative = format!("modules/{id}/initrc/{name}");
        let file = esu_platform::open_file(root, &relative).map_err(rc_error)?;
        let header = format!("# === {id}/initrc/{name} ===\n");
        let remaining = limit.saturating_sub(fragment.len());
        if header.len() > remaining {
            return Err(too_large());
        }
        fragment.extend_from_slice(header.as_bytes());
        let start = fragment.len();
        file.take((remaining - header.len() + 1) as u64)
            .read_to_end(&mut fragment)
            .map_err(rc_error)?;
        if fragment.len() > limit {
            return Err(too_large());
        }
        let contents = &fragment[start..];
        let text = std::str::from_utf8(contents).map_err(rc_error)?;
        if text.contains('\0') {
            return Err(rc_error("NUL in module RC"));
        }
        if !contents.is_empty() && !contents.ends_with(b"\n") {
            if fragment.len() == limit {
                return Err(too_large());
            }
            fragment.push(b'\n');
        }
    }
    Ok(fragment)
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
///
/// Stock init can leave `/debug_ramdisk` as a plain directory on the read-only
/// system image, where the child mount points cannot be created. The whole
/// sequence therefore runs as one shell script under `set -e`: stock init has
/// no conditional command, so a separate preparation step could not stop the
/// staging commands from running after it failed. The script reuses a parent it
/// can create in, mounts its own tmpfs when the parent is read-only and empty,
/// and fails closed when that read-only parent already carries content the
/// mount would hide.
///
/// The script is one init action argument, so the stock parser constrains its
/// bytes: init expands properties in every argument and rejects the unbraced
/// `$name` form, and its tokenizer has no escape for `"` inside a quoted
/// token. The script uses no `$`, `"`, `\` or `#` at all. Only stock `sh`,
/// `ls`, and the toybox applets the staging already needs are available here:
/// the ESP payload, including esud, is not staged yet. `toybox mount` splits
/// `-o` into mount flags and filesystem data exactly like init's own `mount`,
/// so the device, access mode, nosuid/nodev/noexec flags and `mode=0755` data
/// of both staging mounts are unchanged.
fn bootstrap_rc(device: (u32, u32), writable: bool) -> String {
    let (major, minor) = device;
    let access = if writable { "rw" } else { "ro" };
    format!(
        "\non early-init\n    exec u:r:esu:s0 root -- /system/bin/sh -c \"set -eu; \
         test -d /debug_ramdisk; test ! -L /debug_ramdisk; \
         if /system/bin/toybox mkdir /debug_ramdisk/.esu-probe; then \
         /system/bin/toybox rmdir /debug_ramdisk/.esu-probe; \
         elif /system/bin/toybox ls -A /debug_ramdisk | read entry; then exit 1; \
         else /system/bin/toybox mount -t tmpfs -o nosuid,nodev,mode=0755 esu-parent /debug_ramdisk; fi; \
         /system/bin/toybox mknod /dev/esu-esp b {major} {minor}; \
         /system/bin/toybox mkdir -p /debug_ramdisk/esp; \
         /system/bin/toybox chmod 0700 /debug_ramdisk/esp; \
         /system/bin/toybox mount -t vfat -o {access},nosuid,nodev,noexec /dev/esu-esp /debug_ramdisk/esp; \
         /system/bin/toybox mkdir -p /debug_ramdisk/esu; \
         /system/bin/toybox chmod 0755 /debug_ramdisk/esu; \
         /system/bin/toybox mount -t tmpfs -o nosuid,nodev,mode=0755 esu /debug_ramdisk/esu; \
         /system/bin/toybox cp -R /debug_ramdisk/esp/esu/bin /debug_ramdisk/esu/bin; \
         /system/bin/toybox chmod -R 0755 /debug_ramdisk/esu/bin; \
         echo {major}:{minor} > /debug_ramdisk/esu/esp-device\"\n\n"
    )
}

pub fn publish_module_rc(
    payload: &Path,
    order: &[String],
    device: (u32, u32),
    writable: bool,
) -> Result<(), Failure> {
    let mut rc = bootstrap_rc(device, writable).into_bytes();
    let modules = module_rc_with_limit(
        payload,
        order,
        crate::scripts::is_recovery(),
        MAX_MODULE_RC.saturating_sub(rc.len()),
    )?;
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
        fn marker(&self, id: &str, name: &str) {
            let directory = self.0.join("modules").join(id);
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join(name), b"").unwrap();
        }
        fn prop(&self, id: &str, text: &str) {
            let directory = self.0.join("modules").join(id);
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("module.prop"), text).unwrap();
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
    fn the_exact_limit_is_published_and_an_oversized_optional_module_is_skipped() {
        let temp = Temp::new();
        let header = b"# === a/initrc/a.rc ===\n";
        temp.rc("a", "a.rc", &vec![b'\n'; MAX_MODULE_RC - header.len()]);
        assert_eq!(
            module_rc(&temp.0, &["a".into()], false).unwrap().len(),
            MAX_MODULE_RC
        );

        // The optional module contributes nothing rather than a partial RC.
        temp.rc("a", "a.rc", &vec![b'\n'; MAX_MODULE_RC - header.len() + 1]);
        assert!(module_rc(&temp.0, &["a".into()], false).unwrap().is_empty());
    }
    #[test]
    fn an_oversized_critical_module_rejects_the_handoff() {
        let temp = Temp::new();
        temp.marker("a", "critical");
        let header = b"# === a/initrc/a.rc ===\n";
        temp.rc("a", "a.rc", &vec![b'\n'; MAX_MODULE_RC - header.len() + 1]);
        assert_eq!(
            module_rc(&temp.0, &["a".into()], false).unwrap_err().error,
            "ModuleRcTooLarge"
        );
    }
    #[test]
    fn an_unreadable_optional_module_is_skipped_and_a_critical_one_rejects() {
        let temp = Temp::new();
        temp.rc("a", "a.rc", b"a\n");
        temp.rc("b", "b.rc", b"\0");
        // The malformed optional module is skipped, the sibling still publishes.
        assert_eq!(
            module_rc(&temp.0, &["a".into(), "b".into()], false).unwrap(),
            b"# === a/initrc/a.rc ===\na\n"
        );
        temp.marker("b", "critical");
        assert_eq!(
            module_rc(&temp.0, &["b".into()], false).unwrap_err().error,
            "ModuleRcUnreadable"
        );
    }
    #[test]
    fn recovery_admits_only_marked_modules_and_a_malformed_marker_only_filters() {
        let temp = Temp::new();
        temp.rc("a", "a.rc", b"a\n");
        temp.rc("b", "b.rc", b"b\n");
        temp.marker("b", "recovery-ok");
        assert_eq!(
            module_rc(&temp.0, &["a".into(), "b".into()], true).unwrap(),
            b"# === b/initrc/b.rc ===\nb\n"
        );

        // A malformed admission marker filters the module, it never stops boot.
        fs::remove_file(temp.0.join("modules/b/recovery-ok")).unwrap();
        std::os::unix::fs::symlink("missing", temp.0.join("modules/b/recovery-ok")).unwrap();
        temp.marker("b", "critical");
        assert!(module_rc(&temp.0, &["b".into()], true).unwrap().is_empty());
    }
    #[test]
    fn disable_and_remove_skip_a_module_even_when_it_is_critical() {
        let temp = Temp::new();
        temp.rc("a", "a.rc", b"a\n");
        temp.marker("a", "critical");
        temp.marker("a", "disable");
        assert!(module_rc(&temp.0, &["a".into()], false).unwrap().is_empty());

        fs::remove_file(temp.0.join("modules/a/disable")).unwrap();
        temp.marker("a", "remove");
        assert!(module_rc(&temp.0, &["a".into()], false).unwrap().is_empty());

        assert!(module_rc(&temp.0, &[], false).unwrap().is_empty());
    }
    #[test]
    fn invalid_optional_identity_cannot_publish_init_actions() {
        let temp = Temp::new();
        temp.prop("a", "id=wrong\n");
        temp.rc("a", "a.rc", b"invalid module action\n");
        temp.prop("c", "id=c\n");
        temp.rc("c", "c.rc", b"valid module action\n");
        temp.marker("c", "critical");
        temp.marker("a", "recovery-ok");
        temp.marker("c", "recovery-ok");
        let mut order = vec!["a".into(), "missing".into(), "c".into()];
        validate_modules(&temp.0, &mut order, false).unwrap();
        assert_eq!(order, ["c"]);
        let rc = String::from_utf8(module_rc(&temp.0, &order, false).unwrap()).unwrap();
        let actions: Vec<_> = rc.lines().filter(|line| !line.starts_with('#')).collect();
        assert_eq!(actions, ["valid module action"]);
        temp.marker("a", "critical");
        assert_eq!(
            validate_modules(&temp.0, &mut vec!["a".into()], false)
                .unwrap_err()
                .error,
            "ModuleIdMismatch"
        );
    }
}
