use esu_platform::{BootMode, Contents, Plan, PlannedFile, Platform, generation::generation, plan};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const ROM_PATH: &str = "roms/android-a.toml";
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let root = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "esu-packages-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn platform() -> Platform {
    Platform {
        metadata_filesystem: "ext4".into(),
        packages: vec!["tiny-espsu".into(), "boot-hal".into()],
        recovery_packages: Vec::new(),
    }
}
fn file(source: &str, destination: &str, kind: &str, mode: &str) -> String {
    format!(
        "\n[[files]]\nsource='{source}'\ndestination='{destination}'\nkind='{kind}'\nmode='{mode}'\n"
    )
}
fn package(id: &str, generation: &str) -> String {
    format!("schema_version=1\ngeneration='{generation}'\nid='{id}'\n")
}
fn fixture(root: &Path) {
    for directory in ["bin", "roms", "modules/boot-hal", "modules/tiny-espsu"] {
        fs::create_dir_all(root.join(directory)).unwrap();
    }
    let executable = std::env::current_exe().unwrap();
    // The real test ELF includes the same retained generation note as shipped
    // binaries; exercise the executable parser, not a source-text assertion.
    for binary in [
        "bin/esud",
        "modules/boot-hal/hal",
        "modules/tiny-espsu/helper",
    ] {
        fs::copy(&executable, root.join(binary)).unwrap();
    }
    fs::write(
        root.join(ROM_PATH),
        format!(
            "schema_version=1\ngeneration='{}'\nid='android-a'\nmanaged=true\n",
            generation()
        ),
    )
    .unwrap();
    let boot = package("boot-hal", generation())
        + &file(
            "hal",
            "android.hardware.boot-service.gblbds",
            "binary",
            "0755",
        )
        + &file("hal.rc", "initrc/boot-gblbds.rc", "initrc", "0644");
    fs::write(root.join("modules/boot-hal/module.toml"), boot).unwrap();
    fs::write(root.join("modules/boot-hal/hal.rc"), "service vendor.boot-qti /vendor/bin/hw/android.hardware.boot-service.qti\n    override\n    class early_hal\n").unwrap();
    let helper =
        package("tiny-espsu", generation()) + &file("helper", "tiny-espsu", "binary", "0755");
    fs::write(root.join("modules/tiny-espsu/module.toml"), helper).unwrap();
}

