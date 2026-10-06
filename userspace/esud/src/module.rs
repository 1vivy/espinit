//! KernelSU scripts from the immutable ESP module tree.
use crate::{defs, rom_isolation::RuntimeRom};
use anyhow::{Context, Result, ensure};
use std::path::Path;
use std::process::Command;

pub fn manifest() -> Result<esuinit::config::Manifest> {
    esuinit::config::parse_manifest(&std::fs::read_to_string("/dev/esp/esu/manifest.toml")?)
        .map_err(anyhow::Error::msg)
}

pub fn scripts(order: &[String], stage: &str, rom: Option<&RuntimeRom>) -> Result<()> {
    for id in order {
        let directory = Path::new(defs::MODULE_DIR).join(id);
        if stage == "recovery" && !directory.join("recovery-ok").is_file() {
            continue;
        }
        let script = directory.join(format!("{stage}.sh"));
        if !script.exists() {
            continue;
        }
        let mut command = Command::new(defs::BUSYBOX);
        command
            .arg("sh")
            .arg(&script)
            .current_dir(&directory)
            .env("ASH_STANDALONE", "1")
            .env("ESU", "true")
            .env("ESU_MODULE", id)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    defs::BINARY_DIR,
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
        if let Some(rom) = rom {
            command
                .env("ESU_ROM", &rom.config.id)
                .env("ESU_ROM_NUMBER", rom.number.to_string());
        } else {
            command.env_remove("ESU_ROM").env_remove("ESU_ROM_NUMBER");
        }
        let status = command
            .status()
            .with_context(|| format!("start {}", script.display()))?;
        ensure!(status.success(), "module {id} {stage} failed: {status}");
    }
    Ok(())
}
