//! PID1 scripts are distinct from Android-side KernelSU lifecycle scripts.

use std::fs;
use std::io;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::receipt::{Failure, Stage};

/// Bootconfig key Android uses to select a normal boot instead of recovery.
const FORCE_NORMAL_BOOT: &str = "androidboot.force_normal_boot";

/// AOSP's `IsRecoveryMode()` marker: the recovery ramdisk's own executable.
pub(crate) const RECOVERY_EXECUTABLE: &str = "/system/bin/recovery";

/// Fixed deadline for a single module script. PID 1 cannot wait forever.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(35);

/// Polling interval while waiting for a script to finish.
const SCRIPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Shared mode for vendor module lists and PID-1 script selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BootMode {
    Normal,
    Recovery,
    Charger,
}

/// Classify boot inputs without reading the host filesystem.
///
/// Bootconfig overrides the command line per key. Explicit recovery wins over
/// charger; otherwise AOSP recovery needs its executable and no forced normal boot.
pub(crate) fn classify_boot_mode<'a>(
    bootconfig: &'a str,
    cmdline: &'a str,
    recovery_present: bool,
) -> BootMode {
    let value = |key: &str| -> Option<&'a str> {
        bootconfig
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once('=')?;
                (name.trim() == key).then(|| value.trim().trim_matches('"').trim())
            })
            .or_else(|| {
                cmdline.split_whitespace().find_map(|token| {
                    let (name, value) = token.split_once('=')?;
                    (name == key).then(|| value.trim_matches('"'))
                })
            })
    };

    if matches!(
        value("androidboot.mode"),
        Some("recovery" | "fastboot" | "fastbootd")
    ) || value("androidboot.recovery") == Some("1")
    {
        return BootMode::Recovery;
    }

    if value("androidboot.mode") == Some("charger") {
        return BootMode::Charger;
    }

    let force_normal = value(FORCE_NORMAL_BOOT);

    if force_normal.is_some_and(|value| value != "0" && value != "1") {
        log::warn!("Unexpected {FORCE_NORMAL_BOOT} value {force_normal:?}");
    }

    if recovery_present && force_normal != Some("1") {
        return BootMode::Recovery;
    }

    BootMode::Normal
}

/// Stable selection shared by every PID1 module script and core RC handshake.
pub fn is_recovery() -> bool {
    static MODE: std::sync::LazyLock<BootMode> = std::sync::LazyLock::new(|| {
        classify_boot_mode(
            &fs::read_to_string("/proc/bootconfig").unwrap_or_default(),
            &fs::read_to_string("/proc/cmdline").unwrap_or_default(),
            Path::new(RECOVERY_EXECUTABLE).exists(),
        )
    });
    *MODE == BootMode::Recovery
}

/// Run the module's early or recovery script when the ESP provides one.
///
/// Admission follows the module markers. An absent script is not a failure:
/// the stage simply does not exist for that module. An optional module whose
/// script cannot be prepared, spawned, or finished within the fixed deadline is
/// logged and skipped so the remaining modules still run; a critical module's
/// failure is returned and rejects the handoff. A script that outlives the
/// deadline is killed and reaped before that decision.
///
/// `esu_stage` is the derived `ESU_STAGE` value of this boot, exported next to
/// `ESU_ROM`/`ESU_ROM_NUMBER` so the module's helpers address the staged letter
/// exactly as PID 1 resolved it.
pub fn run_module_scripts(
    payload_root: &Path,
    module: &str,
    rom: &str,
    rom_number: u32,
    esu_stage: &str,
) -> Result<(), Failure> {
    let directory = payload_root.join("modules").join(module);
    let crate::platform::ModulePolicy::Admitted { critical } =
        crate::platform::module_policy(&directory, is_recovery())?
    else {
        return Ok(());
    };
    match run_script(&directory, module, rom, rom_number, esu_stage) {
        Ok(()) => Ok(()),
        Err(failure) if critical => Err(failure),
        Err(failure) => {
            log::warn!(
                "Skipping optional module {module} script: {}: {}",
                failure.error,
                failure.detail
            );
            Ok(())
        }
    }
}

