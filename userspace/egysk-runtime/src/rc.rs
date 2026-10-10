//! One-pass RC: stock-tool reconstruction first, product stage controls second,
//! admitted module actions last. The complete descriptor lives in the RC bytes.
use crate::context::*;
use crate::fsutil as fsu;
use anyhow::{Context, Result, ensure};
use std::fmt::Write;
use std::path::Path;

/// Parse once during admission, retaining both the transformed actions and
/// their service schedule. Assembly must not reopen module files after scripts.
pub fn module_rc(module: &mut Module) -> Result<String> {
    let (text, services) =
        module_rc_at(module, &Path::new(MODULES).join(&module.id).join("initrc"))?;
    module.services = services;
    Ok(text)
}
fn module_rc_at(module: &Module, root: &Path) -> Result<(String, Vec<ServiceSpec>)> {
    if !root.exists() {
        return Ok((String::new(), Vec::new()));
    }
    let metadata = std::fs::symlink_metadata(root)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "unsafe initrc directory"
    );
    let mut output = String::new();
    let mut services: Vec<ServiceSpec> = Vec::new();
    let mut starts = std::collections::BTreeMap::<String, crate::Stage>::new();
    for file in fsu::children(root)? {
        if file.extension().and_then(|s| s.to_str()) != Some("rc") {
            continue;
        }
        let text = fsu::text(&file, 32768)?;
        let mut service = false;
        let mut current_stage = None;
        let mut section = String::new();
        let mut action = false;
        let mut commands = 0;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                section.push_str(line);
                section.push('\n');
                continue;
            }
            if line.starts_with("service ") || line.starts_with("on ") {
                if !action || commands > 0 {
                    output.push_str(&section);
                }
                section.clear();
                commands = 0;
            }
            if let Some(header) = line.strip_prefix("service ") {
                let name = header
                    .split_whitespace()
                    .next()
                    .context("missing service name")?;
                identifier(name)?;
                ensure!(
                    name.starts_with(&format!("{SERVICE_PREFIX}{}-", module.id)),
                    "module service must use egysk-{}- namespace",
                    module.id
                );
                ensure!(
                    !services.iter().any(|old| old.name == name),
                    "duplicate module service {name}"
                );
                services.push(ServiceSpec {
                    name: name.to_owned(),
                    stage: Some("post-fs".into()),
                });
                service = true;
                action = false;
                current_stage = None;
                section.push_str(line);
                section.push_str("\n    disabled\n");
            } else if let Some(trigger) = line.strip_prefix("on ") {
                service = false;
                action = true;
                ensure!(
                    !trigger.contains(MODULE_PROPERTY_PREFIX)
                        && !trigger.contains(STAGE_PROPERTY_PREFIX),
                    "module cannot forge runtime gate"
                );
                current_stage = trigger.parse::<crate::Stage>().ok();
                let mut pieces = trigger.splitn(2, " && ");
                let first = pieces.next().unwrap_or("");
                let first = if first.parse::<crate::Stage>().is_ok() {
                    format!("property:{STAGE_PROPERTY_PREFIX}{first}=1")
                } else {
                    first.to_owned()
                };
                section.push_str(&format!("on {first}"));
                if let Some(rest) = pieces.next() {
                    section.push_str(" && ");
                    section.push_str(rest);
                }
                section.push_str(&format!(
                    " && property:{MODULE_PROPERTY_PREFIX}{}.ready=1\n",
                    module.id
                ));
            } else {
                ensure!(
                    line.starts_with(char::is_whitespace),
                    "unsupported module RC top-level directive"
                );
                ensure!(
                    !trimmed.starts_with("setprop egysk.") && !trimmed.starts_with("enable "),
                    "module cannot override readiness gates"
                );
                if service && trimmed == "disabled" {
                    services.last_mut().context("missing service")?.stage = None;
                    continue;
                }
                if service && trimmed.starts_with("class ") {
                    let class = trimmed.split_whitespace().nth(1).unwrap_or("");
                    let specification = services.last_mut().context("missing service")?;
                    if specification.stage.is_some() && matches!(class, "late_start" | "main") {
                        specification.stage = Some("service".into());
                    }
                }
                if let (Some(stage), Some(name)) = (current_stage, trimmed.strip_prefix("start ")) {
                    identifier(name)?;
                    ensure!(
                        name.starts_with(&format!("{SERVICE_PREFIX}{}-", module.id)),
                        "cannot schedule foreign service"
                    );
                    starts
                        .entry(name.to_owned())
                        .and_modify(|old| *old = (*old).min(stage))
                        .or_insert(stage);
                    // Native starts it synchronously before its blocking stage
                    // returns. A property-triggered start could run after fs.
                    continue;
                }
                section.push_str(line);
                section.push('\n');
                commands += 1;
            }
        }
        if !action || commands > 0 {
            output.push_str(&section);
        }
        output.push('\n');
    }
    for (name, stage) in starts {
        let service = services
            .iter_mut()
            .find(|service| service.name == name)
            .context("stage starts undeclared service")?;
        service.stage = Some(stage.as_str().to_owned());
    }
    ensure!(output.len() <= 32768, "module RC exceeds bound");
    Ok((output, services))
}

