// SPDX-License-Identifier: GPL-3.0-only
//! The KMI admission contract: which module sets are accepted, which are refused
//! and what the refusal says.

use ota_core::kmi::Kmi;
use ota_core::modules::{SCHEMA_VERSION, select_module_set};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn kmi() -> Kmi {
    Kmi {
        branch: "android16-6.12".into(),
        generation: 6,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Write one set under `payload_root/esu/kmi/<dir>/`.
struct Set {
    dir: tempfile::TempDir,
    payload: PathBuf,
}

impl Set {
    fn new(dir_name: &str, branch: &str, generation: u32, schema: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("payload");
        let root = payload.join("esu/kmi").join(dir_name);
        std::fs::create_dir_all(root.join("lib")).unwrap();
        let modules = BTreeMap::from([("kernelesp.ko", b"module bytes".to_vec())]);
        Self::write(&root, branch, generation, schema, &modules);
        Self { dir, payload }
    }

    fn write(
        root: &Path,
        branch: &str,
        generation: u32,
        schema: u32,
        modules: &BTreeMap<&str, Vec<u8>>,
    ) {
        let mut manifest = format!(
            r#"{{"schema_version":{schema},"kmi":{{"branch":"{branch}","generation":{generation}}},"modules":{{"#,
        );
        let mut digests = Vec::new();
        for (name, bytes) in modules {
            std::fs::write(root.join("lib").join(name), bytes).unwrap();
            digests.push(format!("\"{name}\":\"{}\"", hex(&Sha256::digest(bytes))));
        }
        manifest.push_str(&digests.join(","));
        manifest.push_str("}}");
        std::fs::write(root.join("set.json"), manifest).unwrap();
    }

    fn root(&self) -> PathBuf {
        self.dir
            .path()
            .join("payload/esu/kmi")
            .join("android16-6.12-6")
    }

    fn manifest(&self, text: &str) {
        std::fs::write(self.root().join("set.json"), text).unwrap();
    }
}

#[test]
fn a_matching_set_is_selected_with_its_modules_verified() {
    let set = Set::new("android16-6.12-6", "android16-6.12", 6, SCHEMA_VERSION);
    let selected = select_module_set(&set.payload, &kmi()).unwrap();

    assert_eq!(selected.root(), set.root().as_path());
    assert_eq!(selected.kmi(), &kmi());
    assert_eq!(selected.modules().len(), 1);
    assert_eq!(selected.modules()[0].name, "kernelesp.ko");
    assert_eq!(
        selected.modules()[0].path,
        set.root().join("lib/kernelesp.ko")
    );
    assert_eq!(
        selected.modules()[0].sha256,
        hex(&Sha256::digest(b"module bytes"))
    );

    let members = selected.payload_members().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members["lib/kernelesp.ko"], b"module bytes");
}

#[test]
fn a_missing_set_is_the_documented_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let error = select_module_set(dir.path(), &kmi()).unwrap_err();
    assert_eq!(error.to_string(), "no module set for KMI android16-6.12-6");

    // A directory without a manifest is not a set either.
    let set = Set::new("android16-6.12-6", "android16-6.12", 6, SCHEMA_VERSION);
    std::fs::remove_file(set.root().join("set.json")).unwrap();
    assert_eq!(
        select_module_set(&set.payload, &kmi())
            .unwrap_err()
            .to_string(),
        "no module set for KMI android16-6.12-6"
    );
}

#[test]
fn a_set_for_another_kmi_is_refused() {
    for (dir_name, branch, generation) in [
        ("android16-6.12-7", "android16-6.12", 7),
        ("android15-6.6-6", "android15-6.6", 6),
    ] {
        let set = Set::new(dir_name, branch, generation, SCHEMA_VERSION);
        let error = select_module_set(&set.payload, &kmi()).unwrap_err();
        assert_eq!(error.to_string(), "no module set for KMI android16-6.12-6");
    }

    // A directory that exists under the right name but declares another KMI is
    // refused with its own message rather than silently accepted.
    let set = Set::new("android16-6.12-6", "android16-6.12", 7, SCHEMA_VERSION);
    let error = select_module_set(&set.payload, &kmi()).unwrap_err();
    assert!(
        error.to_string().contains("is for android16-6.12-7"),
        "{error}"
    );
}

