//! Hard-failure classification and the bounded early-boot receipt.
//!
//! A managed boot has no stock-ROM fallback: every init failure stops the
//! Android handoff and is persisted to the ESP as `/espinit/receipts/failure.json`
//! before the platform fatal-boot stop. The receipt fields are stable so that
//! the failure can be classified without parsing human-readable text.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use rustix::mount::{MountFlags, mount_remount};
use serde::Serialize;

use crate::esp;

/// Receipt schema emitted by this loader.
pub const RECEIPT_SCHEMA_VERSION: u32 = 1;

/// Upper bound for the diagnostic string, keeping the receipt bounded.
pub const MAX_DETAIL_BYTES: usize = 512;

/// Upper bound for a component identifier (file, module, or projected name).
pub const MAX_COMPONENT_BYTES: usize = 64;

/// Mount flags used while the ESP is read-only. The same base flag set is used
/// for the bounded read-write receipt window so the remount only toggles
/// `RDONLY` and cannot silently widen the block device's exposure. The ESP is
/// deliberately executable (`NOEXEC` is not set) because the early and recovery
/// scripts run the busybox binary from the ESP payload.
pub const ESP_MOUNT_FLAGS_RW: MountFlags = MountFlags::NOSUID
    .union(MountFlags::NODEV)
    .union(MountFlags::RELATIME);

/// Read-only ESP mount flags, used for normal boot and after the receipt write.
pub const ESP_MOUNT_FLAGS_RO: MountFlags = ESP_MOUNT_FLAGS_RW.union(MountFlags::RDONLY);

/// Early-boot stage that failed. These strings are part of the receipt ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    Configuration,
    Generation,
    Storage,
    ModuleLoad,
    ModuleCheck,
    Projection,
    Handoff,
}

/// A classified init failure. `error` is a stable machine-readable identifier
/// (never user data), while `detail` is a bounded diagnostic string.
#[derive(Debug)]
pub struct Failure {
    pub stage: Stage,
    pub component: Option<String>,
    pub error: &'static str,
    pub detail: String,
}

impl Failure {
    /// Classify a failure with no attributable component.
    pub fn new(stage: Stage, error: &'static str, detail: impl AsRef<str>) -> Self {
        Self {
            stage,
            component: None,
            error,
            detail: bound(detail.as_ref(), MAX_DETAIL_BYTES),
        }
    }

    /// Classify a failure attributable to a file, module, or projected name.
    pub fn at(
        stage: Stage,
        component: Option<&str>,
        error: &'static str,
        detail: impl AsRef<str>,
    ) -> Self {
        Self {
            stage,
            component: component.map(|value| bound(value, MAX_COMPONENT_BYTES)),
            error,
            detail: bound(detail.as_ref(), MAX_DETAIL_BYTES),
        }
    }
}

/// Options needed to persist a receipt: the ESP mount, when it was mounted, and
/// the selected generation, when it validated.
#[derive(Default)]
pub struct ReceiptState {
    pub generation: Option<String>,
    pub esp_mount: Option<esp::Mount>,
}

#[derive(Serialize)]
struct Receipt<'a> {
    schema_version: u32,
    generation: Option<&'a str>,
    stage: Stage,
    component: Option<&'a str>,
    error: &'a str,
    detail: &'a str,
}

/// Persist the failure receipt, report receipt-storage problems to the kernel
/// log, and always attempt to restore the read-only ESP. The ESP is re-attached
/// first when the handoff teardown already detached it, so a failed exec still
/// leaves a receipt. This never panics: the caller remains in the fatal-boot
/// stop path regardless of the outcome.
pub fn record(state: &mut ReceiptState, failure: &Failure) {
    log::error!(
        "espinit early boot failed: stage={:?} component={:?} error={} detail={}",
        failure.stage,
        failure.component.as_deref().unwrap_or("<none>"),
        failure.error,
        failure.detail,
    );

    let Some(mount) = state.esp_mount.as_mut() else {
        log::error!(
            "espinit receipt storage unavailable: ESP was never mounted, failure not persisted"
        );
        return;
    };

    if mount.is_detached()
        && let Err(error) = mount.reattach_for_receipt()
    {
        log::error!(
            "espinit receipt storage failure: stage={:?} error={} detail={}",
            error.stage,
            error.error,
            error.detail,
        );
        return;
    }

    if let Err(error) = write_receipt(mount.path(), state.generation.as_deref(), failure) {
        log::error!("espinit receipt storage failure: {error:#}");
    }
}