fn bootstrap_step(
    rc: &mut String,
    name: &str,
    command: std::fmt::Arguments<'_>,
) -> std::fmt::Result {
    write!(
        rc,
        "\nservice egysk.bootstrap-{name} {command}\n    user root\n    group root\n    seclabel u:r:init:s0\n    disabled\n    oneshot\n    timeout_period 120\n    reboot_on_failure reboot\n\non init\n    exec_start egysk.bootstrap-{name}\n"
    )
}

pub fn assemble(
    descriptor: &Descriptor,
    fragments: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<u8>> {
    if descriptor.config.norc {
        return Ok(Vec::new());
    }
    // Optional failures were removed during admission/rdinit dispatch. Only
    // that surviving set contributes RC, in descriptor order, from the snapshot.
    for module in &descriptor.modules {
        ensure!(
            fragments.contains_key(&module.id),
            "missing admitted module RC: {}",
            module.id
        );
    }
    let encoded = descriptor.encode()?;
    // Prefix actions register before stock actions. Android's early-init must
    // first start ueventd/bootstrap APEXes and complete the built-in coldboot
    // wait; reconstruction and logical EarlyInit run at the front of `init`.
    // Explicit service labels keep stock toybox in init, not toolbox.
    let mut rc = String::with_capacity(encoded.len() + 4096);
    bootstrap_step(
        &mut rc,
        "directories",
        format_args!("/system/bin/toybox mkdir -p /dev/block {ESP} {BIN}"),
    )?;
    bootstrap_step(
        &mut rc,
        "device",
        format_args!(
            "/system/bin/toybox mknod -m 0600 {SELECTED_ESP} b {} {}",
            descriptor.esp_major, descriptor.esp_minor
        ),
    )?;
    bootstrap_step(
        &mut rc,
        "esp",
        format_args!(
            "/system/bin/toybox mount -t {} -o {} {SELECTED_ESP} {ESP}",
            ESP_MOUNT.fs_type, ESP_MOUNT.init_options
        ),
    )?;
    bootstrap_step(
        &mut rc,
        "bin",
        format_args!(
            "/system/bin/toybox mount -t {} -o {} {MOUNT_SOURCE} {BIN}",
            EXECUTABLE_MOUNT.fs_type, EXECUTABLE_MOUNT.init_options
        ),
    )?;
    bootstrap_step(
        &mut rc,
        "copy",
        format_args!("/system/bin/toybox cp {PACKAGE}/bin/{LOADER} {BIN}/{LOADER}"),
    )?;
    bootstrap_step(
        &mut rc,
        "mode",
        format_args!("/system/bin/toybox chmod 0755 {BIN}/{LOADER}"),
    )?;
    bootstrap_step(
        &mut rc,
        "label",
        format_args!("/system/bin/toybox chcon {FILE_CONTEXT} {ROOT} {BIN} {BIN}/{LOADER}"),
    )?;
    bootstrap_step(
        &mut rc,
        "reconstruct",
        format_args!("{BIN}/{LOADER} --reconstruct {encoded}"),
    )?;
    // A launch/lookup failure must not fall through into product or stock work.
    // Only reconstruct() publishes this after policy, mounts and labels succeed.
    rc.push_str("    wait_for_prop egysk.bootstrap.ready 1\n");
    for stage in crate::Stage::ALL {
        writeln!(
            rc,
            "\nservice egysk.stage-{} {BIN}/{DAEMON} --egysk-stage {}\n    user root\n    group root\n    seclabel {DOMAIN}\n    disabled\n    oneshot\n    timeout_period 300\n    reboot_on_failure reboot",
            stage.as_str(),
            stage.as_str(),
        )?;
        for event in stage.init_events() {
            writeln!(
                rc,
                "\non {event}\n    exec_start egysk.stage-{}\n    wait_for_prop {STAGE_PROPERTY_PREFIX}{} 1",
                stage.as_str(),
                stage.as_str(),
            )?;
        }
    }
    for module in &descriptor.modules {
        rc.push_str(&fragments[&module.id]);
    }
    ensure!(
        rc.len() <= 65536,
        "complete bootstrap/module RC exceeds 64 KiB"
    );
    Ok(rc.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn service_is_disabled_and_only_started_after_owning_stage() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("service.rc"),
            "service egysk-example-hal /dev/egysk/bin/hal\n    class hal\n    user root\non post-fs\n    write /dev/example 1\n").unwrap();
        let module = Module {
            id: "example".into(),
            generation: "ab".repeat(32),
            owner: Owner::Esp,
            critical: false,
            skip_mount: false,
            services: Vec::new(),
        };
        let (rc, services) = module_rc_at(&module, directory.path()).unwrap();
        assert!(rc.contains("    disabled\n"));
        assert!(rc.contains(
            "on property:egysk.stage.post-fs=1 && property:egysk.module.example.ready=1"
        ));
        assert!(!rc.contains("    start egysk-example-hal"));
        assert_eq!(services[0].name, "egysk-example-hal");
        assert_eq!(services[0].stage.as_deref(), Some("post-fs"));
        std::fs::write(directory.path().join("service.rc"),
            "service egysk-example-watchdog /dev/egysk/bin/watchdog\n    disabled\n    class core\non init\n    start egysk-example-watchdog\n").unwrap();
        let (rc, services) = module_rc_at(&module, directory.path()).unwrap();
        assert_eq!(services[0].stage.as_deref(), Some("init"));
        assert!(!rc.contains("on property:egysk.stage.init"));
        std::fs::write(
            directory.path().join("service.rc"),
            "service foreign-service /system/bin/true\n",
        )
        .unwrap();
        assert!(module_rc_at(&module, directory.path()).is_err());
    }

    #[test]
    fn assembly_uses_admitted_snapshot_and_omits_rejected_modules() {
        let directory = tempfile::tempdir().unwrap();
        let mut module = Module {
            id: "example".into(),
            generation: "ab".repeat(32),
            owner: Owner::Esp,
            critical: true,
            skip_mount: false,
            services: Vec::new(),
        };
        let path = directory.path().join("module.rc");
        std::fs::write(&path,
            "service egysk-example-hal /dev/egysk/bin/hal\n    class hal\non post-fs\n    start egysk-example-hal\n    write /dev/example admitted\n").unwrap();
        let (fragment, services) = module_rc_at(&module, directory.path()).unwrap();
        module.services = services;
        let fragments = std::collections::BTreeMap::from([
            (module.id.clone(), fragment),
            (
                "rejected".into(),
                "on init\n    write /dev/rejected 1\n".into(),
            ),
        ]);
        let mut descriptor = Descriptor {
            version: 1,
            esp_major: 259,
            esp_minor: 7,
            backing_device: 42,
            config: crate::parse_bootstrap_config(
                r#"
                kmi = "android16-6.12-6"
                [backing]
                kind = "block"
                source = "/dev/block/by-name/metadata"
                fs_type = "f2fs"
                mount_at = "/dev/egysk/metadata"
                helper = "gobbl-runtime"
            "#,
            )
            .unwrap(),
            mode: BootMode::Normal,
            modules: vec![module],
        };
        // rdinit scripts may alter the files, but cannot replace the admitted RC.
        std::fs::write(&path, "invalid replacement\n").unwrap();
        let output = String::from_utf8(assemble(&descriptor, &fragments).unwrap()).unwrap();
        assert!(output.contains("write /dev/example admitted"));
        assert!(!output.contains("/dev/rejected"));
        assert!(!output.contains("invalid replacement"));
        assert_eq!(
            descriptor.modules[0].services[0].stage.as_deref(),
            Some("post-fs")
        );
        assert!(
            output.find("--reconstruct").unwrap()
                < output.find("--egysk-stage early-init").unwrap()
        );
        assert!(
            output.find("--egysk-stage post-fs").unwrap()
                < output.find("service egysk-example-hal").unwrap()
        );
        assert!(assemble(&descriptor, &std::collections::BTreeMap::new()).is_err());
        descriptor.config.norc = true;
        assert!(
            assemble(&descriptor, &std::collections::BTreeMap::new())
                .unwrap()
                .is_empty()
        );
    }
}
