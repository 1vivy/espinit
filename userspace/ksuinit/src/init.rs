//! espinit PID-1 early managed boot.
//!
//! The order is fixed by the boot contract: minimal mounts and logging, normal
//! vendor module loading, ESP discovery and read-only mount, strict manifest
//! and ROM validation, generation matching, ordered payload module loading with
//! self-checks, the single projection boundary immediately before the `gpt`
//! entry, and finally the real-init handoff. Any failure stops the handoff,
//! persists a receipt, and enters the fatal-boot stop path: a reboot by
//! default, or the AOSP sysrq crash when the exact
//! `androidboot.init_fatal_panic=true` opt-in is active. There is no soft
//! fallback and no stock-ROM fallback.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use rustix::fs::{CWD, FileType, makedev, mknodat};
use rustix::system::{RebootCommand, reboot};

use crate::block;
use crate::config::{self, Manifest, RomConfig};
use crate::esp;
use crate::gptctl;
use crate::loader;
use crate::receipt::{Failure, ReceiptState, Stage};
use crate::scripts;
use crate::selfcheck;

/// Generation compiled into this PID-1 binary, derived by `build.rs` from
/// `ESPINIT_GENERATION` or the full Git HEAD hash.
pub const BINARY_GENERATION: &str = env!("ESPINIT_GENERATION");

/// Run the early managed boot. This must run as process 1: the entry point
/// refuses to continue otherwise, before any platform side effect. On success
/// the caller may hand off to the real init; every error is classified for the
/// failure receipt.
pub fn run(state: &mut ReceiptState) -> Result<(), Failure> {
    setup_kmsg();
    log::info!("espinit early managed boot starting");
    let mounts = mount_minimal()?;
    unlimit_kmsg();

    // UFS and VFAT are built in on the phone, so the payload ESP can already be
    // available before vendor module preload. Retain that mount when possible:
    // a preload failure can then persist its normal bounded failure receipt.
    // Devices that need modular storage keep the original load-then-wait path.
    state.esp_mount = match esp::mount_esp() {
        Ok(mount) => {
            log::info!("ESP was available before vendor module preload");
            Some(mount)
        }
        Err(failure) if esp_enumeration_pending(&failure) => None,
        Err(failure) => return Err(failure),
    };

    loader::load_vendor_modules()?;

    if state.esp_mount.is_none() {
        state.esp_mount = Some(wait_for_esp()?);
    }
    let mount = state.esp_mount.as_ref().ok_or_else(|| {
        Failure::new(
            Stage::Storage,
            "EspMountUnavailable",
            "ESP discovery succeeded without retaining its mount",
        )
    })?;
    let esp_mount = mount.path().to_owned();
    let esp_device = mount.device();
    let payload_root = esp::payload_root(mount.path());

    let manifest = read_manifest(&payload_root)?;
    let rom = read_rom(&payload_root, &manifest)?;

    check_binary_generation(&manifest)?;
    state.generation = Some(manifest.generation.clone());

    config::validate_managed(&manifest, &rom).map_err(Failure::from)?;

    if rom.managed {
        // Backend resolution is deliberately deferred until immediately before
        // the `gpt` entry, after every earlier ordered module and script has
        // run: a logical volume, mapper device, loop or ESP file published by
        // them is visible there, and no backend is touched for a ROM that never
        // reaches its projection.
        require_receipt_storage(&payload_root)?;
    } else if !payload_root.join("receipts").is_dir() {
        log::warn!("ESP receipt directory is missing; failures cannot be persisted");
    }

    // Bounded, path-free summary of the validated configuration. The requested
    // access modes are the input to projection; report them before the `gpt`
    // module publishes anything.
    log::info!("Validated configuration: {}", rom.partition_modes());

    log::info!(
        "Boot mode: {}, managed: {}",
        if scripts::is_recovery() {
            "recovery"
        } else {
            "normal"
        },
        rom.managed
    );

    load_and_check_payload(&payload_root, &manifest, &rom, &esp_mount, esp_device)?;
    crate::platform::stage(&payload_root, &manifest, &rom)?;

    log::info!(
        "Early managed boot checks passed; handing off to {}",
        crate::handoff::REAL_INIT
    );

    prepare_handoff(state, &mounts)
}

