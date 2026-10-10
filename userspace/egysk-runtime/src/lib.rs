//! Shared non-daemon runtime linked by the native product dispatcher and rdinit.
//! Native owns Android event serialization and Magisk's built-in projection.
//! This crate never projects system trees or manufactures post-fs-data events.
pub mod bootstrap;
pub mod context;
pub mod fsutil;
pub mod helper;
pub mod rc;
pub mod scripts;
pub mod store;
pub mod transition;

use anyhow::{Context, Result, bail, ensure};
use context::*;
use std::path::Path;
use std::sync::Mutex;
pub use store::{
    cancel_remove, request_remove, request_remove_all, rollback, set_enabled, stage_install,
};

/// Shared cpio/package parser used by the standalone host builder.
pub fn parse_bootstrap_config(text: &str) -> Result<BootstrapConfig> {
    ensure!(text.len() <= 65536, "bootstrap config exceeds bound");
    let config: BootstrapConfig = toml::from_str(text)?;
    config.validate()?;
    Ok(config)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    EarlyInit,
    Init,
    EarlyFs,
    PostFs,
    PostFsData,
    PostMount,
    Service,
    BootCompleted,
}
impl Stage {
    pub const ALL: [Self; 8] = [
        Self::EarlyInit,
        Self::Init,
        Self::EarlyFs,
        Self::PostFs,
        Self::PostFsData,
        Self::PostMount,
        Self::Service,
        Self::BootCompleted,
    ];
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EarlyInit => "early-init",
            Self::Init => "init",
            Self::EarlyFs => "early-fs",
            Self::PostFs => "post-fs",
            Self::PostFsData => "post-fs-data",
            Self::PostMount => "post-mount",
            Self::Service => "service",
            Self::BootCompleted => "boot-completed",
        }
    }
}
impl std::str::FromStr for Stage {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|stage| stage.as_str() == value)
            .ok_or_else(|| anyhow::anyhow!("unknown runtime stage {value}"))
    }
}
#[derive(Default)]
struct StageProgress {
    completed: Option<Stage>,
}
impl StageProgress {
    fn pending(&self, stage: Stage) -> Result<bool> {
        if self.completed.is_some_and(|last| stage <= last) {
            return Ok(false);
        }
        let expected = Stage::ALL
            .into_iter()
            .find(|next| self.completed.is_none_or(|last| *next > last));
        ensure!(
            expected == Some(stage),
            "out-of-order stage {} (expected {expected:?})",
            stage.as_str()
        );
        Ok(true)
    }
}
struct Runtime {
    descriptor: Descriptor,
    stages: StageProgress,
}
static RUNTIME: Mutex<Option<Runtime>> = Mutex::new(None);

fn property(name: &str, value: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new("/system/bin/setprop");
    scripts::child_signals(&mut command);
    let mut child = command
        .args([name, value])
        .process_group(0)
        .spawn()
        .context("set runtime property")?;
    scripts::wait_until(
        &mut child,
        std::time::Instant::now() + std::time::Duration::from_secs(35),
        false,
    )
    .with_context(|| format!("setprop {name} {value}"))
}

fn disable_module(module: &Module) -> Result<()> {
    property(&format!("{MODULE_PROPERTY_PREFIX}{}.ready", module.id), "0")?;
    for service in &module.services {
        property("ctl.stop", &service.name)?;
    }
    Ok(())
}
/// Validate the reconstructed context and prepare only its selected effective
/// view. Idempotent after success; errors must stop the native managed boot.
/// No /data or Android target-mount dependency; projection is Native's PostFs job.
pub fn prepare() -> Result<Vec<String>> {
    let mut runtime = RUNTIME
        .lock()
        .map_err(|_| anyhow::anyhow!("runtime mutex poisoned"))?;
    prepare_runtime(&mut runtime)?;
    Ok(runtime
        .as_ref()
        .context("missing prepared runtime")?
        .descriptor
        .modules
        .iter()
        .map(|module| module.id.clone())
        .collect())
}

