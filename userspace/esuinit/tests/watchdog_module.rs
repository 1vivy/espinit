//! The lab-only watchdog module ships as module.prop plus one init RC, so the
//! payload admission and the RC concatenation must accept exactly that shape.

use esuinit::platform::{module_rc, validate_modules};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Payload(PathBuf);

impl Payload {
    fn new(module_prop: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "esu-watchdog-module-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let module = root.join("modules/watchdog");
        fs::create_dir_all(module.join("initrc")).unwrap();
        fs::write(module.join("module.prop"), module_prop).unwrap();
        fs::write(module.join("initrc/watchdog.rc"), RC).unwrap();
        Self(root)
    }
}

impl Drop for Payload {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The checked-in module RC, byte for byte.
const RC: &str = include_str!("../../../esu/modules/watchdog/initrc/watchdog.rc");

/// The checked-in module identity.
const MODULE_PROP: &str = include_str!("../../../esu/modules/watchdog/module.prop");

#[test]
fn the_shipped_watchdog_module_is_admitted_and_concatenated() {
    assert!(MODULE_PROP.lines().any(|line| line == "id=watchdog"));
    assert!(RC.ends_with('\n') && !RC.contains('\0'));

    let payload = Payload::new(MODULE_PROP);
    let mut order = vec!["watchdog".to_owned()];

    validate_modules(&payload.0, &mut order, false).unwrap();
    assert_eq!(order, ["watchdog"]);

    let expected = format!("# === watchdog/initrc/watchdog.rc ===\n{RC}");
    assert_eq!(
        module_rc(&payload.0, &order, false).unwrap(),
        expected.as_bytes()
    );

    // Recovery admits only modules carrying `recovery-ok`, which this lab-only
    // module does not ship.
    assert!(module_rc(&payload.0, &order, true).unwrap().is_empty());
}

#[test]
fn a_module_prop_id_mismatch_is_rejected_not_concatenated() {
    let payload = Payload::new("id=watchdog-other\nname=Lab boot watchdog\n");
    let mut order = vec!["watchdog".to_owned()];

    validate_modules(&payload.0, &mut order, false).unwrap();
    assert!(order.is_empty());
    assert!(module_rc(&payload.0, &order, false).unwrap().is_empty());
}