/// Load the payload modules in manifest order, self-check each one, and run its
/// early or recovery script before the next entry is processed.
///
/// The `gpt` entry is the single projection boundary. Its backends are resolved
/// only when that entry is reached, after every earlier module and script has
/// run, so a logical volume, mapper device, loop or ESP file published by them
/// is visible. The complete projection is applied and verified before the
/// module's stage script runs; nothing earlier publishes a projected view and
/// there is no partial fallback.
fn load_and_check_payload(
    payload_root: &Path,
    manifest: &Manifest,
    rom: &RomConfig,
    esp_mount: &str,
    esp_device: (u32, u32),
) -> Result<(), Failure> {
    let generation = manifest.generation.as_str();
    let modes = rom.partition_modes();
    let core = manifest
        .modules
        .first()
        .ok_or_else(|| Failure::new(Stage::Configuration, "ManifestModulesEmpty", "no modules"))?;

    if crate::core_loaded() {
        log::info!("Core module is already loaded; validating it instead of reloading");
    } else {
        let path = loader::resolve_payload_file(payload_root, &core.path, &core.name)?;
        loader::load_managed_module(&path, core)?;
    }

    selfcheck::check_core(generation)?;
    crate::platform::select_core_boot_mode()?;
    scripts::run_module_scripts(payload_root, &core.name, generation)?;

    let mut projection_checked = false;

    for entry in manifest.modules.iter().skip(1) {
        let path = loader::resolve_payload_file(payload_root, &entry.path, &entry.name)?;

        if entry.name == "gpt" {
            // Every earlier entry and script has run, so anything they created
            // is visible here. The resolved set keeps every ESP-file loop guard
            // open until APPLY has returned.
            resolve_backends(rom, esp_mount)?;
        }

        if loader::module_loaded(&entry.name) {
            log::info!(
                "Module {} is already loaded; validating it instead of reloading",
                entry.name
            );
        } else {
            loader::load_managed_module(&path, entry)?;
        }

        if entry.name == "gpt" {
            // Identity is checked before the consequential APPLY. Readiness can
            // only become true after one atomic APPLY and exact QUERY.
            selfcheck::check_module_generation(&entry.name, generation)?;
            apply_projection(rom, esp_device)?;
            selfcheck::check_projection_ready(&entry.name, modes)?;
            projection_checked = true;
        } else {
            selfcheck::check_module(&entry.name, generation)?;
        }

        scripts::run_module_scripts(payload_root, &entry.name, generation)?;
    }

    if rom.managed && !projection_checked {
        // Configuration validation already requires `gpt` for a managed ROM;
        // this guards against the two checks drifting apart.
        return Err(Failure::new(
            Stage::Projection,
            "ProjectionModuleMissing",
            format!("a managed ROM requires an initialized gpt module before handoff ({modes})"),
        ));
    }

    Ok(())
}

/// The PID-1 binary must carry the same generation as the manifest.
fn check_binary_generation(manifest: &Manifest) -> Result<(), Failure> {
    if BINARY_GENERATION != manifest.generation {
        return Err(Failure::at(
            Stage::Generation,
            Some("bin/espinit"),
            "HandoffGenerationMismatch",
            format!(
                "PID-1 generation {BINARY_GENERATION} does not match manifest generation {}",
                manifest.generation
            ),
        ));
    }

    Ok(())
}

/// Receipt storage is required for a managed ROM: unavailable receipt storage
/// is itself a hard failure, and `/metadata` must never be involved.
fn require_receipt_storage(payload_root: &Path) -> Result<(), Failure> {
    let receipts = payload_root.join("receipts");

    match fs::symlink_metadata(&receipts) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(Failure::new(
            Stage::Storage,
            "ReceiptStorageUnavailable",
            format!("{} is not a directory", receipts.display()),
        )),
        Err(error) => Err(Failure::new(
            Stage::Storage,
            "ReceiptStorageUnavailable",
            format!("cannot use {}: {error}", receipts.display()),
        )),
    }
}

fn read_manifest(payload_root: &Path) -> Result<Manifest, Failure> {
    let text = read_config_file(payload_root, "manifest.toml", "ManifestUnreadable")?;

    config::parse_manifest(&text).map_err(Failure::from)
}

