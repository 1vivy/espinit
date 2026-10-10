//! One-pass RC: stock-tool reconstruction first, product stage controls second,
//! admitted module actions last. The complete descriptor lives in the RC bytes.
use crate::context::*;
use crate::fsutil as fsu;
use anyhow::{Context, Result, ensure};
use std::fmt::Write;
use std::path::Path;

pub fn module_rc(module: &Module) -> Result<String> {
    Ok(module_rc_at(module, &Path::new(MODULES).join(&module.id).join("initrc"))?.0)
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
                    name.starts_with(&format!("esp-{}-", module.id)),
                    "module service must use esp-{}- namespace",
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
                    !trigger.contains("esp.module.") && !trigger.contains("esp.stage."),
                    "module cannot forge runtime gate"
                );
                current_stage = trigger.parse::<crate::Stage>().ok();
                let mut pieces = trigger.splitn(2, " && ");
                let first = pieces.next().unwrap_or("");
                let first = if first.parse::<crate::Stage>().is_ok() {
                    format!("property:esp.stage.{first}=1")
                } else {
                    first.to_owned()
                };
                section.push_str(&format!("on {first}"));
                if let Some(rest) = pieces.next() {
                    section.push_str(" && ");
                    section.push_str(rest);
                }
                section.push_str(&format!(" && property:esp.module.{}.ready=1\n", module.id));
            } else {
                ensure!(
                    line.starts_with(char::is_whitespace),
                    "unsupported module RC top-level directive"
                );
                ensure!(
                    !trimmed.starts_with("setprop esp.") && !trimmed.starts_with("enable "),
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
                        name.starts_with(&format!("esp-{}-", module.id)),
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
        "\nservice kernelsu-esp-bootstrap-{name} {command}\n    user root\n    group root\n    seclabel u:r:init:s0\n    disabled\n    oneshot\n    reboot_on_failure reboot\n\non early-init\n    exec_start kernelsu-esp-bootstrap-{name}\n"
    )
}

pub fn assemble(descriptor: &mut Descriptor) -> Result<Vec<u8>> {
    if descriptor.config.norc {
        return Ok(Vec::new());
    }
    let mut fragments = Vec::new();
    let mut admitted = Vec::new();
    for mut module in std::mem::take(&mut descriptor.modules) {
        match module_rc_at(&module, &Path::new(MODULES).join(&module.id).join("initrc")) {
            Ok((rc, services)) => {
                module.services = services;
                fragments.push(rc);
                admitted.push(module);
            }
            Err(error) if module.critical => {
                return Err(error).with_context(|| format!("critical module {} RC", module.id));
            }
            Err(error) => log::warn!("rejecting optional module {} RC: {error:#}", module.id),
        }
    }
    descriptor.modules = admitted;
    let encoded = descriptor.encode()?;
    // Explicit service labels keep stock toybox in init, instead of taking its
    // ordinary toolbox transition. exec_start plus reboot_on_failure makes each
    // prerequisite blocking and fatal, including failure to execute the tool.
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
            "/system/bin/toybox mknod -m 0600 /dev/block/esp-selected b {} {}",
            descriptor.esp_major, descriptor.esp_minor
        ),
    )?;
    bootstrap_step(
        &mut rc,
        "esp",
        format_args!(
            "/system/bin/toybox mount -t vfat -o rw,nosuid,nodev,noexec,uid=0,gid=0,fmask=0022,dmask=0022 /dev/block/esp-selected {ESP}"
        ),
    )?;
    bootstrap_step(
        &mut rc,
        "bin",
        format_args!(
            "/system/bin/toybox mount -t tmpfs -o nosuid,nodev,mode=0755 kernelsu-esp {BIN}"
        ),
    )?;
    bootstrap_step(
        &mut rc,
        "copy",
        format_args!("/system/bin/toybox cp {PACKAGE}/bin/esuinit {BIN}/esuinit"),
    )?;
    bootstrap_step(
        &mut rc,
        "mode",
        format_args!("/system/bin/toybox chmod 0755 {BIN}/esuinit"),
    )?;
    bootstrap_step(
        &mut rc,
        "label",
        format_args!("/system/bin/toybox chcon u:object_r:esp_file:s0 {ROOT} {BIN} {BIN}/esuinit"),
    )?;
    bootstrap_step(
        &mut rc,
        "reconstruct",
        format_args!("{BIN}/esuinit --reconstruct {encoded}"),
    )?;
    for stage in crate::Stage::ALL {
        let single = [stage.as_str()];
        let events: &[&str] = match stage {
            crate::Stage::Service => &[
                "nonencrypted",
                "property:vold.decrypt=trigger_restart_framework",
            ],
            crate::Stage::BootCompleted => &["property:sys.boot_completed=1"],
            crate::Stage::PostMount => &["post-fs-data"],
            _ => &single,
        };
        for event in events {
            rc.push_str(&format!(
                "\non {event}\n    exec u:r:esp:s0 root -- {BIN}/ksud --esp-stage {}\n",
                stage.as_str()
            ));
        }
    }
    for fragment in fragments {
        rc.push_str(&fragment);
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
            "service esp-example-hal /dev/kernelsu-esp/bin/hal\n    class hal\n    user root\non post-fs\n    write /dev/example 1\n").unwrap();
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
        assert!(
            rc.contains("on property:esp.stage.post-fs=1 && property:esp.module.example.ready=1")
        );
        assert!(!rc.contains("    start esp-example-hal"));
        assert_eq!(services[0].name, "esp-example-hal");
        assert_eq!(services[0].stage.as_deref(), Some("post-fs"));
        std::fs::write(directory.path().join("service.rc"),
            "service esp-example-watchdog /dev/kernelsu-esp/bin/watchdog\n    disabled\n    class core\non init\n    start esp-example-watchdog\n").unwrap();
        let (rc, services) = module_rc_at(&module, directory.path()).unwrap();
        assert_eq!(services[0].stage.as_deref(), Some("init"));
        assert!(!rc.contains("on property:esp.stage.init"));
        std::fs::write(
            directory.path().join("service.rc"),
            "service foreign-service /system/bin/true\n",
        )
        .unwrap();
        assert!(module_rc_at(&module, directory.path()).is_err());
    }
}
