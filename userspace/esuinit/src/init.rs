//! esu PID-1 early managed boot.
//!
//! Minimal mounts and logging precede vendor modules, ESP discovery, strict
//! manifest/ROM and identity validation, payload loading, the GPT projection
//! boundary, and real-init handoff. ESP module work is optional unless the
//! module carries the `critical` marker: an optional module's failure is logged
//! and skipped while the remaining modules still run, whereas mandatory
//! manifest kernel modules, identity, and the final required-backend and
//! projection validation stay strict. Core failures persist a receipt and enter
//! the generic reboot-and-park stop path. Explicit recovery passthrough requires
//! both `androidboot.mode=recovery` and
//! `androidboot.esu.recovery_passthrough=true` exactly once in bootconfig and
//! hands off before touching managed payload work.

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

/// Run the early managed boot. This must run as process 1: the entry point
/// refuses to continue otherwise, before any platform side effect. On success
/// the caller may hand off to the real init; every error is classified for the
/// failure receipt.
pub fn run(state: &mut ReceiptState) -> Result<(), Failure> {
    setup_kmsg();
    log::info!("esu early managed boot starting");
    let (mut mounts, bootconfig) = mount_minimal()?;
    if recovery_passthrough_requested(&bootconfig) {
        log::warn!("explicit recovery passthrough requested; skipping all managed payload work");
        return prepare_handoff(state, &mounts);
    }

    // UFS and VFAT are built in on the phone, so the payload ESP can already be
    // available before vendor module preload. Retain that mount when possible:
    // a preload failure can then persist its normal bounded failure receipt.
    // Devices that need modular storage keep the original load-then-wait path.
    state.esp_mount = retain_esp(esp::mount_esp())?;

    loader::load_vendor_modules()?;

    // Identity must be available even when an unmanaged boot has no ESP payload.
    let bdsvars = load_identity_modules()?;
    let identity = read_identity(bdsvars.is_some())?;
    if state.esp_mount.is_none() {
        state.esp_mount = optional_unmanaged_payload(wait_for_esp(), identity.is_some())?;
    }
    if state.esp_mount.is_none() {
        publish_empty_module_rc()?;
        return prepare_handoff(state, &mounts);
    }
    let mount = state.esp_mount.as_mut().ok_or_else(|| {
        Failure::new(
            Stage::Storage,
            "EspMountUnavailable",
            "ESP discovery succeeded without retaining its mount",
        )
    })?;
    let esp_mount = mount.path().to_owned();
    let esp_device = mount.device();
    let payload_root = esp::payload_root(mount.path());

    if identity.is_none()
        && fs::symlink_metadata(payload_root.join("manifest.toml"))
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        publish_empty_module_rc()?;
        return prepare_handoff(state, &mounts);
    }
    let mut manifest = read_manifest(&payload_root)?;
    config::validate_bootstrap(&manifest).map_err(Failure::from)?;
    mount.verify_retained()?;
    esp::stage_executables(&payload_root, esp_device)?;
    mounts.push(esp::EXECUTABLE_ROOT);
    crate::platform::validate_modules(
        &payload_root,
        &mut manifest.modules_order,
        scripts::is_recovery(),
    )?;
    log_build_ids(&payload_root);
    state.build_id = fs::read_to_string("/esu-build-id")
        .ok()
        .map(|id| id.trim_end().to_owned());
    let Some((id, rom_number)) = identity else {
        log::info!("No managed efivarfs identity; handing off without projection");
        crate::platform::publish_module_rc(
            &payload_root,
            &manifest.modules_order,
            esp_device,
            false,
        )?;
        return prepare_handoff(state, &mounts);
    };
    let rom = read_rom(&payload_root, &manifest, &id, rom_number)?;
    config::validate_managed(&manifest, &rom).map_err(Failure::from)?;

    if rom.has_writable_esp_file() {
        // A writable `esp-file:` projection (only a managed ROM >= 2 may have
        // one) needs its preallocated image reachable for writing, so the ESP
        // is remounted read-write before any module runs. Open loop files keep
        // that access across detach; module data gets a read-only bind view.
        esp::make_payload_writable(mount)?;
    }

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

    load_and_check_payload(
        &payload_root,
        &manifest,
        &rom,
        rom_number,
        &esp_mount,
        esp_device,
    )?;
    crate::platform::publish_module_rc(
        &payload_root,
        &manifest.modules_order,
        esp_device,
        rom.has_writable_esp_file(),
    )?;

    log::info!(
        "Early managed boot checks passed; handing off to {}",
        crate::handoff::REAL_INIT
    );

    prepare_handoff(state, &mounts)
}

