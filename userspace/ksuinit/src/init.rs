//! espinit PID-1 early managed boot.
//!
//! The order is fixed by the boot contract: minimal mounts and logging, the
//! opt-in APSS minidump transport preload when
//! `androidboot.espinit.apss_minidump=true` is active, normal vendor module
//! loading, ESP discovery and read-only mount, strict manifest
//! and ROM validation, generation matching, ordered payload module loading with
//! self-checks, the single projection boundary immediately before the `gpt`
//! entry, and finally the real-init handoff. Any managed-boot failure stops the
//! handoff, persists a receipt, and enters the fatal-boot stop path: a reboot by
//! default, or the AOSP sysrq crash when the exact
//! `androidboot.init_fatal_panic=true` opt-in is active. The only fallback is the
//! explicit recovery rescue contract: both `androidboot.mode=recovery` and
//! `androidboot.espinit.recovery_passthrough=true` must occur exactly once in
//! bootconfig, in which case espinit tears down its minimal mounts and hands off
//! before touching the ESP, vendor modules, projection, or platform payload.
//!
//! The lab-only `androidboot.espinit.probe=<stage>` opt-in marks one boundary of
//! this order. Because bootconfig is only reachable through procfs, the minimal
//! setup mounts `/proc` first, then reads and parses the probe once, then mounts
//! `/sys` and `/dev` and creates the device nodes. A boot that reaches the named
//! checkpoint sets a 30-second panic delay and requests the same sysrq crash
//! instead of continuing, so the reset timing names the stage. Normal boots never
//! carry the key and are unchanged.

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
use crate::probe::{
    KEY as PROBE_KEY, PANIC_DELAY as PROBE_PANIC_DELAY, ProbeStage, arm_delayed_handoff,
};
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
    let (mounts, bootconfig, probe) = mount_minimal()?;
    if probe == Some(ProbeStage::Handoff) {
        log::warn!("lab handoff probe requested; skipping managed payload work");
        return prepare_handoff(state, &mounts);
    }
    if recovery_passthrough_requested(&bootconfig) {
        log::warn!("explicit recovery passthrough requested; skipping all managed payload work");
        return prepare_handoff(state, &mounts);
    }

    // Opt-in Qualcomm APSS minidump transport: load its vendor module closure
    // before the ESP is mounted, because a boot that dies this early can only
    // leave evidence through a sink the firmware already owns. Normal boots
    // skip this entirely.
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if apss_minidump_requested(&bootconfig, &cmdline) {
        loader::preload_apss_minidump()?;
    }
    checkpoint(probe, ProbeStage::ApssLoaded);

    // UFS and VFAT are built in on the phone, so the payload ESP can already be
    // available before vendor module preload. Retain that mount when possible:
    // a preload failure can then persist its normal bounded failure receipt.
    // Devices that need modular storage keep the original load-then-wait path.
    state.esp_mount = retain_esp(probe, esp::mount_esp())?;

    loader::load_vendor_modules()?;
    checkpoint(probe, ProbeStage::VendorLoaded);

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
    checkpoint(probe, ProbeStage::EspReady);
    let esp_mount = mount.path().to_owned();
    let esp_device = mount.device();
    let payload_root = esp::payload_root(mount.path());

    let manifest = read_manifest(&payload_root)?;
    checkpoint(probe, ProbeStage::ManifestRead);
    let rom = read_rom(&payload_root, &manifest)?;

    check_binary_generation(&manifest)?;
    state.generation = Some(manifest.generation.clone());
    checkpoint(probe, ProbeStage::GenerationMatched);

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
    checkpoint(probe, ProbeStage::PayloadLoaded);
    crate::platform::stage(&payload_root, &manifest, &rom)?;
    checkpoint(probe, ProbeStage::PlatformStaged);
    if probe == Some(ProbeStage::HandoffDelayed) {
        arm_delayed_handoff(&payload_root)?;
    }

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

    gptctl::project(
        &rom.partitions,
        rom.resolved_backends(),
        &hide,
        rom.rom_number >= 2,
    )
}