/// One bounded read-write remount window: write a temporary file in the
/// receipts directory, fsync it, atomically rename it to `failure.json`, fsync
/// the directory, sync the filesystem, then restore the read-only mount. The
/// previous receipt survives until the rename succeeds.
fn write_receipt(mount: &str, generation: Option<&str>, failure: &Failure) -> Result<()> {
    let receipts = Path::new(mount).join("espinit/receipts");

    let json = serialize_receipt(generation, failure)?;

    mount_writable(mount).context("cannot remount the ESP read-write for the failure receipt")?;

    let result = write_and_replace(&receipts, json.as_bytes());

    let restore = mount_read_only(mount);
    rustix::fs::sync();

    match (result, restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error).context("cannot replace the failure receipt"),
        (Ok(()), Err(error)) => Err(error).context("cannot restore the read-only ESP mount"),
        (Err(error), Err(restore)) => Err(error).context(format!(
            "cannot replace the failure receipt; read-only restore also failed: {restore}"
        )),
    }
}

fn write_and_replace(receipts: &Path, json: &[u8]) -> Result<()> {
    std::fs::create_dir_all(receipts)
        .with_context(|| format!("cannot open receipt directory {}", receipts.display()))?;

    let temporary = receipts.join("failure.json.tmp");
    let target = receipts.join("failure.json");

    {
        let mut file = File::create(&temporary)
            .with_context(|| format!("cannot create {}", temporary.display()))?;
        file.write_all(json)
            .with_context(|| format!("cannot write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("cannot fsync {}", temporary.display()))?;
    }

    std::fs::rename(&temporary, &target)
        .with_context(|| format!("cannot replace {}", target.display()))?;

    let directory = File::open(receipts)
        .with_context(|| format!("cannot open {} for fsync", receipts.display()))?;
    directory
        .sync_all()
        .with_context(|| format!("cannot fsync {}", receipts.display()))?;

    Ok(())
}

/// Remount the ESP writable for the single bounded receipt window.
fn mount_writable(mount: &str) -> Result<()> {
    mount_remount(mount, ESP_MOUNT_FLAGS_RW, "")
        .with_context(|| format!("cannot remount {mount} read-write"))
}

/// Restore the ESP to the read-only mount used during normal boot.
fn mount_read_only(mount: &str) -> Result<()> {
    mount_remount(mount, ESP_MOUNT_FLAGS_RO, "")
        .with_context(|| format!("cannot remount {mount} read-only"))
}

/// Bound a string to a byte budget on a character boundary.
fn bound(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }

    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }

    value[..end].to_owned()
}