/// Load helpers before the single GPT projection boundary.
fn load_and_check_payload(
    payload_root: &Path,
    manifest: &Manifest,
    rom: &RomConfig,
    rom_number: u32,
    esp_mount: &str,
    esp_device: (u32, u32),
) -> Result<(), Failure> {
    for entry in manifest.modules.iter().filter(|entry| {
        !matches!(
            entry.name.as_str(),
            "kernelesp" | "efivarfs" | "efivar_store" | "gpt"
        )
    }) {
        let path = loader::resolve_payload_file(payload_root, &entry.path, &entry.name)?;
        if !loader::module_loaded(&entry.name) {
            loader::load_managed_module(&path, entry)?;
        }
        selfcheck::check_module(&entry.name)?;
    }
    for id in &manifest.modules_order {
        scripts::run_module_scripts(payload_root, id, &rom.id, rom_number)?;
    }
    if rom.managed {
        let entry = manifest
            .modules
            .iter()
            .find(|entry| entry.name == "gpt")
            .ok_or_else(|| {
                Failure::new(
                    Stage::Projection,
                    "ProjectionModuleMissing",
                    "managed ROM requires gpt",
                )
            })?;
        esp::verify_single_esp(
            &fs::read_to_string("/proc/self/mountinfo").map_err(|error| {
                Failure::new(Stage::Storage, "EspMountLifecycle", error.to_string())
            })?,
            esp_device,
        )
        .map_err(|detail| Failure::new(Stage::Storage, "EspMountLifecycle", detail))?;
        resolve_backends(rom, esp_mount)?;
        let path = loader::resolve_payload_file(payload_root, &entry.path, &entry.name)?;
        if !loader::module_loaded("gpt") {
            loader::load_managed_module(&path, entry)?;
        }
        apply_projection(rom, rom_number, esp_device)?;
        selfcheck::check_projection_ready("gpt", rom.partition_modes())?;
    }
    Ok(())
}

/// The identity bootstrap cannot depend on an ESP manifest: direct boot may have
/// none. These two fixed kernel modules are always supplied by the cpio.
fn load_identity_modules() -> Result<Option<u64>, Failure> {
    let bdsvars = classify_bdsvars(esu_platform::block::partition_by_name("bdsvars"))?;
    for name in ["kernelesp", "efivarfs", "efivar_store"] {
        if name != "kernelesp" && bdsvars.is_none() {
            continue;
        }
        let entry = config::ModuleEntry {
            name: name.into(),
            path: format!("lib/{name}.ko"),
            params: if name == "efivar_store" {
                let device = bdsvars.expect("backend requires a discovered bdsvars partition");
                format!(
                    "dev={}:{}",
                    rustix::fs::major(device),
                    rustix::fs::minor(device)
                )
            } else {
                String::new()
            },
        };
        if !loader::module_loaded(name) {
            let path = loader::resolve_payload_file(Path::new("/"), &entry.path, name)?;
            loader::load_managed_module(&path, &entry)?;
        }
        if name == "kernelesp" {
            selfcheck::check_core()?;
            crate::platform::select_core_boot_mode()?;
        }
    }
    Ok(bdsvars)
}

