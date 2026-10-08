// SPDX-License-Identifier: GPL-3.0-only
//! The host half of `lab.py phone ota-probe`: one machine-readable line per probe.
//!
//! ```text
//! cargo run --release -p ota-core --example inspect -- kmi boot_b.img
//! kernel-6.12 banner -> "kmi android16-6.12-6"
//! cargo run --release -p ota-core --example inspect -- arb xbl_config_b.img
//! OEM ARB metadata  -> "arb major=1 minor=2 arb=5", or "arb none"
//! cargo run --release -p ota-core --example inspect -- efisp abl_b.img
//! ABL LinuxLoader   -> "efisp true" or "efisp false"
//! ```
//!
//! The three probes are exactly the functions the boot HAL and `ota-stage` call
//! (`kmi_from_boot`, `arb::scan`, `abl_has_efisp`), so this example cannot drift
//! from them: it only prints their answers in one line each. A failure (an
//! unreadable file, an image that is not a boot image, an unknown probe) prints
//! the error to stderr and exits 1.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use ota_core::{abl_has_efisp, arb, kmi_from_boot};

fn main() -> ExitCode {
    match run() {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("inspect: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// The one probe the arguments name, as the one line the lab record keeps.
fn run() -> Result<String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [probe, image] = args.as_slice() else {
        bail!("usage: inspect <kmi|arb|efisp> <image>");
    };
    let path = PathBuf::from(image);
    let bytes = std::fs::read(&path)
        .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
    match probe.as_str() {
        "kmi" => {
            let kmi = kmi_from_boot(&bytes)?;
            Ok(format!("kmi {}-{}", kmi.branch, kmi.generation))
        }
        "arb" => Ok(match arb::scan(&bytes) {
            Some(arb) => format!(
                "arb major={} minor={} arb={}",
                arb.major, arb.minor, arb.arb
            ),
            None => "arb none".to_string(),
        }),
        "efisp" => Ok(format!("efisp {}", abl_has_efisp(&bytes))),
        other => bail!("unknown probe {other:?}: expected kmi, arb or efisp"),
    }
}
