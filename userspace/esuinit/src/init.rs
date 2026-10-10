//! PID1 port: vendor/critical LKMs, exact ESP/backing selection, shared module
//! admission, bounded phases, one-pass RC, complete owned-mount teardown.
//! Platform ROM/storage/EFI-variable/BCB orchestration is not linked here.
use anyhow::{Context, Result, ensure};
use esp_runtime::{bootstrap, context::*, fsutil as fsu, helper, rc, scripts, store};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

// Open per record: retaining a /dev/kmsg FD would pin the owned /dev mount at handoff.
struct KernelLog(std::sync::Mutex<Vec<u8>>);
impl log::Log for KernelLog {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        use std::io::Write;
        let Ok(mut record_bytes) = self.0.lock() else {
            return;
        };
        record_bytes.clear();
        let _ = writeln!(
            record_bytes,
            "kernelsu-esp {}: {}",
            record.level(),
            record.args()
        );
        if let Ok(mut file) = fs::OpenOptions::new().write(true).open("/dev/kmsg") {
            // Each device write is a record; write_fmt would split its fields.
            let _ = file.write_all(&record_bytes);
        }
    }
    fn flush(&self) {}
}
static LOGGER: KernelLog = KernelLog(std::sync::Mutex::new(Vec::new()));

fn minimal_mounts() -> Result<Vec<PathBuf>> {
    let mut owned = Vec::new();
    for (path, kind, flags, data) in [
        (
            "/proc",
            "proc",
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            "",
        ),
        (
            "/sys",
            "sysfs",
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            "",
        ),
        ("/dev", "tmpfs", libc::MS_NOSUID, "mode=0755"),
    ] {
        if !fsu::mounted(Path::new(path)).unwrap_or(false) {
            fsu::mount(kind, Path::new(path), kind, flags, data)?;
            owned.push(PathBuf::from(path));
        }
    }
    for (path, major, minor, mode) in [
        ("/dev/null", 1, 3, 0o666),
        ("/dev/kmsg", 1, 11, 0o600),
        ("/dev/random", 1, 8, 0o666),
        ("/dev/urandom", 1, 9, 0o666),
    ] {
        if !Path::new(path).exists() {
            let path = std::ffi::CString::new(path)?;
            ensure!(
                unsafe {
                    libc::mknod(
                        path.as_ptr(),
                        libc::S_IFCHR | mode,
                        libc::makedev(major, minor),
                    )
                } == 0,
                "create bootstrap device: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    log::set_logger(&LOGGER).map_err(|error| anyhow::anyhow!("kernel logger: {error}"))?;
    log::set_max_level(log::LevelFilter::Info);
    Ok(owned)
}
fn load_critical(config: &BootstrapConfig) -> Result<()> {
    let helper = crate::config::ModuleEntry {
        name: "kernelsu_esp".into(),
        path: "/kernelsu-esp.ko".into(),
        params: if config.norc {
            "norc=1".into()
        } else {
            String::new()
        },
    };
    for entry in std::iter::once(&helper).chain(config.critical_modules.iter()) {
        let path = Path::new(&entry.path);
        fsu::regular(path)?;
        if !crate::loader::module_loaded(&entry.name) {
            crate::loader::load_managed_module(path, entry)?;
        }
        ensure!(
            crate::loader::module_loaded(&entry.name),
            "critical module {} not loaded",
            entry.name
        );
        // A loaded module may await configuration (projection's ready means
        // committed, not registered). Its owning stage validates the control
        // protocol/readiness; the generic loader cannot interpret parameters.
    }
    helper::info()?;
    Ok(())
}
fn load_package_modules(module: &Module, kmi: &str) -> Result<()> {
    let root = Path::new(MODULES).join(&module.id).join("kmod");
    let directory = root.join(kmi);
    for path in [&root, &directory] {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("stat package kernel module directory"),
            Ok(meta) => ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "invalid kmod directory"
            ),
        }
    }
    let mut paths = fsu::children(&directory)?
        .into_iter()
        .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("ko"))
        .collect::<Vec<_>>();
    if let Some(order) = fsu::optional_text(&directory.join("modules.load"), 65536)? {
        let mut ordered = Vec::new();
        for name in order
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.starts_with('#'))
        {
            identifier(name)?;
            let index = paths
                .iter()
                .position(|path| path.file_name().and_then(|s| s.to_str()) == Some(name))
                .context("modules.load names missing/duplicate kernel object")?;
            ordered.push(paths.remove(index));
        }
        ordered.extend(paths);
        paths = ordered;
    }
    for path in paths {
        fsu::regular(&path)?;
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("invalid kernel object name")?
            .replace('-', "_");
        let params = fsu::optional_text(&path.with_extension("params"), 4096)?.unwrap_or_default();
        let entry = crate::config::ModuleEntry {
            name,
            path: path.to_string_lossy().into_owned(),
            params,
        };
        if !crate::loader::module_loaded(&entry.name) {
            crate::loader::load_managed_module(&path, &entry)?;
        }
    }
    Ok(())
}