/// Prepare, spawn, and wait for one admitted module's PID1 script.
fn run_script(
    directory: &Path,
    module: &str,
    rom: &str,
    rom_number: u32,
    esu_stage: &str,
) -> Result<(), Failure> {
    let script_name = if is_recovery() {
        "pid1-recovery.sh"
    } else {
        "pid1.sh"
    };
    let script = directory.join(script_name);

    match fs::symlink_metadata(&script) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(Failure::at(
                Stage::ModuleLoad,
                Some(module),
                "ScriptUnreadable",
                format!("cannot access {}: {error}", script.display()),
            ));
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(Failure::at(
                Stage::ModuleLoad,
                Some(module),
                "ScriptSymlink",
                format!("{} is a symbolic link", script.display()),
            ));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(Failure::at(
                Stage::ModuleLoad,
                Some(module),
                "ScriptNotRegular",
                format!("{} is not a regular file", script.display()),
            ));
        }
        Ok(_) => {}
    }

    let bin = Path::new(crate::esp::EXECUTABLE_BIN);
    let busybox = bin.join("busybox");

    match fs::symlink_metadata(&busybox) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(Failure::at(
                Stage::ModuleLoad,
                Some(module),
                "ScriptInterpreterMissing",
                format!("{} is not a regular file", busybox.display()),
            ));
        }
        Err(error) => {
            return Err(Failure::at(
                Stage::ModuleLoad,
                Some(module),
                "ScriptInterpreterMissing",
                format!("cannot use {}: {error}", busybox.display()),
            ));
        }
    }

    log::info!("Running {script_name} for module {module}");

    let mut child = Command::new(&busybox)
        .arg("sh")
        .arg(&script)
        .env_clear()
        .env("PATH", bin)
        .env("ESU_ROM", rom)
        .env("ESU_ROM_NUMBER", rom_number.to_string())
        .env("ESU_STAGE", esu_stage)
        .current_dir(directory)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|error| {
            Failure::at(
                Stage::ModuleLoad,
                Some(module),
                "ScriptSpawn",
                format!(
                    "cannot run {} sh {}: {error}",
                    busybox.display(),
                    script.display()
                ),
            )
        })?;

    let status = match wait_with_deadline(&mut child, SCRIPT_TIMEOUT) {
        Ok(Some(status)) => status,
        Ok(None) => {
            kill_and_reap(&mut child);
            return Err(Failure::at(
                Stage::ModuleLoad,
                Some(module),
                "ScriptTimeout",
                format!(
                    "{script_name} did not finish within {}s and was killed",
                    SCRIPT_TIMEOUT.as_secs()
                ),
            ));
        }
        Err(error) => {
            kill_and_reap(&mut child);
            return Err(Failure::at(
                Stage::ModuleLoad,
                Some(module),
                "ScriptWait",
                format!("cannot wait for {script_name}: {error}"),
            ));
        }
    };

    if !status.success() {
        return Err(Failure::at(
            Stage::ModuleLoad,
            Some(module),
            "ScriptFailed",
            format!("{script_name} exited with {status}"),
        ));
    }

    Ok(())
}

/// Wait for the script until `timeout` elapses, returning `Ok(None)` when the
/// deadline passes so the caller can kill and reap the child.
fn wait_with_deadline(child: &mut Child, timeout: Duration) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;

    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }

        if Instant::now() >= deadline {
            return Ok(None);
        }

        thread::sleep(SCRIPT_POLL_INTERVAL);
    }
}