#[test]
fn unknown_schemas_and_shapes_are_refused() {
    let set = Set::new("android16-6.12-6", "android16-6.12", 6, SCHEMA_VERSION);
    set.manifest(r#"{"schema_version":2,"kmi":{"branch":"android16-6.12","generation":6},"modules":{"kernelesp.ko":"00"}}"#);
    let error = select_module_set(&set.payload, &kmi()).unwrap_err();
    assert!(error.to_string().contains("schema_version 2"), "{error}");

    set.manifest(r#"{"schema_version":1,"kmi":{"branch":"android16-6.12","generation":6},"modules":{},"extra":1}"#);
    let error = select_module_set(&set.payload, &kmi()).unwrap_err();
    assert!(error.to_string().contains("parse"), "{error}");

    set.manifest(
        r#"{"schema_version":1,"kmi":{"branch":"android16-6.12","generation":6},"modules":{}}"#,
    );
    let error = select_module_set(&set.payload, &kmi()).unwrap_err();
    assert!(error.to_string().contains("declares no modules"), "{error}");

    set.manifest("not json");
    let error = select_module_set(&set.payload, &kmi()).unwrap_err();
    assert!(error.to_string().contains("parse"), "{error}");
}

#[test]
fn module_names_must_be_safe_and_modules_must_match_their_digest() {
    let set = Set::new("android16-6.12-6", "android16-6.12", 6, SCHEMA_VERSION);
    let good = format!("\"{}\"", hex(&Sha256::digest(b"module bytes")));

    for name in ["../escape.ko", "lib/kernelesp.ko", ".ko", "kernelesp"] {
        set.manifest(&format!(
            r#"{{"schema_version":1,"kmi":{{"branch":"android16-6.12","generation":6}},"modules":{{"{name}":"{good}"}}}}"#
        ));
        assert!(select_module_set(&set.payload, &kmi()).is_err(), "{name}");
    }

    // A declared digest that does not match the file, and a missing file.
    set.manifest(
        r#"{"schema_version":1,"kmi":{"branch":"android16-6.12","generation":6},"modules":{"kernelesp.ko":"00"}}"#,
    );
    let error = select_module_set(&set.payload, &kmi()).unwrap_err();
    assert!(error.to_string().contains("is "), "{error}");

    std::fs::remove_file(set.root().join("lib/kernelesp.ko")).unwrap();
    let error = select_module_set(&set.payload, &kmi()).unwrap_err();
    assert!(error.to_string().contains("read module"), "{error}");

    // An uppercase digest of the right value is still the right value.
    let set = Set::new("android16-6.12-6", "android16-6.12", 6, SCHEMA_VERSION);
    let upper = hex(&Sha256::digest(b"module bytes")).to_ascii_uppercase();
    set.manifest(&format!(
        r#"{{"schema_version":1,"kmi":{{"branch":"android16-6.12","generation":6}},"modules":{{"kernelesp.ko":"{upper}"}}}}"#
    ));
    assert_eq!(
        select_module_set(&set.payload, &kmi()).unwrap().modules()[0].sha256,
        upper.to_ascii_lowercase()
    );
}

#[test]
fn a_changed_module_fails_the_second_verification() {
    let set = Set::new("android16-6.12-6", "android16-6.12", 6, SCHEMA_VERSION);
    let selected = select_module_set(&set.payload, &kmi()).unwrap();
    std::fs::write(set.root().join("lib/kernelesp.ko"), b"other bytes").unwrap();
    let error = selected.payload_members().unwrap_err();
    assert!(error.to_string().contains("not"), "{error}");
}