#[test]
fn plans_are_deterministic_and_bind_generation_to_real_executables() {
    let temp = Temp::new();
    fixture(&temp.0);
    let config = platform();
    let a = plan(&temp.0, &config, generation(), ROM_PATH, BootMode::Managed).unwrap();
    let mut reversed = config;
    reversed.packages.reverse();
    let b = plan(
        &temp.0,
        &reversed,
        generation(),
        ROM_PATH,
        BootMode::Managed,
    )
    .unwrap();
    let destinations = |plan: &Plan| {
        plan.files
            .iter()
            .map(|f| f.destination.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(destinations(&a), destinations(&b));
    let mut sorted = destinations(&a);
    sorted.sort();
    assert_eq!(destinations(&a), sorted);
    assert!(
        plan(
            &temp.0,
            &reversed,
            "wrong-generation",
            ROM_PATH,
            BootMode::Managed
        )
        .is_err()
    );
    fs::write(temp.0.join("modules/tiny-espsu/helper"), b"not an ELF").unwrap();
    assert!(
        plan(
            &temp.0,
            &reversed,
            generation(),
            ROM_PATH,
            BootMode::Managed
        )
        .is_err()
    );
}

#[test]
fn missing_files_and_symlinked_components_fail_before_publication() {
    let temp = Temp::new();
    fixture(&temp.0);
    let hal = temp.0.join("modules/boot-hal/hal");
    fs::remove_file(&hal).unwrap();
    assert!(
        plan(
            &temp.0,
            &platform(),
            generation(),
            ROM_PATH,
            BootMode::Managed
        )
        .is_err()
    );
    symlink(std::env::current_exe().unwrap(), &hal).unwrap();
    assert!(
        plan(
            &temp.0,
            &platform(),
            generation(),
            ROM_PATH,
            BootMode::Managed
        )
        .is_err()
    );
    fs::remove_file(&hal).unwrap();
    fs::copy(std::env::current_exe().unwrap(), &hal).unwrap();
    fs::rename(temp.0.join("modules/boot-hal"), temp.0.join("saved")).unwrap();
    symlink("../saved", temp.0.join("modules/boot-hal")).unwrap();
    assert!(
        plan(
            &temp.0,
            &platform(),
            generation(),
            ROM_PATH,
            BootMode::Managed
        )
        .is_err()
    );
}

#[test]
fn recovery_omits_normal_hal_and_requires_only_explicit_recovery_packages() {
    let temp = Temp::new();
    fixture(&temp.0);
    fs::remove_dir_all(temp.0.join("modules")).unwrap();
    let mut config = platform();
    let recovery = plan(&temp.0, &config, generation(), ROM_PATH, BootMode::Recovery).unwrap();
    assert!(
        !recovery
            .files
            .iter()
            .any(|file| file.destination.starts_with("modules/"))
    );
    let rc = recovery
        .files
        .iter()
        .find(|file| file.destination == "initrc/modules.rc")
        .unwrap();
    let Contents::Generated(rc) = &rc.contents else {
        panic!("generated RC expected")
    };
    assert!(!String::from_utf8_lossy(rc).contains("service "));
    config.recovery_packages.push("repair".into());
    assert!(plan(&temp.0, &config, generation(), ROM_PATH, BootMode::Recovery).is_err());
    fs::create_dir_all(temp.0.join("modules/repair")).unwrap();
    fs::write(
        temp.0.join("modules/repair/recovery.sh"),
        "#!/system/bin/sh\nexit 0\n",
    )
    .unwrap();
    fs::write(
        temp.0.join("modules/repair/module.toml"),
        package("repair", generation()) + &file("recovery.sh", "recovery.sh", "script", "0755"),
    )
    .unwrap();
    assert!(plan(&temp.0, &config, generation(), ROM_PATH, BootMode::Recovery).is_ok());
    config.recovery_packages.push("boot-hal".into());
    assert!(config.validate().is_err());
}

#[test]
fn unmanaged_still_requires_the_exact_daemon_but_never_the_normal_hal() {
    let temp = Temp::new();
    fixture(&temp.0);
    fs::remove_dir_all(temp.0.join("modules")).unwrap();
    let plan = plan(
        &temp.0,
        &platform(),
        generation(),
        ROM_PATH,
        BootMode::Unmanaged,
    )
    .unwrap();
    assert!(plan.files.iter().any(|file| file.destination == "esud"));
    assert!(
        !plan
            .files
            .iter()
            .any(|file| file.destination.starts_with("modules/"))
    );
    fs::remove_file(temp.0.join("bin/esud")).unwrap();
    assert!(
        esu_platform::plan(
            &temp.0,
            &platform(),
            generation(),
            ROM_PATH,
            BootMode::Unmanaged
        )
        .is_err()
    );
}

fn snapshot(value: &str) -> Plan {
    Plan {
        files: vec![
            PlannedFile {
                destination: "esud".into(),
                mode: 0o755,
                contents: Contents::Generated(value.as_bytes().to_vec()),
            },
            PlannedFile {
                destination: "initrc/modules.rc".into(),
                mode: 0o644,
                contents: Contents::Generated(value.as_bytes().to_vec()),
            },
        ],
    }
}

#[test]
fn publication_replaces_one_complete_snapshot_and_rejects_partial_or_symlink_targets() {
    let temp = Temp::new();
    esu_platform::staging::publish(&temp.0, snapshot("first"), false).unwrap();
    fs::create_dir(temp.0.join("esu/log")).unwrap();
    fs::write(temp.0.join("esu/log/boot.log"), b"keep").unwrap();
    esu_platform::staging::publish(&temp.0, snapshot("second"), false).unwrap();
    assert_eq!(fs::read(temp.0.join("esu/esud")).unwrap(), b"second");
    assert_eq!(
        fs::read(temp.0.join("esu/initrc/modules.rc")).unwrap(),
        b"second"
    );
    assert_eq!(fs::read(temp.0.join("esu/log/boot.log")).unwrap(), b"keep");
    assert_eq!(
        fs::metadata(temp.0.join("esu/esud"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o755
    );
    fs::create_dir(temp.0.join(".esu-staging")).unwrap();
    assert!(esu_platform::staging::publish(&temp.0, snapshot("third"), false).is_err());
    assert_eq!(fs::read(temp.0.join("esu/esud")).unwrap(), b"second");
    fs::remove_dir(temp.0.join(".esu-staging")).unwrap();
    fs::rename(temp.0.join("esu"), temp.0.join("saved")).unwrap();
    symlink("saved", temp.0.join("esu")).unwrap();
    assert!(esu_platform::staging::publish(&temp.0, snapshot("third"), false).is_err());
    assert_eq!(fs::read(temp.0.join("saved/esud")).unwrap(), b"second");
}

#[test]
fn post_exchange_retirement_is_cleanup_only_and_never_a_staging_resume() {
    let temp = Temp::new();
    let next = Temp::new();
    esu_platform::staging::publish(&temp.0, snapshot("old"), false).unwrap();
    esu_platform::staging::publish(&next.0, snapshot("live"), false).unwrap();
    fs::rename(next.0.join("esu"), temp.0.join(".esu-staging")).unwrap();
    let parent = fs::File::open(&temp.0).unwrap();
    use std::os::fd::AsRawFd;
    // SAFETY: live directory fd and static terminated names in that directory.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            parent.as_raw_fd(),
            c".esu-staging".as_ptr(),
            parent.as_raw_fd(),
            c"esu".as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    assert_eq!(result, 0);
    fs::rename(temp.0.join(".esu-staging"), temp.0.join(".esu-retired")).unwrap();
    // Crash image after exchange/retirement, before parent fsync or cleanup.
    assert_eq!(fs::read(temp.0.join("esu/esud")).unwrap(), b"live");
    assert_eq!(fs::read(temp.0.join(".esu-retired/esud")).unwrap(), b"old");
    esu_platform::staging::publish(&temp.0, snapshot("next"), false).unwrap();
    assert!(!temp.0.join(".esu-retired").exists());
    assert_eq!(fs::read(temp.0.join("esu/esud")).unwrap(), b"next");

    // A crash during cleanup may leave only the completion marker or an empty
    // retired directory; neither case turns an actual staging tree resumable.
    for marked in [true, false] {
        fs::create_dir(temp.0.join(".esu-retired")).unwrap();
        if marked {
            fs::copy(
                temp.0.join("esu/.esu-complete"),
                temp.0.join(".esu-retired/.esu-complete"),
            )
            .unwrap();
        }
        esu_platform::staging::publish(&temp.0, snapshot("next"), false).unwrap();
        assert!(!temp.0.join(".esu-retired").exists());
    }
    fs::create_dir(temp.0.join(".esu-retired")).unwrap();
    fs::write(temp.0.join(".esu-retired/unknown"), b"do not delete").unwrap();
    assert!(esu_platform::staging::publish(&temp.0, snapshot("bad"), false).is_err());
    assert_eq!(
        fs::read(temp.0.join(".esu-retired/unknown")).unwrap(),
        b"do not delete"
    );
    assert_eq!(fs::read(temp.0.join("esu/esud")).unwrap(), b"next");
}

#[test]
fn cleanup_rejects_symlink_roots_and_uncommitted_live_snapshots() {
    let temp = Temp::new();
    let outside = Temp::new();
    fs::write(outside.0.join("keep"), b"outside").unwrap();
    esu_platform::staging::publish(&temp.0, snapshot("live"), false).unwrap();
    symlink(&outside.0, temp.0.join(".esu-retired")).unwrap();
    assert!(esu_platform::staging::publish(&temp.0, snapshot("bad"), false).is_err());
    assert_eq!(fs::read(outside.0.join("keep")).unwrap(), b"outside");
    assert!(
        fs::symlink_metadata(temp.0.join(".esu-retired"))
            .unwrap()
            .is_symlink()
    );
    fs::remove_file(temp.0.join(".esu-retired")).unwrap();
    fs::remove_file(temp.0.join("esu/.esu-complete")).unwrap();
    assert!(esu_platform::staging::publish(&temp.0, snapshot("bad"), false).is_err());
    assert_eq!(fs::read(temp.0.join("esu/esud")).unwrap(), b"live");
    assert!(!temp.0.join(".esu-staging").exists());
}
