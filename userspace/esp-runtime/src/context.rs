//! Wire contract between rdinit, Android reconstruction and the native daemon.
//! `/kernelsu-esp.toml` is the cpio bootstrap configuration. The selected ESP's
//! `kernelsu-esp/config.toml` uses the same schema and must agree on backing/KMI.
//! Descriptor v1 is JSON encoded as lowercase hex in the one-pass init RC. It is
//! not rediscovered after handoff: device, backing, admission, order and package
//! generations are carried verbatim. No ROM identity selects module storage.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const ROOT: &str = "/dev/kernelsu-esp";
pub const ESP: &str = "/dev/esp";
pub const PACKAGE: &str = "/dev/esp/kernelsu-esp";
pub const BIN: &str = "/dev/kernelsu-esp/bin";
pub const STORE: &str = "/dev/kernelsu-esp/store";
pub const MODULES: &str = "/dev/kernelsu-esp/modules";
pub const SOURCE: &str = "/dev/kernelsu-esp/source";
pub const MAX_DESCRIPTOR: usize = 24576;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Backing {
    /// `block` (an explicit physical block node) or `filesystem` (already mounted).
    pub kind: String,
    /// Exactly one source/partition for block; filesystem accepts source only.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub partition: Option<String>,
    pub fs_type: String,
    pub mount_at: String,
    /// Independently staged executable filename, never inside its module view.
    pub helper: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelModule {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub params: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapConfig {
    #[serde(default)]
    pub esp_device: Option<String>,
    /// Exact `<branch>-<generation>`, never a branch-only selector.
    pub kmi: String,
    pub backing: Backing,
    #[serde(default)]
    pub critical_modules: Vec<KernelModule>,
    /// Additional script barriers: stage.sh first, then stage.<phase>.sh in this order.
    #[serde(default)]
    pub phases: BTreeMap<String, Vec<String>>,
    /// Extra package bin files to copy to executable tmpfs (installer/tool inputs).
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub safe_mode: bool,
    #[serde(default)]
    pub norc: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BootMode {
    Normal,
    Recovery,
    Charger,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Owner {
    Esp,
    Local,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSpec {
    pub name: String,
    /// None preserves an explicitly disabled service's dynamic RC triggers.
    pub stage: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Module {
    pub id: String,
    pub generation: String,
    pub owner: Owner,
    pub critical: bool,
    pub skip_mount: bool,
    pub services: Vec<ServiceSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub version: u32,
    pub esp_major: u32,
    pub esp_minor: u32,
    /// Stable filesystem device identity of the helper's owned subtree.
    pub backing_device: u64,
    pub config: BootstrapConfig,
    pub mode: BootMode,
    pub modules: Vec<Module>,
}

pub fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "invalid identifier {value:?}"
    );
    Ok(())
}

pub fn absolute(value: &str) -> Result<()> {
    ensure!(
        value.starts_with('/')
            && value.len() <= 4096
            && !value
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b",:\\\0'\"$;".contains(&b))
            && Path::new(value).components().all(|c| matches!(
                c,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )),
        "unsafe absolute path {value:?}"
    );
    Ok(())
}

impl BootstrapConfig {
    pub fn validate(&self) -> Result<()> {
        identifier(&self.kmi)?;
        ensure!(
            self.kmi
                .rsplit_once('-')
                .is_some_and(|(branch, generation)| !branch.is_empty()
                    && !generation.is_empty()
                    && generation.bytes().all(|b| b.is_ascii_digit())),
            "KMI must include exact generation"
        );
        ensure!(
            matches!(self.backing.kind.as_str(), "block" | "filesystem"),
            "invalid backing kind"
        );
        ensure!(
            self.backing.source.is_some() != self.backing.partition.is_some(),
            "backing requires exactly one source or partition"
        );
        if let Some(source) = &self.backing.source {
            absolute(source)?;
        }
        if let Some(partition) = &self.backing.partition {
            ensure!(
                self.backing.kind == "block",
                "filesystem backing cannot select a partition"
            );
            identifier(partition)?;
        }
        absolute(&self.backing.mount_at)?;
        identifier(&self.backing.fs_type)?;
        identifier(&self.backing.helper)?;
        if let Some(path) = &self.esp_device {
            absolute(path)?;
        }
        for name in &self.tools {
            identifier(name)?;
        }
        for module in &self.critical_modules {
            identifier(&module.name)?;
            absolute(&module.path)?;
            ensure!(!module.params.contains('\0'), "NUL in module parameters");
        }
        for (stage, phases) in &self.phases {
            ensure!(
                stage == "rdinit" || stage.parse::<crate::Stage>().is_ok(),
                "unknown phase stage {stage}"
            );
            let mut seen = BTreeSet::new();
            for phase in phases {
                identifier(phase)?;
                ensure!(seen.insert(phase), "duplicate phase {phase}");
            }
        }
        Ok(())
    }
}

impl Descriptor {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unknown boot descriptor version");
        self.config.validate()?;
        let mut ids = BTreeSet::new();
        for module in &self.modules {
            identifier(&module.id)?;
            ensure!(
                module.generation.len() == 64
                    && module.generation.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid package generation"
            );
            ensure!(ids.insert(&module.id), "duplicate admitted module");
            let mut services = BTreeSet::new();
            for service in &module.services {
                identifier(&service.name)?;
                ensure!(
                    service.name.starts_with(&format!("esp-{}-", module.id))
                        && services.insert(&service.name),
                    "invalid/duplicate owned service"
                );
                if let Some(stage) = &service.stage {
                    stage.parse::<crate::Stage>()?;
                }
            }
        }
        ensure!(
            !self.config.safe_mode || self.modules.is_empty(),
            "safe mode descriptor admits modules"
        );
        Ok(())
    }
    pub fn encode(&self) -> Result<String> {
        self.validate()?;
        let json = serde_json::to_vec(self)?;
        ensure!(
            json.len() <= MAX_DESCRIPTOR,
            "boot descriptor exceeds RC bound"
        );
        let mut encoded = String::with_capacity(json.len() * 2);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in json {
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 15) as usize] as char);
        }
        Ok(encoded)
    }
    pub fn decode(hex: &str) -> Result<Self> {
        ensure!(
            hex.len() <= MAX_DESCRIPTOR * 2 && hex.len().is_multiple_of(2) && hex.is_ascii(),
            "invalid descriptor encoding"
        );
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
            .collect::<Result<Vec<_>, _>>()?;
        let descriptor: Self = serde_json::from_slice(&bytes)?;
        descriptor.validate()?;
        Ok(descriptor)
    }
}