/// Terminate a timed-out script and always reap it, so PID 1 never keeps a
/// zombie and never returns to the module loop with the script still running.
fn kill_and_reap(child: &mut Child) {
    if let Err(error) = child.kill() {
        log::warn!("cannot kill timed-out script process: {error}");
    }

    if let Err(error) = child.wait() {
        log::warn!("cannot reap timed-out script process: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn userspace_fastboot_selects_recovery_even_with_force_normal_boot() {
        for mode in ["fastboot", "fastbootd"] {
            let command = format!("androidboot.mode={mode} androidboot.force_normal_boot=1");
            assert_eq!(classify_boot_mode("", &command, false), BootMode::Recovery);
            let boot =
                format!("androidboot.mode = \"{mode}\"\nandroidboot.force_normal_boot = \"1\"");
            assert_eq!(
                classify_boot_mode(&boot, "androidboot.mode=normal", false),
                BootMode::Recovery
            );
        }
    }

    #[test]
    fn boot_mode_precedence() {
        let mode = |cmdline, recovery| classify_boot_mode("", cmdline, recovery);

        // Explicit recovery wins over charger and over a forced normal boot.
        assert_eq!(
            mode(
                "androidboot.mode=recovery androidboot.force_normal_boot=1",
                false
            ),
            BootMode::Recovery
        );
        assert_eq!(
            mode("androidboot.mode=charger androidboot.recovery=1", true),
            BootMode::Recovery
        );
        assert_eq!(
            mode(
                "androidboot.mode=charger androidboot.force_normal_boot=0",
                true
            ),
            BootMode::Charger
        );
        assert_eq!(mode("androidboot.mode=charger", false), BootMode::Charger);

        // AOSP recovery needs its executable and no forced normal boot.
        assert_eq!(
            mode("androidboot.force_normal_boot=1", true),
            BootMode::Normal
        );
        assert_eq!(
            mode("androidboot.force_normal_boot=0", false),
            BootMode::Normal
        );
        assert_eq!(
            mode("androidboot.force_normal_boot=0", true),
            BootMode::Recovery
        );
        assert_eq!(mode("", true), BootMode::Recovery);
        assert_eq!(mode("", false), BootMode::Normal);
    }

    #[test]
    fn optional_module_script_failures_are_skipped_and_critical_ones_are_returned() {
        let root = std::env::temp_dir().join(format!("esu-scripts-{}", std::process::id()));
        let directory = root.join("modules").join("a");
        fs::create_dir_all(&directory).unwrap();
        for name in ["pid1.sh", "pid1-recovery.sh"] {
            std::os::unix::fs::symlink("missing", directory.join(name)).unwrap();
        }
        // Admitted in both boot modes, so the case does not depend on the mode.
        fs::write(directory.join(crate::platform::RECOVERY_OK_MARKER), b"").unwrap();

        // An optional module's broken script is skipped.
        assert!(run_module_scripts(&root, "a", "rom", 1, "").is_ok());

        // The same script in a critical module rejects the handoff.
        fs::write(directory.join(crate::platform::CRITICAL_MARKER), b"").unwrap();
        assert_eq!(
            run_module_scripts(&root, "a", "rom", 1, "")
                .unwrap_err()
                .error,
            "ScriptSymlink"
        );

        // A deliberate skip wins over the critical marker.
        fs::write(directory.join(crate::platform::DISABLE_MARKER), b"").unwrap();
        assert!(run_module_scripts(&root, "a", "rom", 1, "").is_ok());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn bootconfig_is_authoritative_over_the_command_line() {
        assert_eq!(
            classify_boot_mode(
                "androidboot.mode = \"recovery\"\n",
                "androidboot.mode=charger",
                false
            ),
            BootMode::Recovery
        );
        assert_eq!(
            classify_boot_mode(
                "androidboot.mode = charger\n",
                "androidboot.mode=recovery",
                true
            ),
            BootMode::Charger
        );
        assert_eq!(
            classify_boot_mode(
                "androidboot.force_normal_boot = \"1\"\n",
                "androidboot.force_normal_boot=0",
                true
            ),
            BootMode::Normal
        );
    }
}
