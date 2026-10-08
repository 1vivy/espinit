//! The Android runtime behind the transaction's platform surface.
//!
//! [`crate::txn::Env`] is the transaction's whole outside world; this is the
//! implementation the served HAL uses: the project efivarfs records, the ESP
//! payload tree the loader mounted, the module sets boot-patch wrote, the
//! bounded read-write windows the staged archive and the denial receipt are
//! written in, and the detached promote.
//!
//! Everything it touches is a path or a record another component owns, so the
//! layout is taken from [`crate::platform`] and the payload schema from
//! `esu_config` instead of being spelled out here.

use crate::platform::{self, ESP_MOUNT, PAYLOAD};
use crate::txn::{Class, Env, Rom, Rom1, RomN};
use anyhow::{Context, Result, anyhow, ensure};
use esu_config::{KernelImages, parse_installed, parse_manifest, rom_config_path};
use esu_platform::efivars;
use esu_platform::stage::StageState;
use ota_core::{Kmi, select_module_set};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// Receipt of a refused update, relative to the runtime payload root.
///
/// The ota module's `boot-completed.sh` posts it when the notification command
/// itself was refused, and deletes it afterwards.
pub const DENIAL_RECEIPT: &str = "receipts/ota-denied.txt";

/// The Android platform of one served boot.
pub struct Android {
    /// efivarfs root, `/dev/efivars`.
    root: PathBuf,
}

impl Android {
    /// The platform reading this boot's records from `root`.
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
        }
    }

    /// The declared base images of one managed ROM, in `IMAGE_BASES` order.
    ///
    /// The installed manifest and the selected ROM are admitted exactly as PID 1
    /// admits them at boot, so the bases staged here are the bases the running
    /// projection serves.
    fn bases(&self, id: &str, rom_number: u32) -> Result<Vec<&'static str>> {
        let manifest = self.text("manifest.toml")?;
        let parsed = parse_manifest(&manifest).map_err(|error| anyhow!("{error}"))?;
        let rom = self.text(&rom_config_path(&parsed, id))?;
        let installed =
            parse_installed(&manifest, &rom, id, rom_number).map_err(|error| anyhow!("{error}"))?;
        Ok(match installed.kernel_images() {
            KernelImages::Esp(images) => images.into_iter().map(|image| image.base).collect(),
            KernelImages::Physical => Vec::new(),
        })
    }

    /// One text file of the runtime payload tree.
    fn text(&self, relative: &str) -> Result<String> {
        let path = Path::new(PAYLOAD).join(relative);
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))
    }
}

impl Env for Android {
    fn stage(&self, id: &str) -> io::Result<StageState> {
        efivars::stage(&self.root, id)
    }

    fn write_stage(&self, id: &str, state: StageState) -> io::Result<()> {
        efivars::write_stage(&self.root, id, state)
    }

    fn selected(&self, id: &str) -> io::Result<u8> {
        efivars::selected_slot(&self.root, id)
    }

    fn payload_file(&self, relative: &str) -> Result<Vec<u8>> {
        let path = Path::new(PAYLOAD).join(relative);
        std::fs::read(&path).with_context(|| format!("read {}", path.display()))
    }

    /// The module set boot-patch placed for `kmi`, already verified against its
    /// manifest. A missing set is the update's denial, never a fallback to the
    /// set the running kernel uses.
    fn modules(&self, kmi: &Kmi) -> Result<BTreeMap<String, Vec<u8>>> {
        select_module_set(Path::new(ESP_MOUNT), kmi)?.payload_members()
    }

    fn write_stage_payload(&self, id: &str, bytes: &[u8]) -> Result<()> {
        let path = platform::stage_payload(id);
        platform::writable(|| platform::write_replace(&path, bytes))
    }

    fn remove_stage_payload(&self, id: &str) -> Result<()> {
        let path = platform::stage_payload(id);
        platform::writable(|| platform::remove_absent_ok(&path))
    }

    fn commit_payload(&self, id: &str) -> Result<()> {
        let staged = platform::stage_payload(id);
        let committed = platform::committed_payload(id);
        platform::writable(|| {
            // A resumed promote: the archive was already renamed when the
            // previous attempt got this far, and only `Stage` is left to clear.
            if !staged.exists() {
                ensure!(
                    committed.exists(),
                    "{} and {} are both missing",
                    staged.display(),
                    committed.display()
                );
                return Ok(());
            }
            platform::rename(&staged, &committed)
        })
    }

    fn deny(&self, reason: &str) {
        platform::log(&format!("update refused: {reason}"));

        // The receipt is best effort: an ESP that cannot be remounted read-write
        // still leaves the log line and the notification.
        let receipt = Path::new(PAYLOAD).join(DENIAL_RECEIPT);
        let text = format!("{reason}\n");
        if let Err(error) =
            platform::writable(|| platform::write_replace(&receipt, text.as_bytes()))
        {
            platform::log(&format!("denial receipt: {error:#}"));
        }

        platform::notify(reason);
    }

    fn detach(&self, job: Box<dyn FnOnce() + Send + 'static>) {
        // A promote copies whole base images and must not block the Binder
        // thread. A thread that cannot be spawned leaves `Stage` at `Promote`,
        // which the next start resumes.
        if let Err(error) = std::thread::Builder::new()
            .name("esu-promote".to_owned())
            .spawn(job)
        {
            platform::log(&format!("promote thread: {error}"));
        }
    }

    fn class(&self, rom: &Rom) -> Result<Box<dyn Class>> {
        if rom.number < 2 {
            return Ok(Box::new(Rom1::new(rom)));
        }
        Ok(Box::new(RomN::new(rom, self.bases(&rom.id, rom.number)?)))
    }
}
