// SPDX-License-Identifier: GPL-3.0-only
//! KMI-selected kernel module sets.
//!
//! An update's payload tree carries one directory per KMI under `esu/kmi/`, each
//! with a `set.json` manifest and the `lib/<name>.ko` modules it names. The
//! device picks the set that matches the *target* kernel's KMI and refuses the
//! update when there is none: loading a module built for another branch or
//! generation is exactly the failure the generation counter exists to prevent,
//! so a missing set is a denial, never a fallback to the current set.
//!
//! The host writes these sets: `esud boot-patch` places each verified set and
//! writes its `set.json` after `scripts/kmi_modules.py` has checked the modules
//! and emitted their `.ko.compat.json` receipts, so the layout is a
//! cross-repository contract.

use crate::kmi::{self, Kmi};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Directory under the payload root holding every KMI's set.
pub const KMI_DIR: &str = "esu/kmi";
/// Manifest file name inside one set directory.
pub const SET_FILE: &str = "set.json";
/// Module directory inside one set directory.
pub const LIB_DIR: &str = "lib";
/// The only manifest schema version this build understands.
pub const SCHEMA_VERSION: u32 = 1;

/// One verified module of a selected set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Module {
    /// `<name>.ko`, as named by the manifest.
    pub name: String,
    /// Absolute path of `lib/<name>.ko` inside the set.
    pub path: PathBuf,
    /// Lowercase hex SHA256 the manifest declares and the file has.
    pub sha256: String,
}

/// One ROM payload's module set for one KMI, with every module already verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleSet {
    root: PathBuf,
    kmi: Kmi,
    modules: Vec<Module>,
}

impl ModuleSet {
    /// The set directory `esu/kmi/<branch>-<generation>/`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The KMI the set was built for, which is the KMI it was selected with.
    pub fn kmi(&self) -> &Kmi {
        &self.kmi
    }

    /// The verified modules, in manifest order.
    pub fn modules(&self) -> &[Module] {
        &self.modules
    }

    /// The overlay members of this set: `lib/<name>.ko` to bytes, exactly what
    /// [`crate::overlay::build_overlay`] takes.
    ///
    /// Each file is hashed again here. Selection and use are two separate steps
    /// (seal happens after the payload was admitted), and a set that changed in
    /// between must fail the seal rather than be written into a staged payload.
    pub fn payload_members(&self) -> Result<BTreeMap<String, Vec<u8>>> {
        let mut members = BTreeMap::new();
        for module in &self.modules {
            let bytes = std::fs::read(&module.path)
                .with_context(|| format!("read module {}", module.path.display()))?;
            let digest = hex(&Sha256::digest(&bytes));
            ensure!(
                digest == module.sha256,
                "module {} is {digest}, not {}",
                module.path.display(),
                module.sha256
            );
            members.insert(format!("{LIB_DIR}/{}", module.name), bytes);
        }
        Ok(members)
    }
}

/// `payload_root/esu/kmi/<branch>-<generation>/`.
pub fn set_dir(payload_root: &Path, kmi: &Kmi) -> PathBuf {
    payload_root
        .join(KMI_DIR)
        .join(format!("{}-{}", kmi.branch, kmi.generation))
}

/// Select and verify the module set for `kmi` under `payload_root`.
///
/// A missing set is `no module set for KMI <branch>-<generation>`: that string is
/// what the updater reports for a payload built for another kernel. Everything
/// else — an unreadable or unknown-schema manifest, a set for a different KMI, a
/// missing module, a digest mismatch — is a refusal with its own message.
pub fn select_module_set(payload_root: &Path, kmi: &Kmi) -> Result<ModuleSet> {
    kmi::validate(kmi)?;
    let missing = || format!("no module set for KMI {}-{}", kmi.branch, kmi.generation);
    let root = set_dir(payload_root, kmi);
    if !root.is_dir() {
        bail!("{}", missing());
    }
    let manifest_path = root.join(SET_FILE);
    let bytes = match std::fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => bail!("{}", missing()),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", manifest_path.display()));
        }
    };
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse {}", manifest_path.display()))?;
    ensure!(
        manifest.schema_version == SCHEMA_VERSION,
        "{} declares schema_version {}, not {SCHEMA_VERSION}",
        manifest_path.display(),
        manifest.schema_version
    );
    ensure!(
        manifest.kmi.branch == kmi.branch && manifest.kmi.generation == kmi.generation,
        "{} is for {}-{}, not {}-{}",
        manifest_path.display(),
        manifest.kmi.branch,
        manifest.kmi.generation,
        kmi.branch,
        kmi.generation
    );
    ensure!(
        !manifest.modules.is_empty(),
        "{} declares no modules",
        manifest_path.display()
    );

    let mut modules = Vec::with_capacity(manifest.modules.len());
    for (name, declared) in manifest.modules {
        let stem = name
            .strip_suffix(".ko")
            .with_context(|| format!("module {name} does not end in .ko"))?;
        esu_platform::identifier(stem).with_context(|| format!("invalid module name {name}"))?;
        let path = root.join(LIB_DIR).join(&name);
        let bytes =
            std::fs::read(&path).with_context(|| format!("read module {}", path.display()))?;
        let digest = hex(&Sha256::digest(&bytes));
        ensure!(
            digest == declared.to_ascii_lowercase(),
            "module {} is {digest}, not {}",
            path.display(),
            declared
        );
        modules.push(Module {
            name,
            path,
            sha256: digest,
        });
    }
    Ok(ModuleSet {
        root,
        kmi: kmi.clone(),
        modules,
    })
}

/// Lowercase hex of a digest.
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from_digit(u32::from(byte >> 4), 16).expect("nibble"));
        text.push(char::from_digit(u32::from(byte & 0xf), 16).expect("nibble"));
    }
    text
}

/// The `set.json` document. Unknown fields are refused: a newer manifest is not
/// something this build can verify.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    kmi: ManifestKmi,
    modules: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestKmi {
    branch: String,
    generation: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_set_directory_is_one_path_component_pair() {
        let kmi = Kmi {
            branch: "android16-6.12".into(),
            generation: 6,
        };
        assert_eq!(
            set_dir(Path::new("/payload"), &kmi),
            Path::new("/payload/esu/kmi/android16-6.12-6")
        );
    }

    #[test]
    fn digests_are_lowercase_hex() {
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0x00, 0x0f, 0xf0, 0xff]), "000ff0ff");
    }
}