fn prepare_runtime(runtime: &mut Option<Runtime>) -> Result<()> {
    fsutil::root_only()?;
    if runtime.is_some() {
        return Ok(());
    }
    ensure!(
        fsutil::text(&Path::new(ROOT).join("reconstructed"), 16)? == "1",
        "boot context has not been reconstructed"
    );
    let mut descriptor: Descriptor =
        serde_json::from_str(&fsutil::text(Path::new(SOURCE), MAX_DESCRIPTOR)?)?;
    descriptor.validate()?;
    helper::info()?;
    let mut admitted = Vec::new();
    for module in std::mem::take(&mut descriptor.modules) {
        let prepare = (|| -> Result<()> {
            let root = Path::new(MODULES).join(&module.id);
            ensure!(
                fsutil::mounted(&root).context("inspect effective module mount")?,
                "effective module view missing: {}",
                module.id
            );
            store::identify(&root, &module.id).context("validate effective module identity")?;
            if let Some(policy) = fsutil::optional_text(&root.join("sepolicy.rule"), 65536)
                .context("read effective module policy")?
            {
                helper::apply_policy(&policy).context("apply effective module policy")?;
            }
            bootstrap::label_module(&module)
                .context("prepare effective module permissions and labels")?;
            // Persist only after all required preparation succeeded. Policy is
            // boot-local and always applied on a fresh daemon boot context.
            fsutil::atomic(
                &Path::new(STORE)
                    .join("prepared")
                    .join(&module.id)
                    .join(&module.generation),
                b"1",
            )
            .context("persist prepared module generation")?;
            property(&format!("{MODULE_PROPERTY_PREFIX}{}.ready", module.id), "1")?;
            Ok(())
        })();
        match prepare {
            Ok(()) => admitted.push(module),
            Err(error) if module.critical => {
                return Err(error)
                    .with_context(|| format!("prepare critical module {}", module.id));
            }
            Err(error) => {
                disable_module(&module)?;
                log::warn!("reject optional module {}: {error:#}", module.id);
            }
        }
    }
    descriptor.modules = admitted;
    *runtime = Some(Runtime {
        descriptor,
        stages: StageProgress::default(),
    });
    Ok(())
}

pub fn run_stage(stage: Stage) -> Result<()> {
    let mut guard = RUNTIME
        .lock()
        .map_err(|_| anyhow::anyhow!("runtime mutex poisoned"))?;
    prepare_runtime(&mut guard)?;
    let runtime = guard.as_mut().context("missing prepared runtime")?;
    if !runtime.stages.pending(stage)? {
        return Ok(());
    }
    for removed in scripts::dispatch(&mut runtime.descriptor, stage.as_str())? {
        disable_module(&removed)?;
    }
    let mut admitted = Vec::new();
    for module in std::mem::take(&mut runtime.descriptor.modules) {
        let start = (|| -> Result<()> {
            for service in &module.services {
                if service.stage.as_deref() == Some(stage.as_str()) {
                    property("ctl.start", &service.name)?;
                }
            }
            Ok(())
        })();
        match start {
            Ok(()) => admitted.push(module),
            Err(error) if module.critical => {
                return Err(error)
                    .with_context(|| format!("start critical module {} services", module.id));
            }
            Err(error) => {
                disable_module(&module)?;
                log::warn!(
                    "reject optional module {} service start: {error:#}",
                    module.id
                );
            }
        }
    }
    runtime.descriptor.modules = admitted;
    property(&format!("{STAGE_PROPERTY_PREFIX}{}", stage.as_str()), "1")?;
    runtime.stages.completed = Some(stage);
    Ok(())
}

/// Projection consumes the admission-time value, never a second flag inventory.
pub fn skip_mount(id: &str) -> Result<bool> {
    let mut guard = RUNTIME
        .lock()
        .map_err(|_| anyhow::anyhow!("runtime mutex poisoned"))?;
    prepare_runtime(&mut guard)?;
    guard
        .as_ref()
        .context("missing prepared runtime")?
        .descriptor
        .modules
        .iter()
        .find(|module| module.id == id)
        .map(|module| module.skip_mount)
        .ok_or_else(|| anyhow::anyhow!("module {id} is not admitted"))
}