fn read_rom(payload_root: &Path, manifest: &Manifest) -> Result<RomConfig, Failure> {
    let bootconfig = match fs::read_to_string("/proc/bootconfig") {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(Failure::new(
                Stage::Configuration,
                "BootconfigUnreadable",
                error.to_string(),
            ));
        }
    };
    let cmdline = fs::read_to_string("/proc/cmdline").map_err(|error| {
        Failure::new(Stage::Configuration, "CmdlineUnreadable", error.to_string())
    })?;
    let id = config::selected_rom_id(&bootconfig, &cmdline).map_err(Failure::from)?;
    let path = config::rom_path(manifest, id).map_err(Failure::from)?;
    let text = read_config_file(payload_root, &path, "RomUnreadable")?;
    config::parse_selected_rom(&text, &manifest.generation, id).map_err(Failure::from)
}

/// Read a configuration file rooted at the ESP `/espinit` subtree, rejecting a
/// symbolic link in any component: configuration must never traverse a link.
fn read_config_file(
    payload_root: &Path,
    relative: &str,
    unreadable: &'static str,
) -> Result<String, Failure> {
    let mut path = payload_root.to_path_buf();

    for part in relative.split('/') {
        path.push(part);

        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Failure::at(
                    Stage::Configuration,
                    Some(relative),
                    "PathSymlink",
                    format!("{} is a symbolic link", path.display()),
                ));
            }
            Ok(_) => {}
            Err(error) => {
                return Err(Failure::at(
                    Stage::Configuration,
                    Some(relative),
                    unreadable,
                    format!("cannot access {}: {error}", path.display()),
                ));
            }
        }
    }

    fs::read_to_string(&path).map_err(|error| {
        Failure::at(
            Stage::Configuration,
            Some(relative),
            unreadable,
            format!("cannot read {}: {error}", path.display()),
        )
    })
}

/// Upper bound for an asynchronously enumerated device to appear once the
/// vendor storage modules have been loaded: beyond this window the ESP and the
/// managed backends are genuinely absent.
const ENUMERATION_WINDOW: Duration = Duration::from_secs(10);

/// Delay between enumeration attempts inside the window.
const ENUMERATION_RETRY: Duration = Duration::from_millis(100);

/// Retry `probe` within a bounded window while `pending` classifies a failure
/// as a device or sysfs entry that is not enumerated yet. A permanent failure
/// stops immediately, and the last exact failure is returned when the window
/// expires, so nothing is hidden behind a timeout or a fallback.
fn retry_enumerated<T, E>(
    what: &str,
    window: Duration,
    interval: Duration,
    mut probe: impl FnMut() -> Result<T, E>,
    pending: impl Fn(&E) -> bool,
) -> Result<T, E> {
    let deadline = Instant::now() + window;

    loop {
        match probe() {
            Ok(value) => return Ok(value),
            Err(error) => {
                if !pending(&error) || Instant::now() >= deadline {
                    return Err(error);
                }

                log::info!("{what} is not enumerated yet; retrying within the discovery window");
                std::thread::sleep(interval);
            }
        }
    }
}

fn esp_enumeration_pending(failure: &Failure) -> bool {
    matches!(
        failure.error,
        "EspNotFound" | "EspPayloadNotFound" | "EspSysfsUnavailable" | "EspPartitionMissing"
    )
}

/// Discover and mount the ESP, retrying within the bounded enumeration window.
/// Block devices and their sysfs entries appear asynchronously after the vendor
/// storage modules load, so a single probe can lose a race that is not a real
/// failure.
fn wait_for_esp() -> Result<esp::Mount, Failure> {
    retry_enumerated(
        "ESP",
        ENUMERATION_WINDOW,
        ENUMERATION_RETRY,
        esp::mount_esp,
        esp_enumeration_pending,
    )
}

/// Resolve the managed backends within the bounded enumeration window,
/// immediately before the `gpt` entry. Other UFS LUN, mapper and block-device
/// enumeration can race the managed boot, so a single probe can lose a race
/// that is not a real failure. Ambiguous, malformed, unsupported, duplicated,
/// writable-ESP-file, and invalid configuration stays immediate fatal, and the
/// resolved set stays retained on the configuration so every loop guard lives
/// through APPLY.
fn resolve_backends(rom: &RomConfig, esp_mount: &str) -> Result<(), Failure> {
    retry_enumerated(
        "managed backend",
        ENUMERATION_WINDOW,
        ENUMERATION_RETRY,
        || config::validate_backends(rom, esp_mount).map(|_| ()),
        config::ConfigError::is_pending,
    )
    .map_err(Failure::from)
}
pub(crate) fn resolve_native_metadata() -> std::io::Result<block::ResolvedBackend> {
    retry_native_metadata(ENUMERATION_WINDOW, ENUMERATION_RETRY, || {
        block::resolve("/dev/block/by-name/metadata", esp::ESP_MOUNT_POINT)
    })
}