/// Prepare the minimum early mounts, read the probe once, and retain only the
/// mounts created here.
///
/// `/proc` has to be mounted before the boot configuration can be read at all, so
/// it is ensured first and the probe is parsed from the procfs the same call
/// mounted. The returned boot configuration is that single read, reused for the
/// APSS opt-in instead of reading the file twice.
fn mount_minimal() -> Result<(Vec<&'static str>, String, Option<ProbeStage>), Failure> {
    mount_minimal_with(
        || fs::read_to_string("/proc/bootconfig").unwrap_or_default(),
        ensure_minimal_mount,
        || {
            create_minimal_nodes()?;
            unlimit_kmsg();
            Ok(())
        },
        checkpoint,
    )
}

/// Keep mount/probe ordering shared by production and side-effect-free tests.
fn mount_minimal_with(
    read_bootconfig: impl FnOnce() -> String,
    mut ensure_mount: impl FnMut(&'static str) -> Result<bool, Failure>,
    finish_minimal: impl FnOnce() -> Result<(), Failure>,
    mut reached: impl FnMut(Option<ProbeStage>, ProbeStage),
) -> Result<(Vec<&'static str>, String, Option<ProbeStage>), Failure> {
    let mut owned = Vec::with_capacity(3);
    if ensure_mount("/proc")? {
        owned.push("/proc");
    }
    let bootconfig = read_bootconfig();
    let probe = ProbeStage::parse(&bootconfig)?;
    reached(probe, ProbeStage::ProcMounted);
    for (mountpoint, stage) in [
        ("/sys", ProbeStage::SysMounted),
        ("/dev", ProbeStage::DevMounted),
    ] {
        if ensure_mount(mountpoint)? {
            owned.push(mountpoint);
        }
        reached(probe, stage);
    }
    finish_minimal()?;
    reached(probe, ProbeStage::MinimalMounted);
    Ok((owned, bootconfig, probe))
}

/// Return whether espinit created this mount; existing mounts stay unowned.
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

/// Recovery-only escape hatch. Neither key alone is sufficient.
const RECOVERY_MODE_KEY: &str = "androidboot.mode";
const RECOVERY_MODE_VALUE: &str = "recovery";
const RECOVERY_PASSTHROUGH_KEY: &str = "androidboot.espinit.recovery_passthrough";
const RECOVERY_PASSTHROUGH_VALUE: &str = "true";

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
const PANIC_TIMEOUT: &str = "/proc/sys/kernel/panic";

/// Retain the ESP that was already available before vendor-module preload.
///
/// Only a successful mount is retained, so only that branch reaches the
/// `esp-retained` checkpoint: an enumeration-pending error keeps the original
/// "load modules first, then wait" path with no mount to name, and any other
/// error stays the caller's failure.
fn retain_esp(
    probe: Option<ProbeStage>,
    attempt: Result<esp::Mount, Failure>,
) -> Result<Option<esp::Mount>, Failure> {
    match attempt {
        Ok(mount) => {
            log::info!("ESP was available before vendor module preload");
            checkpoint(probe, ProbeStage::EspRetained);
            Ok(Some(mount))
        }
        Err(failure) if esp_enumeration_pending(&failure) => Ok(None),
        Err(failure) => Err(failure),
    }
}

/// Stop the boot at `reached` when the probe requested exactly that checkpoint.
fn checkpoint(requested: Option<ProbeStage>, reached: ProbeStage) {
    checkpoint_with(
        requested,
        reached,
        || set_panic_timeout_at(Path::new(PANIC_TIMEOUT)),
        crash_kernel,
        || stop_boot(),
    );
}

/// Write the checkpoint's longer panic delay to the kernel's `panic` sysctl.
fn set_panic_timeout_at(path: &Path) -> std::io::Result<()> {
    let mut timeout = fs::File::options().write(true).open(path)?;
    timeout.write_all(PROBE_PANIC_DELAY)
}

/// Run one checkpoint with injectable side effects.
///
/// Only an exact match acts. A matched checkpoint writes the longer panic delay
/// before requesting the crash, so the reset timing separates it from a natural
/// early failure that keeps the profile's own `panic=5`. If either step fails or
/// returns, the unchanged fatal fallback runs and the boot never continues past
/// the checkpoint. No failure receipt is recorded: reaching a checkpoint is not a
/// failure.
fn checkpoint_with(
    requested: Option<ProbeStage>,
    reached: ProbeStage,
    set_timeout: impl FnOnce() -> std::io::Result<()>,
    panic: impl FnOnce(),
    fallback: impl FnOnce(),
) {
    if requested != Some(reached) {
        return;
    }
    log::error!("{PROBE_KEY}: {reached:?} checkpoint reached; requesting a crash");
    let stop = match set_timeout() {
        Ok(()) => FatalStop::Panic,
        Err(error) => {
            log::error!("cannot set {PANIC_TIMEOUT} for checkpoint probe: {error}");
            FatalStop::Reboot
        }
    };
    stop_with(stop, || {}, panic, fallback);
    unreachable!("a matched checkpoint's fatal fallback never returns");
}

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
    bootconfig_value(bootconfig, FATAL_PANIC_KEY)
        .or_else(|| cmdline_value(cmdline, FATAL_PANIC_KEY))
        == Some(FATAL_PANIC_VALUE)
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

/// Lab-only Qualcomm transport opt-in.
///
/// `androidboot.espinit.apss_minidump=true` is read exactly as the fatal-panic
/// opt-in is: the boot configuration wins, the kernel command line is only a
/// fallback while the boot configuration is silent for the key, and only the
/// exact key with the exact lowercase value `true` opts in.
fn apss_minidump_requested(bootconfig: &str, cmdline: &str) -> bool {
    const KEY: &str = "androidboot.espinit.apss_minidump";
    bootconfig_value(bootconfig, KEY).or_else(|| cmdline_value(cmdline, KEY)) == Some("true")
}

/// Value the boot configuration attributes to an exact key, if any.
fn bootconfig_value<'a>(bootconfig: &'a str, key: &str) -> Option<&'a str> {
    bootconfig.lines().find_map(|line| {
        let (name, value) = line.split_once('=').unwrap_or((line, ""));
        (name.trim() == key).then(|| unquote(value.trim()))
    })
}