fn classify_bdsvars(device: std::io::Result<u64>) -> Result<Option<u64>, Failure> {
    match device {
        Ok(device) => Ok(Some(device)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Failure::new(
            Stage::Storage,
            "BdsvarsDiscoveryFailed",
            error.to_string(),
        )),
    }
}

fn publish_empty_module_rc() -> Result<(), Failure> {
    crate::set_module_rc(&[]).map_err(|error| {
        Failure::new(
            Stage::ModuleCheck,
            "ModuleRcIoctlFailed",
            format!("{error:#}"),
        )
    })
}

fn optional_unmanaged_payload<T>(
    payload: Result<T, Failure>,
    managed: bool,
) -> Result<Option<T>, Failure> {
    match payload {
        Ok(payload) => Ok(Some(payload)),
        Err(error) if !managed && esp_enumeration_pending(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_identity(bdsvars_present: bool) -> Result<Option<(String, u32)>, Failure> {
    if !bdsvars_present {
        return Ok(None);
    }
    fs::create_dir_all("/efivars")
        .map_err(|error| Failure::new(Stage::Storage, "EfivarsMountFailed", error.to_string()))?;
    rustix::mount::mount(
        "none",
        "/efivars",
        "efivarfs",
        rustix::mount::MountFlags::NOSUID
            | rustix::mount::MountFlags::NODEV
            | rustix::mount::MountFlags::NOEXEC,
        "",
    )
    .map_err(|error| Failure::new(Stage::Storage, "EfivarsMountFailed", error.to_string()))?;
    let result = read_identity_at(Path::new("/efivars"));
    let unmount = rustix::mount::unmount("/efivars", rustix::mount::UnmountFlags::empty())
        .map_err(|error| Failure::new(Stage::Storage, "EfivarsUnmountFailed", error.to_string()));
    let identity = result?;
    unmount?;
    Ok(identity)
}

fn read_identity_at(root: &Path) -> Result<Option<(String, u32)>, Failure> {
    let Some(id) = esu_platform::efivars::booted_rom(root).map_err(identity_error)? else {
        return Ok(None);
    };
    let number = esu_platform::efivars::rom_number(root, &id).map_err(identity_error)?;
    Ok(Some((id, number)))
}

fn identity_error(error: esu_platform::efivars::Error) -> Failure {
    use esu_platform::efivars::Error;
    let code = match &error {
        Error::RomRecordMissing => "RomRecordMissing",
        Error::RomNumberInvalid => "RomNumberInvalid",
        Error::BootedRomInvalid => "BootedRomInvalid",
        Error::Io(_) => "EfivarsUnreadable",
    };
    Failure::new(Stage::Configuration, code, error.to_string())
}

fn log_build_ids(payload_root: &Path) {
    let ramdisk = fs::read_to_string("/esu-build-id").ok();
    let esp = fs::read_to_string(payload_root.join("build-id")).ok();
    let ramdisk = ramdisk.as_deref().map(str::trim_end);
    let esp = esp.as_deref().map(str::trim_end);
    log::info!("Build IDs: ramdisk={ramdisk:?}, ESP={esp:?}");
    if ramdisk != esp || ramdisk.is_none() {
        log::warn!("Ramdisk and ESP build IDs differ or are absent; continuing");
    }
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

fn read_rom(
    payload_root: &Path,
    manifest: &Manifest,
    id: &str,
    rom_number: u32,
) -> Result<RomConfig, Failure> {
    let path = config::rom_path(manifest, id).map_err(Failure::from)?;
    let text = read_config_file(payload_root, &path, "RomUnreadable")?;
    let rom = config::parse_selected_rom(&text, id).map_err(Failure::from)?;
    config::validate_rom(&rom, rom_number).map_err(Failure::from)?;
    Ok(rom)
}

/// Read a configuration file rooted at the ESP `/esu` subtree, rejecting a
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

/// Apply and verify the complete projection: enumerate only physical
/// partitions whose PARTNAME collides with a projected name, retain the
/// mounted ESP for failure receipts and unrelated stock partitions for normal
/// platform operation, build the exact APPLY payload from the resolved
/// backends, issue APPLY, and require QUERY to report the exact projection.
fn apply_projection(
    rom: &RomConfig,
    rom_number: u32,
    esp_device: (u32, u32),
) -> Result<(), Failure> {
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

    gptctl::project(
        &rom.partitions,
        rom.resolved_backends(),
        &hide,
        rom_number >= 2,
    )
}

/// Mount procfs before reading recovery inputs; retain only mounts created here.
fn mount_minimal() -> Result<(Vec<&'static str>, String), Failure> {
    mount_minimal_with(
        || fs::read_to_string("/proc/bootconfig").unwrap_or_default(),
        ensure_minimal_mount,
        || {
            create_minimal_nodes()?;
            unlimit_kmsg();
            Ok(())
        },
    )
}

fn mount_minimal_with(
    read_bootconfig: impl FnOnce() -> String,
    mut ensure_mount: impl FnMut(&'static str) -> Result<bool, Failure>,
    finish_minimal: impl FnOnce() -> Result<(), Failure>,
) -> Result<(Vec<&'static str>, String), Failure> {
    let mut owned = Vec::with_capacity(3);
    if ensure_mount("/proc")? {
        owned.push("/proc");
    }
    let bootconfig = read_bootconfig();
    for mountpoint in ["/sys", "/dev"] {
        if ensure_mount(mountpoint)? {
            owned.push(mountpoint);
        }
    }
    finish_minimal()?;
    Ok((owned, bootconfig))
}

/// Return whether esu created this mount; existing mounts stay unowned.
fn ensure_minimal_mount(mountpoint: &'static str) -> Result<bool, Failure> {
    let (filesystem, error) = match mountpoint {
        "/proc" => ("proc", "ProcMountFailed"),
        "/sys" => ("sysfs", "SysMountFailed"),
        "/dev" => ("devtmpfs", "DevMountFailed"),
        _ => unreachable!("only the three minimal mounts are requested"),
    };
    if esp::is_mounted(mountpoint).map_err(|detail| Failure::new(Stage::Storage, error, detail))? {
        return Ok(false);
    }
    if let Err(detail) = esp::mount_kernel_fs(filesystem, mountpoint) {
        if mountpoint != "/dev" {
            return Err(Failure::new(Stage::Storage, error, detail));
        }
        esp::mount_kernel_fs("tmpfs", "/dev").map_err(|tmpfs_error| {
            Failure::new(
                Stage::Storage,
                error,
                format!("cannot mount devtmpfs ({detail}) or tmpfs ({tmpfs_error})"),
            )
        })?;
        log::warn!("devtmpfs is unavailable; using an empty tmpfs and explicit device nodes");
    }
    Ok(true)
}

/// Create the two device nodes the minimal mounts must carry; an existing node
/// is left exactly as found.
fn create_minimal_nodes() -> Result<(), Failure> {
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

    Ok(())
}

/// Detach every esuinit-owned mount before Android can overmount its paths.
/// Loop backing files keep the detached ESP alive; a failed exec reattaches it
/// only to persist the failure receipt.
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

/// Recovery-only escape hatch. Neither key alone is sufficient.
const RECOVERY_MODE_KEY: &str = "androidboot.mode";
const RECOVERY_MODE_VALUE: &str = "recovery";
const RECOVERY_PASSTHROUGH_KEY: &str = "androidboot.esu.recovery_passthrough";
const RECOVERY_PASSTHROUGH_VALUE: &str = "true";

/// Retain an already available ESP; retry pending enumeration after vendor load.
fn retain_esp(attempt: Result<esp::Mount, Failure>) -> Result<Option<esp::Mount>, Failure> {
    match attempt {
        Ok(mount) => {
            log::info!("ESP was available before vendor module preload");
            Ok(Some(mount))
        }
        Err(failure) if esp_enumeration_pending(&failure) => Ok(None),
        Err(failure) => Err(failure),
    }
}

/// sysfs `PARTNAME` of the partition carrying the AOSP bootloader message.
const MISC_PARTITION: &str = "misc";

/// Generic fatal stop for a failure that carries no classified [`Failure`]:
/// the caller persists its evidence, then the device stops. Classified PID-1
/// failures use [`fatal_boot_classified`] instead, which also records the
/// one-shot bootloader request.
pub fn fatal_boot(record: impl FnOnce()) -> ! {
    record();
    stop_boot()
}

/// Fatal stop of a classified PID-1 failure: request the one-shot bootloader
/// command through the BCB, persist the receipt, then stop.
///
/// The BCB write runs first because it is the only failure mark that survives a
/// device resetting before or instead of the ESP receipt write, and it routes
/// the next ordinary restart through Surfacer and GBL, where the failure is
/// observable without physical access. Both steps are best effort: neither may
/// block the stop path or panic, and the reboot stays a plain restart, never
/// `RESTART2 bootloader`, which would skip both.
pub fn fatal_boot_classified(failure: &Failure, record: impl FnOnce()) -> ! {
    record_bootloader_request(failure);
    record();
    stop_boot()
}

/// `esu:<stage>:<component>` identifies the failure in the BCB status field.
fn bootloader_cause(failure: &Failure) -> String {
    format!(
        "esu:{}:{}",
        stage_name(failure.stage),
        failure.component.as_deref().unwrap_or("init")
    )
}

/// Receipt-ABI stage name. Pinned to the `Failure` serialization by
/// `bootloader_cause_follows_the_receipt_stage_names`.
const fn stage_name(stage: Stage) -> &'static str {
    match stage {
        Stage::Configuration => "configuration",
        Stage::Storage => "storage",
        Stage::ModuleLoad => "module-load",
        Stage::ModuleCheck => "module-check",
        Stage::Projection => "projection",
        Stage::Handoff => "handoff",
    }
}

/// Best-effort one-shot bootloader request for a classified failure.
///
/// The misc record must be written before Android's `/dev/block/by-name` links
/// exist, so the partition is resolved through sysfs `PARTNAME` and opened
/// through a node esu creates. A missing partition, node or device is logged
/// and never stops the fatal-boot path.
fn record_bootloader_request(failure: &Failure) {
    let cause = bootloader_cause(failure);

    let node = match esp::partition_node(MISC_PARTITION) {
        Ok(node) => node,
        Err(detail) => {
            log::error!("cannot record the bootloader request: {detail}");
            return;
        }
    };

    match esu_platform::bcb::request_bootloader(Path::new(&node), &cause) {
        Ok(()) => log::info!("bootloader request recorded: {cause}"),
        Err(error) => log::error!("cannot record the bootloader request in {node}: {error}"),
    }
}

/// Whether the explicit recovery rescue contract is active.
///
/// Bootconfig is authoritative and each exact key must occur exactly once.
/// Normal boots, command-line-only requests, duplicated keys, malformed quotes,
/// and every non-lowercase value retain the managed path.
fn recovery_passthrough_requested(bootconfig: &str) -> bool {
    bootconfig_has_exactly(bootconfig, RECOVERY_MODE_KEY, RECOVERY_MODE_VALUE)
        && bootconfig_has_exactly(
            bootconfig,
            RECOVERY_PASSTHROUGH_KEY,
            RECOVERY_PASSTHROUGH_VALUE,
        )
}

fn bootconfig_has_exactly(bootconfig: &str, key: &str, expected: &str) -> bool {
    let mut values = bootconfig.lines().filter_map(|line| {
        let (name, value) = line.split_once('=').unwrap_or((line, ""));
        (name.trim() == key).then(|| unquote(value.trim()))
    });
    values.next() == Some(expected) && values.next().is_none()
}

/// Strip exactly one surrounding pair of quotes. A lone quote is malformed and
/// is left in place, so it cannot compare equal to an accepted value.
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
}

/// AOSP init's own opt-in: `androidboot.init_fatal_panic=true` makes a fatal init
/// failure kernel-panic instead of reboot, so the failure leaves a crash trail.
const FATAL_PANIC_KEY: &str = "androidboot.init_fatal_panic";
const FATAL_PANIC_VALUE: &str = "true";

pub fn fatal_panic_requested(bootconfig: &str) -> bool {
    bootconfig_has_exactly(bootconfig, FATAL_PANIC_KEY, FATAL_PANIC_VALUE)
}

/// Enter the fatal-boot stop path: sync, then panic when AOSP's
/// `androidboot.init_fatal_panic=true` asks for it (sysrq `c`, as AOSP init does),
/// otherwise reboot. Never continues normal boot when neither takes effect.
pub fn stop_boot() -> ! {
    rustix::fs::sync();

    let bootconfig = fs::read_to_string("/proc/bootconfig").unwrap_or_default();
    if fatal_panic_requested(&bootconfig) {
        log::error!("fatal early boot: {FATAL_PANIC_KEY}={FATAL_PANIC_VALUE}, panicking");
        if let Err(error) = fs::write("/proc/sysrq-trigger", b"c") {
            log::error!("cannot panic through sysrq, falling back to reboot: {error}");
        }
    }

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
    fn minimal_mount_order_preserves_recovery_inputs_and_owned_mounts() {
        let events = std::cell::RefCell::new(Vec::new());
        let config = "androidboot.mode=recovery\n";
        let (owned, bootconfig) = mount_minimal_with(
            || {
                events.borrow_mut().push("read");
                config.to_owned()
            },
            |mountpoint| {
                events.borrow_mut().push(mountpoint);
                Ok(mountpoint != "/sys")
            },
            || {
                events.borrow_mut().push("nodes");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(owned, ["/proc", "/dev"]);
        assert_eq!(bootconfig, config);
        assert_eq!(*events.borrow(), ["/proc", "read", "/sys", "/dev", "nodes"]);
    }

    #[test]
    fn failed_proc_mount_stops_before_reading_recovery_inputs() {
        let result = mount_minimal_with(
            || panic!("bootconfig read before procfs was mounted"),
            |_| Err(Failure::new(Stage::Storage, "ProcMountFailed", "no proc")),
            || panic!("minimal setup continued"),
        );
        assert_eq!(result.unwrap_err().error, "ProcMountFailed");
    }

    #[test]
    fn sysfs_bdsvars_discovery_reaches_managed_identity_without_by_name_links() {
        use esu_platform::{block as platform_block, efivars};
        let root = std::env::temp_dir().join(format!("esu-bdsvars-sysfs-{}", std::process::id()));
        let partition = root.join("sys/class/block/storage-part");
        let variables = root.join("efivars");
        fs::create_dir_all(&partition).unwrap();
        fs::create_dir(&variables).unwrap();
        fs::write(
            partition.join("uevent"),
            "DEVTYPE=partition\nPARTNAME=bdsvars\n",
        )
        .unwrap();
        fs::write(partition.join("dev"), "259:3\n").unwrap();
        let sources = vec![(
            fs::read_to_string(partition.join("uevent")).unwrap(),
            fs::read_to_string(partition.join("dev")).unwrap(),
        )];
        assert!(!root.join("dev/block/by-name").exists());
        let device = classify_bdsvars(platform_block::partition_in("bdsvars", &sources))
            .unwrap()
            .unwrap();
        assert_eq!(
            (rustix::fs::major(device), rustix::fs::minor(device)),
            (259, 3)
        );
        assert!(
            classify_bdsvars(platform_block::partition_in("missing", &sources))
                .unwrap()
                .is_none()
        );
        let duplicate = vec![sources[0].clone(), sources[0].clone()];
        assert_eq!(
            classify_bdsvars(platform_block::partition_in("bdsvars", &duplicate))
                .unwrap_err()
                .error,
            "BdsvarsDiscoveryFailed"
        );
        // A successfully discovered varstore may still describe native/direct
        // boot, but a selected managed ROM must never lose its Slot failure.
        assert!(read_identity_at(&variables).unwrap().is_none());
        efivars::write(&variables, "BootedRom", 7, b"rom2\0").unwrap();
        assert_eq!(
            read_identity_at(&variables).unwrap_err().error,
            "RomRecordMissing"
        );
        efivars::write(&variables, "Slot-rom2", 7, b"GBS1\0\0\0\0").unwrap();
        assert_eq!(
            read_identity_at(&variables).unwrap_err().error,
            "RomNumberInvalid"
        );
        efivars::write(&variables, "Slot-rom2", 7, b"GBS1\x02\0\0\0").unwrap();
        assert_eq!(
            read_identity_at(&variables).unwrap(),
            Some(("rom2".into(), 2))
        );
        efivars::write(&variables, "BootedRom", 7, b"direct\0").unwrap();
        assert!(read_identity_at(&variables).unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_payload_is_optional_only_without_managed_identity() {
        let missing = || Failure::new(Stage::Storage, "EspNotFound", "absent");
        assert!(
            optional_unmanaged_payload::<()>(Err(missing()), false)
                .unwrap()
                .is_none()
        );
        assert!(optional_unmanaged_payload::<()>(Err(missing()), true).is_err());
        assert_eq!(optional_unmanaged_payload(Ok(42), false).unwrap(), Some(42));
        assert_eq!(optional_unmanaged_payload(Ok(42), true).unwrap(), Some(42));
        let unreadable = Failure::new(Stage::Storage, "EspMountFailed", "I/O");
        assert!(optional_unmanaged_payload::<()>(Err(unreadable), false).is_err());
    }

    #[test]
    fn managed_identity_errors_keep_their_classification() {
        use esu_platform::efivars::Error;
        assert_eq!(
            identity_error(Error::RomRecordMissing).error,
            "RomRecordMissing"
        );
        assert_eq!(
            identity_error(Error::RomNumberInvalid).error,
            "RomNumberInvalid"
        );
    }

    #[test]
    fn recovery_passthrough_requires_two_unique_exact_bootconfig_keys() {
        let enabled = format!(
            "{RECOVERY_MODE_KEY}=\"{RECOVERY_MODE_VALUE}\"\n\
             {RECOVERY_PASSTHROUGH_KEY}={RECOVERY_PASSTHROUGH_VALUE}\n"
        );
        assert!(recovery_passthrough_requested(&enabled));

        for disabled in [
            format!("{RECOVERY_MODE_KEY}=recovery\n"),
            format!("{RECOVERY_PASSTHROUGH_KEY}=true\n"),
            format!("{RECOVERY_MODE_KEY}=normal\n{RECOVERY_PASSTHROUGH_KEY}=true\n"),
            format!("{RECOVERY_MODE_KEY}=recovery\n{RECOVERY_PASSTHROUGH_KEY}=TRUE\n"),
            format!(
                "{RECOVERY_MODE_KEY}=recovery\n{RECOVERY_PASSTHROUGH_KEY}=true\n\
                 {RECOVERY_PASSTHROUGH_KEY}=true\n"
            ),
            format!(
                "{RECOVERY_MODE_KEY}=recovery\n{RECOVERY_MODE_KEY}=recovery\n\
                 {RECOVERY_PASSTHROUGH_KEY}=true\n"
            ),
            format!(
                "vendor.{RECOVERY_MODE_KEY}=recovery\n\
                 {RECOVERY_PASSTHROUGH_KEY}=true\n"
            ),
        ] {
            assert!(!recovery_passthrough_requested(&disabled));
        }
        assert!(!recovery_passthrough_requested(&format!(
            "{RECOVERY_MODE_KEY}=recovery\n"
        ),));
    }

    #[test]
    fn fatal_panic_follows_aosp_bootconfig_exactly_once() {
        assert!(fatal_panic_requested("androidboot.init_fatal_panic=true\n"));
        assert!(fatal_panic_requested("androidboot.init_fatal_panic = \"true\"\n"));
        for off in [
            "",
            "androidboot.init_fatal_panic=false\n",
            "androidboot.init_fatal_panic=TRUE\n",
            "androidboot.init_fatal_panic=true\nandroidboot.init_fatal_panic=true\n",
            "vendor.androidboot.init_fatal_panic=true\n",
        ] {
            assert!(!fatal_panic_requested(off), "{off:?}");
        }
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
        let rom =
            config::parse_rom("schema_version = 1\nid = \"android-a\"\nmanaged = false\n").unwrap();

        resolve_backends(&rom, esp::ESP_MOUNT_POINT).unwrap();
        // A second resolution reuses the published set instead of re-resolving.
        config::validate_backends(&rom, esp::ESP_MOUNT_POINT).unwrap();
        assert!(rom.resolved_backends().is_empty());
    }

    #[test]
    fn pending_esp_enumeration_retains_no_mount() {
        let pending = Failure::new(Stage::Storage, "EspNotFound", "not enumerated");
        assert!(matches!(retain_esp(Err(pending)), Ok(None)));
    }

    #[test]
    fn a_hard_mount_failure_stays_the_callers_failure() {
        let fatal = Failure::new(
            Stage::Storage,
            "EspMountAmbiguous",
            "more than one ESP candidate",
        );
        assert!(!esp_enumeration_pending(&fatal));
        assert!(retain_esp(Err(fatal)).is_err());
    }

    /// The BCB cause carries the stage spelling of the failure receipt, so the
    /// two cannot drift apart silently.
    #[test]
    fn bootloader_cause_follows_the_receipt_stage_names() {
        for (stage, name) in [
            (Stage::Configuration, "configuration"),
            (Stage::Storage, "storage"),
            (Stage::ModuleLoad, "module-load"),
            (Stage::ModuleCheck, "module-check"),
            (Stage::Projection, "projection"),
            (Stage::Handoff, "handoff"),
        ] {
            assert_eq!(
                serde_json::to_value(stage).unwrap(),
                serde_json::json!(name)
            );
            assert_eq!(stage_name(stage), name);
        }

        let component = Failure::at(Stage::Projection, Some("rom1"), "ProjectionFailed", "boom");
        assert_eq!(bootloader_cause(&component), "esu:projection:rom1");
        let bare = Failure::new(Stage::Storage, "EspMount", "boom");
        assert_eq!(bootloader_cause(&bare), "esu:storage:init");
    }

    /// The BCB status field is 32 bytes including its NUL, so a realistic
    /// classified cause must survive the writer intact and leave the rest of
    /// the record alone.
    #[test]
    fn classified_causes_fit_the_bcb_status_field() {
        let failure = Failure::at(Stage::ModuleCheck, Some("boot-hal"), "ModuleRc", "boom");
        let cause = bootloader_cause(&failure);
        assert!(cause.len() < 32, "{cause:?}");

        let temporary = std::env::temp_dir().join(format!("esu-bcb-funnel-{}", std::process::id()));
        fs::write(&temporary, [0xa5u8; 128]).unwrap();
        esu_platform::bcb::request_bootloader(&temporary, &cause).unwrap();
        let mut expected = [0xa5u8; 128];
        expected[..64].fill(0);
        expected[..19].copy_from_slice(b"bootonce-bootloader");
        expected[32..32 + cause.len()].copy_from_slice(cause.as_bytes());
        assert_eq!(fs::read(&temporary).unwrap(), expected);
        fs::remove_file(&temporary).unwrap();
    }
}