fn retry_native_metadata<T>(
    window: Duration,
    interval: Duration,
    probe: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    retry_enumerated(
        "native metadata",
        window,
        interval,
        probe,
        block::is_pending,
    )
}

/// Apply and verify the complete projection: enumerate only physical
/// partitions whose PARTNAME collides with a projected name, retain the
/// mounted ESP for failure receipts and unrelated stock partitions for normal
/// platform operation, build the exact APPLY payload from the resolved
/// backends, issue APPLY, and require QUERY to report the exact projection.
fn apply_projection(rom: &RomConfig, esp_device: (u32, u32)) -> Result<(), Failure> {
    let hide = retry_enumerated(
        "shadowed physical partitions",
        ENUMERATION_WINDOW,
        ENUMERATION_RETRY,
        || {
            block::hidden_partitions(
                crate::gpt_uapi::GptDevice {
                    major: esp_device.0,
                    minor: esp_device.1,
                },
                |name| {
                    rom.partitions
                        .iter()
                        .any(|partition| partition.name == name)
                },
            )
        },
        block::is_pending,
    )
    .map_err(|error| {
        Failure::new(
            Stage::Projection,
            "ProjectionEnumerationFailed",
            format!("cannot enumerate physical partitions: {error}"),
        )
    })?;

    gptctl::project(&rom.partitions, rom.resolved_backends(), &hide)
}

/// Prepare the minimum early mounts, retaining only the mounts created here.
fn mount_minimal() -> Result<Vec<&'static str>, Failure> {
    let mut owned = Vec::with_capacity(3);

    for (filesystem, mountpoint, error) in [
        ("proc", "/proc", "ProcMountFailed"),
        ("sysfs", "/sys", "SysMountFailed"),
    ] {
        if !esp::is_mounted(mountpoint)
            .map_err(|detail| Failure::new(Stage::Storage, error, detail))?
        {
            esp::mount_kernel_fs(filesystem, mountpoint)
                .map_err(|detail| Failure::new(Stage::Storage, error, detail))?;
            owned.push(mountpoint);
        }
    }

    if !esp::is_mounted("/dev")
        .map_err(|detail| Failure::new(Stage::Storage, "DevMountFailed", detail))?
    {
        if let Err(devtmpfs_error) = esp::mount_kernel_fs("devtmpfs", "/dev") {
            esp::mount_kernel_fs("tmpfs", "/dev").map_err(|tmpfs_error| {
                Failure::new(
                    Stage::Storage,
                    "DevMountFailed",
                    format!("cannot mount devtmpfs ({devtmpfs_error}) or tmpfs ({tmpfs_error})"),
                )
            })?;
            log::warn!("devtmpfs is unavailable; using an empty tmpfs and explicit device nodes");
        }
        owned.push("/dev");
    }

    for (path, major, minor) in [("/dev/kmsg", 1, 11), ("/dev/null", 1, 3)] {
        if rustix::fs::access(path, rustix::fs::Access::EXISTS).is_ok() {
            continue;
        }
        mknodat(
            CWD,
            path,
            FileType::CharacterDevice,
            0o600.into(),
            makedev(major, minor),
        )
        .map_err(|error| {
            Failure::new(
                Stage::Storage,
                "DevNodeCreateFailed",
                format!("cannot create {path}: {error}"),
            )
        })?;
    }

    Ok(owned)
}

/// Remove espinit's own early mounts immediately before the real init runs, so
/// Android's first-stage init starts with a clean mount namespace. Only mounts
/// espinit created are removed, and a teardown failure is a handoff failure.
/// The ESP's block device identity is retained so a failed handoff exec can
/// still re-attach the ESP and persist its receipt.
fn prepare_handoff(state: &mut ReceiptState, mounts: &[&str]) -> Result<(), Failure> {
    if let Some(mount) = state.esp_mount.as_mut() {
        mount.detach()?;
    }

    for mountpoint in mounts.iter().rev() {
        esp::detach_owned(mountpoint)?;
    }

    Ok(())
}

