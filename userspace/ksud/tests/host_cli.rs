#![cfg(target_os = "linux")]

use std::process::Command;

#[test]
fn host_help_exposes_only_artifact_builder() {
    let output = Command::new(env!("CARGO_BIN_EXE_ksud"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("boot-patch"));
    for removed in [
        "boot-restore",
        "resetprop",
        "sepolicy",
        "unload",
        "soft-reboot",
    ] {
        assert!(!help.contains(removed));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_ksud"))
        .args(["boot-patch", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for required in ["--espinit", "--payload", "--rom", "--out", "--boot"] {
        assert!(help.contains(required));
    }
    for removed in [
        "--flash",
        "--ota",
        "--backup",
        "--kernel",
        "--allow-shell",
        "--enable-adbd",
        "--partition",
        "--kmi",
    ] {
        assert!(!help.contains(removed));
    }
}

#[test]
fn removed_actions_and_implicit_inputs_are_rejected() {
    for args in [
        vec!["boot-restore"],
        vec!["boot-patch"],
        vec!["boot-patch", "--flash"],
        vec!["boot-patch", "--ota"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ksud"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
}