/// Serialize the bounded failure receipt exactly as it is written to
/// `espinit/receipts/failure.json`.
fn serialize_receipt(generation: Option<&str>, failure: &Failure) -> Result<String> {
    let receipt = Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        generation,
        stage: failure.stage,
        component: failure.component.as_deref(),
        error: failure.error,
        detail: &failure.detail,
    };
    serde_json::to_string(&receipt).context("cannot serialize the failure receipt")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn consumer_json(generation: Option<&str>, failure: &Failure) -> serde_json::Value {
        serialize_receipt(generation, failure)
            .expect("the failure receipt must serialize")
            .parse()
            .expect("the failure receipt must be valid JSON")
    }

    #[test]
    fn receipt_json_exposes_exactly_the_stable_classification_fields() {
        let failure = Failure::at(
            Stage::Configuration,
            Some("manifest.toml"),
            "RomGenerationMismatch",
            "rom generation a does not match manifest generation b",
        );

        let json = consumer_json(None, &failure);
        assert_eq!(
            json["schema_version"],
            serde_json::json!(RECEIPT_SCHEMA_VERSION)
        );
        assert_eq!(json["stage"], serde_json::json!("configuration"));
        assert_eq!(json["component"], serde_json::json!("manifest.toml"));
        assert_eq!(json["error"], serde_json::json!("RomGenerationMismatch"));
        assert_eq!(
            json["detail"],
            serde_json::json!("rom generation a does not match manifest generation b")
        );
        assert!(
            json["generation"].is_null(),
            "a generation that never validated stays null"
        );

        let mut keys: Vec<&str> = json
            .as_object()
            .expect("the receipt is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "component",
                "detail",
                "error",
                "generation",
                "schema_version",
                "stage",
            ]
        );

        let validated = consumer_json(Some("release-1"), &failure);
        assert_eq!(validated["generation"], serde_json::json!("release-1"));

        let unattributed = consumer_json(
            None,
            &Failure::new(Stage::Storage, "ReceiptStorageUnavailable", "no storage"),
        );
        assert!(unattributed["component"].is_null());
        assert_eq!(unattributed["stage"], serde_json::json!("storage"));
    }

    #[test]
    fn stage_identifiers_are_the_stable_kebab_case_abi() {
        for (stage, expected) in [
            (Stage::Configuration, "configuration"),
            (Stage::Generation, "generation"),
            (Stage::Storage, "storage"),
            (Stage::ModuleLoad, "module-load"),
            (Stage::ModuleCheck, "module-check"),
            (Stage::Projection, "projection"),
            (Stage::Handoff, "handoff"),
        ] {
            assert_eq!(
                serde_json::to_string(&stage).expect("stage serialization"),
                format!("\"{expected}\"")
            );
        }
    }

    #[test]
    fn detail_and_component_are_bounded_on_a_character_boundary() {
        let at_limit = Failure::new(Stage::ModuleLoad, "X", "b".repeat(MAX_DETAIL_BYTES));
        assert_eq!(at_limit.detail.len(), MAX_DETAIL_BYTES);

        let over = Failure::new(Stage::ModuleLoad, "X", "a".repeat(MAX_DETAIL_BYTES + 200));
        assert_eq!(over.detail, "a".repeat(MAX_DETAIL_BYTES));

        // The limit lands inside the two-byte `é`, so the bound must back off to
        // the previous character boundary rather than split or panic on UTF-8.
        let split = Failure::new(
            Stage::Projection,
            "X",
            format!("{}é{}", "a".repeat(MAX_DETAIL_BYTES - 1), "tail"),
        );
        assert_eq!(split.detail, "a".repeat(MAX_DETAIL_BYTES - 1));

        let long_component = Failure::at(
            Stage::Projection,
            Some(&"c".repeat(MAX_COMPONENT_BYTES + 40)),
            "X",
            "d",
        );
        assert_eq!(
            long_component.component.as_deref(),
            Some("c".repeat(MAX_COMPONENT_BYTES).as_str())
        );
    }

    #[test]
    fn a_worst_case_receipt_stays_within_the_bounded_fields() {
        let failure = Failure::at(
            Stage::ModuleCheck,
            Some(&"c".repeat(MAX_COMPONENT_BYTES + 10)),
            "CoreSelfCheckFailed",
            "d".repeat(MAX_DETAIL_BYTES + 10),
        );

        let json = serde_json::to_string(&consumer_json(Some("release-1"), &failure))
            .expect("receipt serialization");
        assert!(
            json.len() <= MAX_DETAIL_BYTES + MAX_COMPONENT_BYTES + 256,
            "receipt grew beyond its bounded fields: {} bytes",
            json.len()
        );
    }

    #[test]
    fn a_failure_without_a_mounted_esp_is_not_persisted_and_does_not_panic() {
        // No ESP means no receipt target and no fallback generation: the call
        // must return without opening the filesystem.
        record(
            &mut ReceiptState::default(),
            &Failure::new(Stage::Storage, "EspMountFailed", "no ESP"),
        );

        let mut state = ReceiptState {
            generation: Some("release-1".to_owned()),
            esp_mount: None,
        };
        record(
            &mut state,
            &Failure::new(Stage::Storage, "EspMountFailed", "no ESP"),
        );
    }

    #[test]
    fn the_receipt_window_only_toggles_the_read_only_bit() {
        assert_eq!(
            ESP_MOUNT_FLAGS_RO,
            ESP_MOUNT_FLAGS_RW.union(MountFlags::RDONLY)
        );
        assert!(
            ESP_MOUNT_FLAGS_RW
                .contains(MountFlags::NOSUID | MountFlags::NODEV | MountFlags::RELATIME),
            "the shared base flags must not be widened"
        );
        assert!(
            !ESP_MOUNT_FLAGS_RW.contains(MountFlags::NOEXEC),
            "the ESP payload busybox must stay executable"
        );
        assert!(!ESP_MOUNT_FLAGS_RW.contains(MountFlags::RDONLY));
        assert!(ESP_MOUNT_FLAGS_RO.contains(MountFlags::RDONLY));
    }
}