/// Set up kernel logging as early as possible.
fn setup_kmsg() {
    const KMSG: &str = "/dev/kmsg";

    let device = match rustix::fs::access(KMSG, rustix::fs::Access::EXISTS) {
        Ok(_) => KMSG,
        Err(_) => {
            mknodat(
                CWD,
                "/kmsg",
                FileType::CharacterDevice,
                0o666.into(),
                makedev(1, 11),
            )
            .ok();
            "/kmsg"
        }
    };

    let _ = kernlog::init_with_device(device);
}

fn unlimit_kmsg() {
    if let Ok(mut rate) = fs::File::options()
        .write(true)
        .open("/proc/sys/kernel/printk_devkmsg")
    {
        writeln!(rate, "on").ok();
    }
}

/// Exact AOSP opt-in that turns a fatal espinit failure into a real kernel
/// crash instead of the default reboot.
const FATAL_PANIC_KEY: &str = "androidboot.init_fatal_panic";

/// The only value that enables the opt-in; AOSP compares it literally.
const FATAL_PANIC_VALUE: &str = "true";

/// AOSP's sysrq crash request. Writing this byte to `/proc/sysrq-trigger`
/// panics the kernel, so the failure can be captured through pstore/minidump
/// instead of only rebooting.
const SYSRQ_TRIGGER: &str = "/proc/sysrq-trigger";
const SYSRQ_CRASH: &[u8] = b"c";

/// Which fatal-boot stop applies to the current boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FatalStop {
    /// The exact AOSP opt-in is active: request a kernel panic.
    Panic,
    /// No opt-in: sync, reboot, and park PID 1, unchanged.
    Reboot,
}

/// Enter the fatal-boot stop path for a failure the caller classified.
///
/// The `record` closure persists the bounded ESP failure receipt first, so the
/// evidence survives the crash capture. A kernel panic is requested only for
/// the exact opt-in and never returns; every other outcome continues into
/// [`stop_boot`], the unchanged reboot-and-park path, so a failing opt-in can
/// never strand PID 1.
pub fn fatal_boot(record: impl FnOnce()) -> ! {
    let bootconfig = fs::read_to_string("/proc/bootconfig").unwrap_or_default();
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let stop = if fatal_panic_requested(&bootconfig, &cmdline) {
        FatalStop::Panic
    } else {
        FatalStop::Reboot
    };

    stop_with(stop, record, crash_kernel, || stop_boot());

    unreachable!("the fatal-boot stop fallback never returns")
}

/// Run the fatal-boot stop with injectable side effects.
///
/// The receipt is recorded before any panic request, and the fallback is the
/// only observable outcome of a stop that did not panic the kernel.
fn stop_with(
    stop: FatalStop,
    record: impl FnOnce(),
    panic: impl FnOnce(),
    fallback: impl FnOnce(),
) {
    record();

    if stop == FatalStop::Panic {
        log::error!("{FATAL_PANIC_KEY}={FATAL_PANIC_VALUE}: requesting a sysrq crash");
        panic();
        log::error!("sysrq crash request returned without panicking the kernel");
    }

    fallback();
}

/// Request the AOSP sysrq crash. Returns only when the kernel did not panic.
fn crash_kernel() {
    match fs::File::options().write(true).open(SYSRQ_TRIGGER) {
        Ok(mut trigger) => {
            if let Err(error) = trigger.write_all(SYSRQ_CRASH) {
                log::error!("cannot write {SYSRQ_TRIGGER}: {error}");
            }
        }
        Err(error) => {
            log::error!("cannot open {SYSRQ_TRIGGER}: {error}");
        }
    }
}

/// Whether the exact AOSP fatal-panic opt-in is active for this boot.
///
/// The key is read from the boot configuration, falling back to the kernel
/// command line only when the boot configuration is silent for it, matching how
/// espinit resolves its other boot inputs. Only the exact key with the exact
/// value `true` opts in: case variants, key prefixes, malformed quote pairs and
/// every other value stay non-opt-in, and there is no OEM- or vendor-specific
/// spelling.
fn fatal_panic_requested(bootconfig: &str, cmdline: &str) -> bool {
    bootconfig_value(bootconfig).or_else(|| cmdline_value(cmdline)) == Some(FATAL_PANIC_VALUE)
}