/// Generic fail-closed reboot-and-park. No BCB, ROM selector or root-provider report.
pub fn fatal_boot(message: &str) -> ! {
    use std::io::{IoSlice, Write};
    log::error!("egysk fatal boot: {message}");
    if let Ok(mut file) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = file.write_vectored(&[
            IoSlice::new(b"egysk fatal boot: "),
            IoSlice::new(message.as_bytes()),
            IoSlice::new(b"\n"),
        ]);
    }
    if Path::new(STORE).is_dir() {
        let _ = fsutil::atomic(
            &Path::new(STORE).join("last-failure"),
            message.as_bytes().get(..4096).unwrap_or(message.as_bytes()),
        );
    }
    unsafe {
        libc::sync();
        // Linux reboot(2) ABI; bionic does not export libc's reboot wrapper.
        libc::syscall(
            libc::SYS_reboot,
            0xfee1deadu32,
            0x28121969u32,
            0x01234567u32,
            std::ptr::null::<libc::c_void>(),
        );
    }
    loop {
        std::thread::park_timeout(std::time::Duration::from_secs(3600));
    }
}

/// First-stage classifier ported from egyskinit; bootconfig overrides cmdline per key.
pub fn boot_mode(bootconfig: &str, cmdline: &str, recovery_present: bool) -> BootMode {
    let value = |key: &str| {
        bootconfig
            .lines()
            .find_map(|line| {
                let (k, v) = line.split_once('=')?;
                (k.trim() == key).then(|| v.trim().trim_matches('"'))
            })
            .or_else(|| {
                cmdline.split_whitespace().find_map(|word| {
                    let (k, v) = word.split_once('=')?;
                    (k == key).then_some(v)
                })
            })
    };
    if matches!(
        value("androidboot.mode"),
        Some("recovery" | "fastboot" | "fastbootd")
    ) || value("androidboot.recovery") == Some("1")
    {
        BootMode::Recovery
    } else if value("androidboot.mode") == Some("charger") {
        BootMode::Charger
    } else if recovery_present && value("androidboot.force_normal_boot") != Some("1") {
        BootMode::Recovery
    } else {
        BootMode::Normal
    }
}

/// Validate an init-only reconstruction invocation without exposing it as an
/// alternate daemon. The original PID1 path remains egyskinit::init::run().
pub fn reconstruction_argument(args: &[String]) -> Result<Option<&str>> {
    if args.get(1).map(String::as_str) != Some("--reconstruct") {
        return Ok(None);
    }
    if args.len() != 3 {
        bail!("usage: egyskinit --reconstruct DESCRIPTOR_HEX");
    }
    Ok(Some(&args[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_reject_skips_and_suppress_only_successful_completions() {
        let mut progress = StageProgress::default();
        assert!(progress.pending(Stage::Init).is_err());
        for stage in Stage::ALL {
            assert!(progress.pending(stage).unwrap());
            // Admission is not completion: failed work remains pending.
            assert!(progress.pending(stage).unwrap());
            progress.completed = Some(stage);
            for earlier in Stage::ALL.into_iter().filter(|earlier| *earlier <= stage) {
                assert!(!progress.pending(earlier).unwrap());
            }
        }
    }

    #[test]
    fn root_gate_precedes_even_cached_runtime_operations() {
        // This assertion must also run in the ordinary unprivileged host lane.
        // SAFETY: geteuid takes no pointers and only reads the effective UID.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let descriptor = Descriptor {
            version: 1,
            esp_major: 0,
            esp_minor: 0,
            backing_device: 0,
            config: parse_bootstrap_config(
                r#"
                kmi = "android16-6.12-6"
                [backing]
                kind = "filesystem"
                source = "/offline"
                fs_type = "f2fs"
                mount_at = "/offline"
                helper = "gobbl-runtime"
            "#,
            )
            .unwrap(),
            mode: BootMode::Normal,
            modules: Vec::new(),
        };
        let mut cached = Some(Runtime {
            descriptor,
            stages: StageProgress::default(),
        });
        // No filesystem/descriptor validation is reached for cached state. A
        // misplaced gate after the cache return would make this succeed.
        assert!(prepare_runtime(&mut cached).is_err());
        assert!(crate::helper::info().is_err());
        assert!(crate::bootstrap::reconstruct("invalid").is_err());
    }
}