pub fn run() -> Result<()> {
    ensure!(unsafe { libc::getpid() } == 1, "rdinit must be PID 1");
    let mut owned = minimal_mounts()?;
    let bootconfig = fs::read_to_string("/proc/bootconfig").unwrap_or_default();
    let cmdline = fs::read_to_string("/proc/cmdline")?;
    let mode = esp_runtime::boot_mode(
        &bootconfig,
        &cmdline,
        Path::new(crate::scripts::RECOVERY_EXECUTABLE).exists(),
    );
    let initial =
        crate::config::parse_bootstrap_config(&fsu::text(Path::new("/kernelsu-esp.toml"), 65536)?)?;
    crate::loader::load_vendor_modules()?;
    load_critical(&initial)?;
    let (esp_major, esp_minor) = crate::esp::mount_esp(initial.esp_device.as_deref())?;
    owned.push(PathBuf::from(ESP));
    let mut config = crate::config::parse_bootstrap_config(&fsu::text(
        &Path::new(PACKAGE).join("config.toml"),
        65536,
    )?)?;
    ensure!(
        initial.backing == config.backing && initial.kmi == config.kmi,
        "cpio/package backing or KMI mismatch"
    );
    config.safe_mode |= initial.safe_mode
        || cmdline
            .split_whitespace()
            .any(|arg| arg == "kernelsu-esp.safe_mode=1")
        || bootconfig
            .lines()
            .any(|line| line.trim() == "kernelsu-esp.safe_mode = \"1\"");
    config.norc |= initial.norc;
    fsu::mount(
        "kernelsu-esp",
        Path::new(BIN),
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        "mode=0755",
    )?;
    owned.push(PathBuf::from(BIN));
    bootstrap::stage_tools(&config, true)?;
    owned.extend(bootstrap::prepare_backing(&config)?);
    bootstrap::initialize_store()?;
    if !config.safe_mode && mode != BootMode::Charger {
        store::activate_pending(mode, &config.backing.mount_at)?;
    }
    let mut descriptor = Descriptor {
        version: 1,
        esp_major,
        esp_minor,
        backing_device: fs::metadata(STORE)?.dev(),
        modules: store::discover(&config, mode)?,
        config,
        mode,
    };
    owned.extend(store::mount_view(&descriptor.modules)?);
    fsu::mount(
        "kernelsu-esp",
        &Path::new(ROOT).join("rd"),
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        "mode=0755",
    )?;
    owned.push(Path::new(ROOT).join("rd"));
    let before_modules = mount_snapshot()?;
    let mut admitted = Vec::new();
    for module in &descriptor.modules {
        let work = (|| -> Result<()> {
            // Validate all consumers before kernel effects or scripts. One set
            // feeds kmods/policy/RC/scripts and the later native projector.
            let root = Path::new(MODULES).join(&module.id);
            if let Some(policy) = fsu::optional_text(&root.join("sepolicy.rule"), 65536)? {
                helper::compile_policy(&policy)?;
            }
            rc::module_rc(module)?;
            bootstrap::stage_module_files(module)?;
            load_package_modules(module, &descriptor.config.kmi)?;
            Ok(())
        })();
        match work {
            Ok(()) => admitted.push(module.clone()),
            Err(error) if module.critical => {
                return Err(error)
                    .with_context(|| format!("critical module {} preparation", module.id));
            }
            Err(error) => log::warn!("rejecting optional module {}: {error:#}", module.id),
        }
    }
    descriptor.modules = admitted;
    scripts::dispatch(&mut descriptor, "rdinit")?;
    let rc = rc::assemble(&mut descriptor)?;
    bootstrap::persist_descriptor(&descriptor)?;
    helper::set_module_rc(&rc)?;
    // All foreground script groups are gone. First unmount module-created
    // descendants (efivarfs/configfs included), never baseline vendor mounts.
    // Every overlay, state bind, store/bin and minimal mount is strict.
    std::env::set_current_dir("/")?;
    let retained_esp_loop = esp_loop_reference()?;
    let baseline = before_modules
        .iter()
        .map(|(id, _)| *id)
        .collect::<std::collections::BTreeSet<_>>();
    let mut extra = mount_snapshot()?
        .into_iter()
        .filter(|(id, _)| !baseline.contains(id))
        .collect::<Vec<_>>();
    extra.sort_by_key(|(id, path)| {
        (
            std::cmp::Reverse(path.components().count()),
            std::cmp::Reverse(*id),
        )
    });
    for (_, path) in extra {
        fsu::unmount(&path)?;
    }
    for mount in owned.iter().rev() {
        if mount == Path::new(ESP) {
            release_esp(retained_esp_loop)?;
        } else {
            fsu::unmount(mount)?;
        }
    }
    Ok(())
}

fn mount_snapshot() -> Result<Vec<(u64, PathBuf)>> {
    fs::read_to_string("/proc/self/mountinfo")?
        .lines()
        .map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            ensure!(fields.len() >= 6, "invalid mountinfo");
            let id = fields[0].parse().context("invalid mount ID")?;
            let path = fields[4]
                .replace("\\040", " ")
                .replace("\\011", "\t")
                .replace("\\012", "\n")
                .replace("\\134", "\\");
            Ok((id, PathBuf::from(path)))
        })
        .collect()
}

fn esp_loop_reference() -> Result<bool> {
    for entry in fs::read_dir("/sys/block")? {
        let path = entry?.path().join("loop/backing_file");
        match fs::read_to_string(path) {
            Ok(value) if Path::new(value.trim()).starts_with(ESP) => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect retained loop backing"),
        }
    }
    Ok(false)
}

fn release_esp(retained_loop: bool) -> Result<()> {
    match fsu::unmount(Path::new(ESP)) {
        Ok(()) => Ok(()),
        Err(error)
            if retained_loop
                && error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.raw_os_error() == Some(libc::EBUSY)) =>
        {
            // Only raw ESP may retain a mount reference from an expected loop
            // backing file. All effective uppers/work have already unmounted.
            // A final named RW mount is recreated from the carried device.
            log::warn!("detaching owned raw ESP retained by persistent loop backing");
            ensure!(
                unsafe { libc::umount2(c"/dev/esp".as_ptr(), libc::MNT_DETACH) } == 0,
                "release retained raw ESP: {}",
                std::io::Error::last_os_error()
            );
            Ok(())
        }
        Err(error) => Err(error),
    }
}