/// Value the boot configuration attributes to the fatal-panic key, if any.
fn bootconfig_value(bootconfig: &str) -> Option<&str> {
    bootconfig.lines().find_map(|line| {
        let (name, value) = line.split_once('=').unwrap_or((line, ""));
        (name.trim() == FATAL_PANIC_KEY).then(|| unquote(value.trim()))
    })
}

/// Value the kernel command line attributes to the fatal-panic key, if any.
fn cmdline_value(cmdline: &str) -> Option<&str> {
    cmdline.split_whitespace().find_map(|token| {
        let (name, value) = token.split_once('=')?;
        (name == FATAL_PANIC_KEY).then(|| unquote(value))
    })
}

/// Strip exactly one surrounding pair of quotes. A lone quote is malformed and
/// is left in place, so it cannot compare equal to an accepted value.
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
}

/// Enter the fatal-boot stop path: sync, reboot, and never continue normal
/// boot when the reboot itself does not take effect.
pub fn stop_boot() -> ! {
    rustix::fs::sync();

    if let Err(error) = reboot(RebootCommand::Restart) {
        log::error!("cannot reboot after a failed early boot: {error}");
    }

    loop {
        std::thread::sleep(std::time::Duration::from_secs(600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_metadata_waits_only_for_enumeration() {
        let mut attempts = 0;
        let device = retry_native_metadata(Duration::from_secs(1), Duration::ZERO, || {
            attempts += 1;
            if attempts == 1 {
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            } else {
                Ok(42)
            }
        })
        .unwrap();
        assert_eq!((device, attempts), (42, 2));
        for kind in [
            std::io::ErrorKind::InvalidData,
            std::io::ErrorKind::PermissionDenied,
        ] {
            let mut attempts = 0;
            let result = retry_native_metadata(Duration::from_secs(1), Duration::ZERO, || {
                attempts += 1;
                Err::<(), _>(std::io::Error::from(kind))
            });
            assert_eq!(result.unwrap_err().kind(), kind);
            assert_eq!(attempts, 1);
        }
        let result = retry_native_metadata(Duration::ZERO, Duration::ZERO, || {
            Err::<(), _>(std::io::Error::from(std::io::ErrorKind::NotFound))
        });
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn permanent_failures_stop_without_retrying() {
        let mut attempts = 0;
        let error = retry_enumerated(
            "probe",
            Duration::from_secs(10),
            Duration::ZERO,
            || {
                attempts += 1;
                Err::<(), &str>("ambiguous backend")
            },
            |_| false,
        )
        .unwrap_err();

        assert_eq!(error, "ambiguous backend");
        assert_eq!(attempts, 1);
    }

    #[test]
    fn pending_failures_retry_until_the_probe_succeeds() {
        let mut attempts = 0;
        let value = retry_enumerated(
            "probe",
            Duration::from_secs(10),
            Duration::ZERO,
            || {
                attempts += 1;
                if attempts < 3 {
                    Err("absent")
                } else {
                    Ok(attempts)
                }
            },
            |_| true,
        )
        .unwrap();

        assert_eq!(value, 3);
        assert_eq!(attempts, 3);
    }

    #[test]
    fn an_expired_window_returns_the_last_exact_failure() {
        let mut attempts = 0;
        let error = retry_enumerated(
            "probe",
            Duration::ZERO,
            Duration::ZERO,
            || {
                attempts += 1;
                Err::<(), &str>("absent")
            },
            |_| true,
        )
        .unwrap_err();

        assert_eq!(error, "absent");
        assert_eq!(attempts, 1);
    }

    #[test]
    fn enumeration_retry_succeeds_and_reuses_the_published_set() {
        let rom = config::parse_rom(
            "schema_version = 1\ngeneration = \"release-1\"\nid = \"android-a\"\nmanaged = false\n",
            "release-1",
        )
        .unwrap();

        resolve_backends(&rom, esp::ESP_MOUNT_POINT).unwrap();
        // A second resolution reuses the published set instead of re-resolving.
        config::validate_backends(&rom, esp::ESP_MOUNT_POINT).unwrap();
        assert!(rom.resolved_backends().is_empty());
    }

    #[test]
    fn only_the_exact_bootconfig_key_and_value_opt_in() {
        let cases = [
            ("androidboot.init_fatal_panic=true\n", true),
            ("androidboot.init_fatal_panic = true", true),
            ("androidboot.init_fatal_panic=\"true\"\n", true),
            (
                "androidboot.force_normal_boot=0\nandroidboot.init_fatal_panic=true\n",
                true,
            ),
            ("", false),
            ("androidboot.init_fatal_panic=false\n", false),
            ("androidboot.init_fatal_panic=TRUE\n", false),
            ("androidboot.init_fatal_panic=True\n", false),
            ("androidboot.init_fatal_panic=1\n", false),
            ("androidboot.init_fatal_panic\n", false),
            ("androidboot.init_fatal_panic=\n", false),
            ("androidboot.init_fatal_panic=\"true\n", false),
            ("androidboot.init_fatal_panic=true\"\n", false),
            ("ANDROIDBOOT.INIT_FATAL_PANIC=true\n", false),
            ("androidboot.init_fatal_panicked=true\n", false),
            ("androidboot.init_fatal_panic_extra=true\n", false),
            ("vendor.androidboot.init_fatal_panic=true\n", false),
            ("androidboot.init_fatal_panic=true x\n", false),
        ];

        for (bootconfig, expected) in cases {
            assert_eq!(fatal_panic_requested(bootconfig, ""), expected);
        }
    }

    #[test]
    fn the_command_line_only_opts_in_when_the_bootconfig_is_silent() {
        let cases = [
            ("", "androidboot.init_fatal_panic=true", true),
            (
                "other=1\n",
                "loglevel=7 androidboot.init_fatal_panic=true",
                true,
            ),
            ("other=1\n", "androidboot.init_fatal_panic=\"true\"", true),
            ("", "androidboot.init_fatal_panic=false", false),
            ("", "androidboot.init_fatal_panicked=true", false),
            ("", "androidboot.init_fatal_panic", false),
            ("", "androidboot.init_fatal_panic=truex", false),
            (
                "androidboot.init_fatal_panic=false\n",
                "androidboot.init_fatal_panic=true",
                false,
            ),
        ];

        for (bootconfig, cmdline, expected) in cases {
            assert_eq!(fatal_panic_requested(bootconfig, cmdline), expected);
        }
    }

    #[test]
    fn the_failure_receipt_is_recorded_before_the_panic_request() {
        let events = std::cell::RefCell::new(Vec::new());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            stop_with(
                FatalStop::Panic,
                || events.borrow_mut().push("receipt"),
                || {
                    events.borrow_mut().push("sysrq");
                    panic!("kernel panicked");
                },
                || events.borrow_mut().push("fallback"),
            );
        }));

        let payload = outcome.expect_err("a requested kernel panic never returns");
        assert_eq!(*payload.downcast_ref::<&str>().unwrap(), "kernel panicked");
        assert_eq!(*events.borrow(), ["receipt", "sysrq"]);
    }

    #[test]
    fn a_panic_request_that_returns_falls_back_to_the_reboot_stop() {
        let events = std::cell::RefCell::new(Vec::new());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            stop_with(
                FatalStop::Panic,
                || events.borrow_mut().push("receipt"),
                || events.borrow_mut().push("sysrq"),
                || {
                    events.borrow_mut().push("fallback");
                    panic!("reboot stop");
                },
            );
        }));

        let payload = outcome.expect_err("the reboot stop parks PID 1");
        assert_eq!(*payload.downcast_ref::<&str>().unwrap(), "reboot stop");
        assert_eq!(*events.borrow(), ["receipt", "sysrq", "fallback"]);
    }

    #[test]
    fn without_the_opt_in_the_stop_never_touches_sysrq() {
        let events = std::cell::RefCell::new(Vec::new());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            stop_with(
                FatalStop::Reboot,
                || events.borrow_mut().push("receipt"),
                || panic!("sysrq must not be requested without the opt-in"),
                || {
                    events.borrow_mut().push("fallback");
                    panic!("reboot stop");
                },
            );
        }));

        let payload = outcome.expect_err("the reboot stop parks PID 1");
        assert_eq!(*payload.downcast_ref::<&str>().unwrap(), "reboot stop");
        assert_eq!(*events.borrow(), ["receipt", "fallback"]);
    }
}
