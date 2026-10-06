use crate::receipt::{Failure, Stage};
use std::path::Path;
use std::time::Duration;

pub(crate) const KEY: &str = "androidboot.esu.probe";
/// The longer delay a reached checkpoint leaves behind, so its reset is
/// distinguishable from the profile's own `panic=N`.
pub(crate) const PANIC_DELAY: &[u8] = b"30";
const HANDOFF_DELAY_SECONDS: u64 = 2;
const ARM_SETTLE_TIME: Duration = Duration::from_millis(100);

/// Fixed, bootconfig-only lab checkpoints in boot order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbeStage {
    Handoff,
    HandoffDelayed,
    ProcMounted,
    SysMounted,
    DevMounted,
    MinimalMounted,
    ApssLoaded,
    EspRetained,
    VendorLoaded,
    EspReady,
    ManifestRead,
    PayloadLoaded,
    ModuleRcPublished,
}

impl ProbeStage {
    pub(crate) fn parse(bootconfig: &str) -> Result<Option<Self>, Failure> {
        let mut values = bootconfig.lines().filter_map(|line| {
            let (name, value) = line.split_once('=').unwrap_or((line, ""));
            (name.trim() == KEY).then(|| unquote(value.trim()))
        });
        let value = values.next();
        if values.next().is_some() {
            return Err(Self::invalid());
        }
        match value {
            None | Some("") => Ok(None),
            Some("handoff") => Ok(Some(Self::Handoff)),
            Some("handoff-delayed") => Ok(Some(Self::HandoffDelayed)),
            Some("proc-mounted") => Ok(Some(Self::ProcMounted)),
            Some("sys-mounted") => Ok(Some(Self::SysMounted)),
            Some("dev-mounted") => Ok(Some(Self::DevMounted)),
            Some("minimal-mounted") => Ok(Some(Self::MinimalMounted)),
            Some("apss-loaded") => Ok(Some(Self::ApssLoaded)),
            Some("esp-retained") => Ok(Some(Self::EspRetained)),
            Some("vendor-loaded") => Ok(Some(Self::VendorLoaded)),
            Some("esp-ready") => Ok(Some(Self::EspReady)),
            Some("manifest-read") => Ok(Some(Self::ManifestRead)),
            Some("payload-loaded") => Ok(Some(Self::PayloadLoaded)),
            Some("module-rc-published") => Ok(Some(Self::ModuleRcPublished)),
            Some(_) => Err(Self::invalid()),
        }
    }

    fn invalid() -> Failure {
        Failure::new(
            Stage::Configuration,
            "InvalidProbeStage",
            "esu probe requires one exact supported checkpoint",
        )
    }
}

/// Arm the APSS-minidump probe only after module RC publication completes.
pub(crate) fn arm_delayed_handoff(
    payload_root: &Path,
    rom: &str,
    rom_number: u32,
) -> Result<(), Failure> {
    let busybox = payload_root.join("bin/busybox");
    let command = format!(
        "exec 3>/proc/sysrq-trigger; ./bin/busybox sleep {HANDOFF_DELAY_SECONDS}; printf c >&3"
    );
    let mut child = std::process::Command::new(&busybox)
        .arg("sh")
        .arg("-c")
        .arg(command)
        .env_clear()
        .env("ESU_ROM", rom)
        .env("ESU_ROM_NUMBER", rom_number.to_string())
        .current_dir(payload_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| {
            Failure::new(
                Stage::Handoff,
                "HandoffProbeSpawn",
                format!("cannot arm delayed post-handoff crash: {error}"),
            )
        })?;
    std::thread::sleep(ARM_SETTLE_TIME);
    if let Some(status) = child.try_wait().map_err(|error| {
        Failure::new(
            Stage::Handoff,
            "HandoffProbeWait",
            format!("cannot inspect delayed post-handoff crash helper: {error}"),
        )
    })? {
        return Err(Failure::new(
            Stage::Handoff,
            "HandoffProbeExited",
            format!("delayed post-handoff crash helper exited early: {status}"),
        ));
    }
    log::warn!("Armed lab post-handoff crash in {HANDOFF_DELAY_SECONDS} seconds");
    Ok(())
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
}