/// Value the kernel command line attributes to an exact key, if any.
fn cmdline_value<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline.split_whitespace().find_map(|token| {
        let (name, value) = token.split_once('=')?;
        (name == key).then(|| unquote(value))
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
    fn apss_opt_in_is_exact_and_bootconfig_is_authoritative() {
        const KEY: &str = "androidboot.espinit.apss_minidump";
        for value in ["true", "\"true\""] {
            assert!(apss_minidump_requested(&format!("{KEY} = {value}\n"), ""));
            assert!(apss_minidump_requested("", &format!("{KEY}={value}")));
        }
        for value in [
            "", "false", "TRUE", "True", "1", "\"true", "true\"", "truex",
        ] {
            assert!(!apss_minidump_requested(&format!("{KEY} = {value}\n"), ""));
            assert!(!apss_minidump_requested("", &format!("{KEY}={value}")));
            assert!(!apss_minidump_requested(
                &format!("{KEY} = {value}\n"),
                &format!("{KEY}=true"),
            ));
        }
        for key in [
            "androidboot.espinit.apss_minidump_extra",
            "vendor.androidboot.espinit.apss_minidump",
            "ANDROIDBOOT.ESPINIT.APSS_MINIDUMP",
        ] {
            assert!(!apss_minidump_requested(&format!("{key}=true"), ""));
            assert!(!apss_minidump_requested("", &format!("{key}=true")));
        }
        assert!(!apss_minidump_requested("", ""));
        assert!(!apss_minidump_requested(KEY, &format!("{KEY}=true")));
        assert!(apss_minidump_requested(
            "other = false\n",
            &format!("{KEY}=true")
        ));
        assert!(apss_minidump_requested(
            &format!("{KEY}=true"),
            &format!("{KEY}=false")
        ));
    }

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

    #[test]
    fn the_probe_accepts_every_exact_checkpoint() {
        for (value, expected) in [
            ("handoff", ProbeStage::Handoff),
            ("handoff-delayed", ProbeStage::HandoffDelayed),
            ("proc-mounted", ProbeStage::ProcMounted),
            ("sys-mounted", ProbeStage::SysMounted),
            ("dev-mounted", ProbeStage::DevMounted),
            ("minimal-mounted", ProbeStage::MinimalMounted),
            ("apss-loaded", ProbeStage::ApssLoaded),
            ("esp-retained", ProbeStage::EspRetained),
            ("vendor-loaded", ProbeStage::VendorLoaded),
            ("esp-ready", ProbeStage::EspReady),
            ("manifest-read", ProbeStage::ManifestRead),
            ("generation-matched", ProbeStage::GenerationMatched),
            ("payload-loaded", ProbeStage::PayloadLoaded),
            ("platform-staged", ProbeStage::PlatformStaged),
        ] {
            assert!(
                matches!(
                    ProbeStage::parse(&format!("{PROBE_KEY} = {value}\n")),
                    Ok(Some(stage)) if stage == expected
                ),
                "{value} is a supported checkpoint"
            );
            assert!(
                matches!(
                    ProbeStage::parse(&format!("other = 1\n{PROBE_KEY}=\"{value}\"\n")),
                    Ok(Some(stage)) if stage == expected
                ),
                "a quoted {value} still names the checkpoint"
            );
        }
    }

    #[test]
    fn the_minimal_setup_mounts_proc_before_it_reads_the_probe_and_names_each_stage() {
        let events = std::cell::RefCell::new(Vec::new());
        let (owned, bootconfig, probe) = mount_minimal_with(
            || {
                events.borrow_mut().push(String::from("read-bootconfig"));
                format!("{PROBE_KEY}=dev-mounted\n")
            },
            |mountpoint| {
                events.borrow_mut().push(format!("mount {mountpoint}"));
                Ok(true)
            },
            || {
                events
                    .borrow_mut()
                    .push(String::from("create /dev/kmsg and /dev/null"));
                Ok(())
            },
            |_, stage| events.borrow_mut().push(format!("checkpoint {stage:?}")),
        )
        .expect("the minimal setup succeeds");

        assert_eq!(owned, ["/proc", "/sys", "/dev"]);
        assert_eq!(bootconfig, format!("{PROBE_KEY}=dev-mounted\n"));
        assert_eq!(probe, Some(ProbeStage::DevMounted));
        assert_eq!(
            *events.borrow(),
            [
                "mount /proc",
                "read-bootconfig",
                "checkpoint ProcMounted",
                "mount /sys",
                "checkpoint SysMounted",
                "mount /dev",
                "checkpoint DevMounted",
                "create /dev/kmsg and /dev/null",
                "checkpoint MinimalMounted",
            ]
        );
    }

    #[test]
    fn a_pre_existing_mount_is_not_owned_and_does_not_skip_the_probe() {
        let events = std::cell::RefCell::new(Vec::new());
        let (owned, _, probe) = mount_minimal_with(
            String::new,
            |mountpoint| {
                events.borrow_mut().push(format!("mount {mountpoint}"));
                Ok(mountpoint == "/dev")
            },
            || Ok(()),
            |_, _| {},
        )
        .expect("the minimal setup succeeds");

        assert_eq!(owned, ["/dev"]);
        assert_eq!(probe, None);
        assert_eq!(
            *events.borrow(),
            ["mount /proc", "mount /sys", "mount /dev"]
        );
    }

    #[test]
    fn a_failed_proc_mount_stops_before_the_probe_is_read() {
        let events = std::cell::RefCell::new(Vec::new());
        let failure = mount_minimal_with(
            || {
                events.borrow_mut().push(String::from("read-bootconfig"));
                String::new()
            },
            |mountpoint| {
                events.borrow_mut().push(format!("mount {mountpoint}"));
                Err(Failure::new(Stage::Storage, "ProcMountFailed", "no proc"))
            },
            || Ok(()),
            |_, _| events.borrow_mut().push(String::from("checkpoint")),
        )
        .expect_err("a failed proc mount stops the minimal setup");

        assert_eq!(failure.error, "ProcMountFailed");
        assert_eq!(*events.borrow(), ["mount /proc"]);
    }

    #[test]
    fn an_invalid_probe_fails_after_proc_and_before_sysfs() {
        let events = std::cell::RefCell::new(Vec::new());
        let failure = mount_minimal_with(
            || format!("{PROBE_KEY}=not-a-stage"),
            |mountpoint| {
                events.borrow_mut().push(format!("mount {mountpoint}"));
                Ok(true)
            },
            || Ok(()),
            |_, _| events.borrow_mut().push(String::from("checkpoint")),
        )
        .expect_err("an invalid probe value is a bounded failure");

        assert_eq!(failure.stage, Stage::Configuration);
        assert_eq!(failure.error, "InvalidProbeStage");
        assert_eq!(*events.borrow(), ["mount /proc"]);
    }

    #[test]
    fn the_probe_is_disabled_without_the_exact_key_or_value() {
        for bootconfig in [
            String::from(""),
            String::from("other = 1\n"),
            format!("{PROBE_KEY}\n"),
            format!("{PROBE_KEY} =\n"),
            // A prefixed, extended, vendor or case-varied key is a different key:
            // it leaves the probe off instead of arming or failing it.
            format!("{PROBE_KEY}2=minimal-mounted"),
            format!("vendor.{PROBE_KEY}=minimal-mounted"),
            String::from("ANDROIDBOOT.ESPINIT.PROBE=minimal-mounted"),
        ] {
            assert!(
                matches!(ProbeStage::parse(&bootconfig), Ok(None)),
                "{bootconfig:?} leaves the probe off"
            );
        }
    }

    #[test]
    fn the_probe_refuses_unknown_or_malformed_values() {
        for bootconfig in [
            format!("{PROBE_KEY}=minimal_mounted"),
            format!("{PROBE_KEY}=Minimal-Mounted"),
            format!("{PROBE_KEY}=minimal-mounted-extra"),
            format!("{PROBE_KEY}=-minimal-mounted"),
            format!("{PROBE_KEY}=minimal-mounted extra"),
            format!("{PROBE_KEY}=\"minimal-mounted"),
            format!("{PROBE_KEY}=minimal-mounted\""),
            format!("{PROBE_KEY}=minimal-mounted\n{PROBE_KEY}=minimal-mounted"),
        ] {
            let failure = ProbeStage::parse(&bootconfig)
                .expect_err("a non-exact probe must be a bounded failure");
            assert_eq!(failure.stage, Stage::Configuration, "{bootconfig}");
            assert_eq!(failure.error, "InvalidProbeStage", "{bootconfig}");
        }
    }

    #[test]
    fn the_checkpoint_writes_the_longer_panic_delay_in_decimal() {
        let path =
            std::env::temp_dir().join(format!("espinit-probe-panic-sysctl-{}", std::process::id()));
        fs::write(&path, "5").expect("the test sysctl file is writable");
        set_panic_timeout_at(&path).expect("the longer delay can be written");
        assert_eq!(fs::read(&path).expect("the sysctl file is readable"), b"30");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn only_a_retained_mount_reaches_the_esp_retained_checkpoint() {
        let pending = Failure::new(
            Stage::Storage,
            "EspNotFound",
            "the ESP has not been enumerated yet",
        );
        assert!(esp_enumeration_pending(&pending));
        assert!(
            matches!(
                retain_esp(Some(ProbeStage::EspRetained), Err(pending)),
                Ok(None)
            ),
            "a pending enumeration retains no mount, so the checkpoint is not reached"
        );
    }

    #[test]
    fn a_hard_mount_failure_stays_the_callers_failure() {
        let fatal = Failure::new(
            Stage::Storage,
            "EspMountAmbiguous",
            "more than one ESP candidate",
        );
        assert!(!esp_enumeration_pending(&fatal));
        assert!(retain_esp(Some(ProbeStage::EspRetained), Err(fatal)).is_err());
    }

    #[test]
    fn a_checkpoint_that_is_not_reached_is_a_no_op() {
        let touched = std::cell::Cell::new(0);
        checkpoint_with(
            None,
            ProbeStage::MinimalMounted,
            || {
                touched.set(touched.get() + 1);
                Ok(())
            },
            || touched.set(touched.get() + 1),
            || touched.set(touched.get() + 1),
        );
        checkpoint_with(
            Some(ProbeStage::EspReady),
            ProbeStage::MinimalMounted,
            || {
                touched.set(touched.get() + 1);
                Ok(())
            },
            || touched.set(touched.get() + 1),
            || touched.set(touched.get() + 1),
        );
        assert_eq!(touched.get(), 0);
    }

    #[test]
    fn the_panic_timeout_is_written_before_the_crash_request() {
        let events = std::cell::RefCell::new(Vec::new());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            checkpoint_with(
                Some(ProbeStage::MinimalMounted),
                ProbeStage::MinimalMounted,
                || {
                    events.borrow_mut().push("timeout=30");
                    Ok(())
                },
                || {
                    events.borrow_mut().push("sysrq");
                    panic!("kernel panicked");
                },
                || events.borrow_mut().push("fallback"),
            );
        }));

        let payload = outcome.expect_err("a requested kernel panic never returns");
        assert_eq!(*payload.downcast_ref::<&str>().unwrap(), "kernel panicked");
        assert_eq!(*events.borrow(), ["timeout=30", "sysrq"]);
    }

    #[test]
    fn a_timeout_write_that_fails_never_reaches_sysrq() {
        let events = std::cell::RefCell::new(Vec::new());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            checkpoint_with(
                Some(ProbeStage::MinimalMounted),
                ProbeStage::MinimalMounted,
                || Err(std::io::Error::other("read-only panic sysctl")),
                || panic!("sysrq must not be reached when the timeout write failed"),
                || {
                    events.borrow_mut().push("fallback");
                    panic!("reboot stop");
                },
            );
        }));

        let payload = outcome.expect_err("the checkpoint fallback parks PID 1");
        assert_eq!(*payload.downcast_ref::<&str>().unwrap(), "reboot stop");
        assert_eq!(*events.borrow(), ["fallback"]);
    }

    #[test]
    fn a_matched_checkpoint_never_proceeds_after_sysrq_returns() {
        let events = std::cell::RefCell::new(Vec::new());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            checkpoint_with(
                Some(ProbeStage::PlatformStaged),
                ProbeStage::PlatformStaged,
                || {
                    events.borrow_mut().push("timeout=30");
                    Ok(())
                },
                || events.borrow_mut().push("sysrq"),
                || {
                    events.borrow_mut().push("fallback");
                    panic!("reboot stop");
                },
            );
        }));

        let payload = outcome.expect_err("a matched checkpoint never continues boot");
        assert_eq!(*payload.downcast_ref::<&str>().unwrap(), "reboot stop");
        assert_eq!(*events.borrow(), ["timeout=30", "sysrq", "fallback"]);
    }
}
