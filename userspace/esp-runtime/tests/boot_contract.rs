use esp_runtime::{Stage, boot_mode, context::*, fsutil, helper, parse_bootstrap_config, store};
use std::fs;
use std::path::Path;

fn config() -> BootstrapConfig {
    parse_bootstrap_config(
        r#"
kmi = "android16-6.12-202509"
tools = ["gobbl-platform"]
[backing]
kind = "block"
source = "/dev/block/by-name/metadata"
fs_type = "f2fs"
mount_at = "/dev/kernelsu-esp/metadata"
helper = "gobbl-runtime"
[phases]
rdinit = ["identity", "storage", "projection"]
"#,
    )
    .unwrap()
}

#[test]
fn descriptor_roundtrip_retains_exact_selection_and_order() {
    let descriptor = Descriptor {
        version: 1,
        esp_major: 259,
        esp_minor: 5,
        backing_device: 123,
        config: config(),
        mode: BootMode::Normal,
        modules: vec![
            Module {
                id: "second".into(),
                generation: "ab".repeat(32),
                owner: Owner::Esp,
                critical: true,
                skip_mount: false,
                services: vec![ServiceSpec {
                    name: "esp-second-hal".into(),
                    stage: Some("post-fs".into()),
                }],
            },
            Module {
                id: "first".into(),
                generation: "cd".repeat(32),
                owner: Owner::Local,
                critical: false,
                skip_mount: true,
                services: Vec::new(),
            },
        ],
    };
    let decoded = Descriptor::decode(&descriptor.encode().unwrap()).unwrap();
    assert_eq!(decoded.esp_major, 259);
    assert_eq!(decoded.backing_device, 123);
    assert_eq!(decoded.config.backing, descriptor.config.backing);
    assert_eq!(
        decoded
            .modules
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["second", "first"]
    );
    assert_eq!(decoded.modules[1].owner, Owner::Local);
    assert!(decoded.modules[1].skip_mount);
    let mut invalid = descriptor;
    invalid.modules[1].id = "../escape".into();
    assert!(invalid.encode().is_err());
    assert!(Descriptor::decode("0x").is_err());
}

#[test]
fn flags_are_values_not_presence_and_files_override_properties() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("module.prop"),
        "id=test\ncritical=true\nskip_mount=1\n",
    )
    .unwrap();
    store::identify(root.path(), "test").unwrap();
    assert!(store::valued_flag(root.path(), "critical").unwrap());
    assert!(store::valued_flag(root.path(), "skip_mount").unwrap());
    for value in ["", "false", "0", "TRUE", "yes"] {
        fs::write(root.path().join("critical"), value).unwrap();
        assert!(!store::valued_flag(root.path(), "critical").unwrap());
    }
    for value in ["1", "true", "true\n"] {
        fs::write(root.path().join("critical"), value).unwrap();
        assert!(store::valued_flag(root.path(), "critical").unwrap());
    }
    assert!(store::identify(root.path(), "other").is_err());
    fs::write(root.path().join("module.prop"), "id=test\nid=test\n").unwrap();
    assert!(store::identify(root.path(), "test").is_err());
}

#[test]
fn generation_is_content_sensitive_and_safe_links_are_preserved() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("system")).unwrap();
    fs::write(root.path().join("system/file"), "old").unwrap();
    std::os::unix::fs::symlink("file", root.path().join("system/link")).unwrap();
    let old = fsutil::generation(root.path()).unwrap();
    fs::write(root.path().join("system/file"), "new").unwrap();
    let new = fsutil::generation(root.path()).unwrap();
    assert_ne!(old, new);
    fs::write(root.path().join(".esp-generation"), &new).unwrap();
    assert_eq!(fsutil::generation(root.path()).unwrap(), new);
    let destination = tempfile::tempdir().unwrap();
    let target = destination.path().join("copy");
    fsutil::copy_tree(root.path(), &target).unwrap();
    assert_eq!(
        fs::read_link(target.join("system/link")).unwrap(),
        Path::new("file")
    );
    std::os::unix::fs::symlink("../../escape", root.path().join("system/bad")).unwrap();
    assert!(fsutil::generation(root.path()).is_err());
}

#[test]
fn policy_expansion_is_serialized_and_bounded() {
    let (bytes, atoms) = helper::compile_policy(
        "type esp_test domain\nallow esp_test { a b } file { read write }; # comment\n",
    )
    .unwrap();
    assert_eq!(atoms, 5);
    assert_eq!(&bytes[..4], &4u32.to_ne_bytes());
    assert!(helper::compile_policy("allow { } target file read").is_err());
    assert!(helper::compile_policy("unknown a b c d").is_err());
    let set = format!(
        "{{ {} }}",
        (0..100)
            .map(|i| format!("a{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert!(helper::compile_policy(&format!("allow {set} {set} file read")).is_err());
}

#[test]
fn stage_names_and_boot_modes_are_userspace_owned() {
    for stage in Stage::ALL {
        assert_eq!(stage.as_str().parse::<Stage>().unwrap(), stage);
    }
    assert!("postfs".parse::<Stage>().is_err());
    assert_eq!(
        boot_mode(
            "androidboot.mode = \"recovery\"",
            "androidboot.mode=charger",
            false
        ),
        BootMode::Recovery
    );
    assert_eq!(
        boot_mode("", "androidboot.mode=charger", false),
        BootMode::Charger
    );
    assert_eq!(
        boot_mode("androidboot.force_normal_boot = \"1\"", "", true),
        BootMode::Normal
    );
    assert_eq!(boot_mode("", "", true), BootMode::Recovery);
    let mut value = config();
    value.kmi = "android16-6.12".into();
    assert!(value.validate().is_err());
    value = config();
    value.backing.source = Some("/dev/../metadata".into());
    assert!(value.validate().is_err());
    value = config();
    value.backing.partition = Some("metadata".into());
    assert!(value.validate().is_err());
    value.backing.source = None;
    assert!(value.validate().is_ok());
    value.backing.kind = "filesystem".into();
    assert!(value.validate().is_err());
}

#[test]
fn foreground_deadline_kills_and_reaps_the_owned_process_group() {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new("/bin/sh");
    command.args(["-c", "sleep 30"]).process_group(0);
    esp_runtime::scripts::child_signals(&mut command);
    let mut child = command.spawn().unwrap();
    let pid = child.id() as i32;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
    assert!(esp_runtime::scripts::wait_until(&mut child, deadline, true).is_err());
    assert_eq!(
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn foreground_completion_observes_script_failure_before_consumers() {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new("/bin/sh");
    command.args(["-c", "sleep 0.05; exit 23"]).process_group(0);
    let mut child = command.spawn().unwrap();
    let result = esp_runtime::scripts::wait_until(
        &mut child,
        std::time::Instant::now() + std::time::Duration::from_secs(5),
        false,
    );
    assert!(result.unwrap_err().to_string().contains("23"));
    assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(23));
}
