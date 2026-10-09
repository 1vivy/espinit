//! The OTA transaction both ROM classes share: staging, sealing, cancelling and
//! promoting.
//!
//! One AIDL mutation is one step of this machine. The letters come from the
//! booted slot, the `Slot-<id>` record and the `Stage-<id>` record — never from
//! anything this process remembers — and the boot's staging posture is
//! snapshotted once at service start, because that snapshot is what decides
//! whether a teardown is allowed to happen at all: while the switch device
//! serves the letter this boot runs from, removing the staging set it points at
//! would turn the running Android's own partitions into I/O errors.
//!
//! The two halves of the platform are injected so the whole machine is
//! host-testable:
//!
//! * [`Env`] is the records, the ESP payload tree and the process side effects
//!   (the denial receipt, the notification, the detached promote).
//! * [`Class`] is the storage side of one ROM: where the target's bytes are,
//!   and how a staging set is created, removed and promoted. [`Rom1`] writes the
//!   physical partitions of the target letter and stages nothing but the
//!   takeover payload; [`RomN`] owns the per-base staging LVs and the switch
//!   devices that serve them.

use crate::platform;
use crate::wire::{Gbs1, NO_PENDING};
use anyhow::{Context, Result, anyhow, bail, ensure};
use esu_platform::stage::StageState;
use generic_bootctl_core::{MergeStatus, Operation};
use ota_core::{
    Kmi, SECTOR_SIZE, arb, build_overlay, copy_range, error_target, esd_by_name, esd_lv,
    exact_sectors, kmi_from_boot, linear, ota_dm_name, stage_dm_name, stage_lv_name, verify_equal,
};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::Arc;

/// Identity of the managed ROM whose transaction is running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RomId {
    /// Catalogue id from `BootedRom`, which names every variable and path.
    pub id: String,
    /// bdsvars ROM number from the provisioned `Slot-<id>` record.
    pub number: u32,
}

/// One transaction's ROM context, handed to [`Env::class`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rom {
    pub id: String,
    pub number: u32,
    /// Booted letter, from `ro.boot.slot_suffix`.
    pub current: u8,
    /// Letter this transaction targets: the requested or staged slot.
    pub target: u8,
    /// The switch devices serve the letter this boot runs from.
    pub booted_staged: bool,
}

/// The transaction's platform surface: the records, the payload tree and the
/// side effects that are not the storage class.
///
/// Everything here is `&self`: the transaction state lives in [`Txn`], the
/// records live in firmware and the payload lives on the ESP, so the handle can
/// be shared with the detached promote.
pub trait Env: Send + Sync {
    /// `Stage-<id>`; an absent variable is [`StageState::None`].
    fn stage(&self, id: &str) -> std::io::Result<StageState>;
    /// Replace `Stage-<id>`.
    fn write_stage(&self, id: &str, state: StageState) -> std::io::Result<()>;
    /// `Slot-<id>` byte 8, the selected letter.
    fn selected(&self, id: &str) -> std::io::Result<u8>;
    /// One file of the runtime payload root, for example `bin/esuinit`.
    fn payload_file(&self, relative: &str) -> Result<Vec<u8>>;
    /// The verified overlay members of the module set for `kmi`:
    /// `lib/<name>.ko` to bytes. A missing set is the update's denial.
    fn modules(&self, kmi: &Kmi) -> Result<BTreeMap<String, Vec<u8>>>;
    /// Replace this ROM's staged takeover archive.
    fn write_stage_payload(&self, id: &str, bytes: &[u8]) -> Result<()>;
    /// Remove this ROM's staged takeover archive; absent is success.
    fn remove_stage_payload(&self, id: &str) -> Result<()>;
    /// Rename the staged archive onto the committed one.
    fn commit_payload(&self, id: &str) -> Result<()>;
    /// Refuse the update: receipt, kernel log and notification.
    fn deny(&self, reason: &str);
    /// Run the promote off the serving thread.
    fn detach(&self, job: Box<dyn FnOnce() + Send + 'static>);
    /// The storage side of one ROM's transaction.
    fn class(&self, rom: &Rom) -> Result<Box<dyn Class>>;
}

/// The storage side of one ROM's transaction.
///
/// Every method takes `&self` so the detached promote can own the class; the
/// transaction serializes the calls that must not overlap.
pub trait Class: Send {
    /// The target's bytes for one base image: the staging LV of a managed ROM,
    /// the physical target partition of ROM 1.
    fn read_target_image(&self, base: &str) -> Result<Vec<u8>>;
    /// The running letter's bytes for one base image.
    fn read_current_image(&self, base: &str) -> Result<Vec<u8>>;
    /// Create, activate and prefill this ROM's staging set; idempotent, because
    /// a resumed update calls it again for the same set.
    fn prepare(&self) -> Result<()>;
    /// Remove the staging set and stop serving it through the switch devices.
    fn teardown(&self) -> Result<()>;
    /// Copy the staged bytes into the base images, verify them, and put the
    /// running letter back on the promoted image before the set is removed.
    fn promote(&self) -> Result<()>;
}

/// The letter a staged set belongs to.
///
/// `Staging` is always the non-current letter: an update writes the slot it is
/// not running. `Sealed` and `Promote` name the selected letter, which is the
/// one the next boot runs the staged set from.
fn staged_letter(current: u8, selected: u8, stage: StageState) -> Option<u8> {
    match stage {
        StageState::None => None,
        StageState::Staging => Some(other(current)),
        StageState::Sealed | StageState::Promote => Some(selected),
    }
}

/// The other letter of a two-slot device.
fn other(letter: u8) -> u8 {
    1 - letter
}

/// The letter of one operation's slot index.
///
/// The shared service validates the index against the slot count before any
/// storage access, so anything else here is a caller this HAL does not serve.
fn letter_of(slot: u32) -> Result<u8> {
    u8::try_from(slot)
        .ok()
        .filter(|letter| *letter < 2)
        .with_context(|| format!("slot index {slot} is not a letter"))
}

/// Partition-name suffix of a letter, for the messages and physical backends.
fn suffix(letter: u8) -> &'static str {
    if letter == 0 { "_a" } else { "_b" }
}

/// Whether one slot of the record is marked successful.
fn successful(record: &Gbs1, slot: u8) -> bool {
    record
        .slots
        .get(usize::from(slot))
        .is_some_and(|s| s.successful)
}

/// Refuse the update: the receipt, the log and the notification, then the error
/// the caller returns. The record is never written when this is returned.
fn refuse(env: &Arc<dyn Env>, reason: impl Into<String>) -> anyhow::Error {
    let reason = reason.into();
    env.deny(&reason);
    anyhow!("{reason}")
}

/// The transaction state machine of one serving process.
pub struct Txn {
    /// Booted letter, from `ro.boot.slot_suffix`.
    current: u8,
    /// The switch devices serve this boot's letter: snapshotted at start, so a
    /// record another process writes mid-boot cannot make this process tear
    /// down the set the running letter reads.
    booted_staged: bool,
    started: bool,
}

impl Txn {
    /// A machine for the booted letter, before any record was read.
    pub fn new(current: u8) -> Self {
        Self {
            current,
            booted_staged: false,
            started: false,
        }
    }

    /// Service start: read this ROM's transaction records, snapshot the boot's
    /// staging posture and run the resume rules once.
    ///
    /// A record that cannot be read is an error the caller reports as a failed
    /// transaction (the state is unknown, so nothing may be staged or torn
    /// down). The resume itself is best effort: a promote that cannot finish
    /// leaves `Stage` at `Promote` and the next start resumes it.
    pub fn start(
        &mut self,
        env: &Arc<dyn Env>,
        rom: &RomId,
        record: &Gbs1,
        merge: MergeStatus,
    ) -> Result<()> {
        if self.started {
            return Ok(());
        }
        let stage = env.stage(&rom.id)?;
        let selected = env.selected(&rom.id)?;
        self.booted_staged =
            matches!(stage, StageState::Sealed | StageState::Promote) && selected == self.current;
        self.started = true;

        let letter = staged_letter(self.current, selected, stage);
        let result = match stage {
            // A promote was interrupted (or is being resumed after a reboot).
            StageState::Promote => match letter {
                Some(target) => self.promote(env, rom, target),
                None => Ok(()),
            },
            StageState::Sealed if self.running_staged(rom, record) => {
                if successful(record, self.current) && merge == MergeStatus::None {
                    self.promote(env, rom, self.current)
                } else {
                    Ok(())
                }
            }
            // ROM 1: the firmware switch was confirmed and this boot still runs
            // the other letter, so the staged payload is never used.
            StageState::Sealed
                if rom.number < 2
                    && record.pending == NO_PENDING
                    && letter != Some(self.current) =>
            {
                self.cancel(env, rom, letter.unwrap_or(self.current))
            }
            StageState::None | StageState::Staging | StageState::Sealed => Ok(()),
        };
        if let Err(error) = result {
            platform::log(&format!("boot-hal transaction resume: {error:#}"));
        }
        Ok(())
    }

    /// The side effects of one AIDL mutation, before the record write.
    ///
    /// Returning `Err` leaves the `Slot-<id>` record untouched, which is what
    /// makes a denial a denial: the updater sees `COMMAND_FAILED` and the boot
    /// state still describes the letter it is running.
    pub fn commit(
        &mut self,
        env: &Arc<dyn Env>,
        rom: &RomId,
        record: &Gbs1,
        merge: MergeStatus,
        operation: Operation,
    ) -> Result<()> {
        let stage = env.stage(&rom.id)?;
        match operation {
            Operation::SetUnbootable(slot) => {
                let target = letter_of(slot)?;
                if target == self.current {
                    // Marking the letter this boot runs from: nothing to stage.
                    return Ok(());
                }
                if rom.number >= 2 {
                    self.unbootable(env, rom, stage, target)
                } else if staged_letter(self.current, record.selected, stage) == Some(target) {
                    self.cancel(env, rom, target)
                } else {
                    Ok(())
                }
            }
            Operation::SetActive(slot) if letter_of(slot)? == self.current => {
                if matches!(stage, StageState::Staging | StageState::Sealed) {
                    if self.booted_staged {
                        // The staged set is what this boot runs: tearing it down
                        // would kill the running system, and the promote path
                        // owns it from here.
                        platform::log(
                            "cancel ignored: the switch device serves the letter this boot runs",
                        );
                        Ok(())
                    } else {
                        self.cancel(env, rom, self.current)
                    }
                } else {
                    Ok(())
                }
            }
            Operation::SetActive(slot) => {
                let target = letter_of(slot)?;
                if self.booted_staged {
                    return Err(refuse(
                        env,
                        "a staged set serves this boot; reboot before staging another update",
                    ));
                }
                // A re-request of the letter this ROM already sealed is the
                // updater retrying, not a new seal.
                let sealed = if rom.number >= 2 {
                    Some(record.selected)
                } else if record.pending == NO_PENDING {
                    None
                } else {
                    Some(record.pending)
                };
                if stage == StageState::Sealed && sealed == Some(target) {
                    return Ok(());
                }
                let context = self.rom(rom, target);
                let class = env.class(&context)?;
                seal(env, &context, &*class)?;
                env.write_stage(&rom.id, StageState::Sealed)?;
                platform::log(&format!(
                    "staged update sealed for letter {}",
                    suffix(target)
                ));
                Ok(())
            }
            Operation::MarkSuccessful(slot) => {
                let target = letter_of(slot)?;
                if stage == StageState::Sealed
                    && merge == MergeStatus::None
                    && target == self.current
                    && self.running_staged(rom, record)
                {
                    self.promote(env, rom, target)
                } else {
                    Ok(())
                }
            }
        }
    }

    /// A merge that just finished: the staged bytes are what this boot runs, so
    /// they are promoted now instead of at the next start.
    pub fn merge_completed(
        &mut self,
        env: &Arc<dyn Env>,
        rom: &RomId,
        record: &Gbs1,
        before: MergeStatus,
        after: MergeStatus,
    ) -> Result<()> {
        if before != MergeStatus::Merging || after != MergeStatus::None {
            return Ok(());
        }
        let stage = env.stage(&rom.id)?;
        if stage == StageState::Sealed
            && successful(record, self.current)
            && self.running_staged(rom, record)
        {
            self.promote(env, rom, self.current)?;
        }
        Ok(())
    }

    /// Whether the staged set is the one this boot runs.
    ///
    /// For a managed ROM the selected letter is the running one. ROM 1 only
    /// after Surfacer confirmed the switch: a pending request means the new slot
    /// has not booted yet, so the staged payload is not what this boot runs.
    fn running_staged(&self, rom: &RomId, record: &Gbs1) -> bool {
        record.selected == self.current && (rom.number >= 2 || record.pending == NO_PENDING)
    }

    /// One transaction's ROM context for `target`.
    fn rom(&self, rom: &RomId, target: u8) -> Rom {
        Rom {
            id: rom.id.clone(),
            number: rom.number,
            current: self.current,
            target,
            booted_staged: self.booted_staged,
        }
    }

    /// `SetUnbootable(target)` of a managed ROM: the updater's first write is
    /// about to go to the target letter, so the staging set has to exist and
    /// the switch devices have to serve it.
    fn unbootable(
        &mut self,
        env: &Arc<dyn Env>,
        rom: &RomId,
        stage: StageState,
        target: u8,
    ) -> Result<()> {
        if self.booted_staged {
            // Also update_engine's post-merge `MarkSlotUnbootable(old)`. A
            // second update in this boot then fails on the read-only ESP the
            // running letter reads; it needs a reboot.
            platform::log(
                "staging not started: the switch device serves the letter this boot runs",
            );
            return Ok(());
        }
        match stage {
            StageState::None => self.prepare(env, rom, target),
            StageState::Staging => {
                platform::log("staging set already exists; resuming the update");
                Ok(())
            }
            StageState::Sealed => {
                self.cancel(env, rom, target)?;
                self.prepare(env, rom, target)
            }
            StageState::Promote => {
                platform::log("a promote is in flight; not staging");
                Ok(())
            }
        }
    }

    /// Create the staging set and record that it holds an in-progress update.
    ///
    /// The set is created before the record says `Staging`: a record that names
    /// a staging LV which does not exist would make the next boot's switch
    /// device fail to resolve.
    fn prepare(&mut self, env: &Arc<dyn Env>, rom: &RomId, target: u8) -> Result<()> {
        let class = env.class(&self.rom(rom, target))?;
        class.prepare().context("create the staging set")?;
        env.write_stage(&rom.id, StageState::Staging)?;
        platform::log(&format!(
            "staging set for letter {} created",
            suffix(target)
        ));
        Ok(())
    }

    /// Drop the staged update: the archive, the set, then the record.
    fn cancel(&mut self, env: &Arc<dyn Env>, rom: &RomId, target: u8) -> Result<()> {
        env.remove_stage_payload(&rom.id)
            .context("remove the staged payload")?;
        env.class(&self.rom(rom, target))?
            .teardown()
            .context("remove the staging set")?;
        env.write_stage(&rom.id, StageState::None)?;
        platform::log("staged update cancelled");
        Ok(())
    }

    /// Promote the staged bytes: `Stage` becomes `Promote` first, so a process
    /// that dies mid-copy resumes the same step at the next start.
    fn promote(&mut self, env: &Arc<dyn Env>, rom: &RomId, target: u8) -> Result<()> {
        let class = env.class(&self.rom(rom, target))?;
        env.write_stage(&rom.id, StageState::Promote)?;

        let id = rom.id.clone();
        let promote_env = Arc::clone(env);
        env.detach(Box::new(move || match class.promote() {
            Ok(()) => {
                if let Err(error) = promote_env.commit_payload(&id) {
                    // `Stage` stays `Promote`: the next start promotes again.
                    platform::log(&format!("promote: commit the payload: {error:#}"));
                    return;
                }
                if let Err(error) = promote_env.write_stage(&id, StageState::None) {
                    platform::log(&format!("promote: clear the stage record: {error:#}"));
                }
            }
            Err(error) => platform::log(&format!("promote: {error:#}")),
        }));
        platform::log("promoting the staged bytes");
        Ok(())
    }
}

/// Seal one update: read the target's kernel identity, refuse what may not run,
/// and write the takeover archive the staged letter boots.
fn seal(env: &Arc<dyn Env>, rom: &Rom, class: &dyn Class) -> Result<()> {
    let boot = class
        .read_target_image("boot")
        .map_err(|error| refuse(env, format!("target boot image: {error:#}")))?;
    let kmi = kmi_from_boot(&boot)
        .map_err(|error| refuse(env, format!("target kernel identity: {error:#}")))?;
    anti_rollback(env, rom, class)?;

    let modules = env
        .modules(&kmi)
        .map_err(|error| refuse(env, format!("{error:#}")))?;
    let esuinit = env
        .payload_file("bin/esuinit")
        .map_err(|error| refuse(env, format!("esuinit payload: {error:#}")))?;
    let build_id = env.payload_file("build-id")?;
    let build_id = std::str::from_utf8(&build_id)
        .map_err(|error| refuse(env, format!("build id: {error}")))?
        .trim();
    ensure!(!build_id.is_empty(), "build id is empty");

    let overlay = build_overlay(&esuinit, &modules, build_id)?;
    env.write_stage_payload(&rom.id, &overlay)
}

/// Refuse a ROM 1 update whose target firmware raises the anti-rollback index.
///
/// Only ROM 1 executes the firmware the update writes, so only ROM 1 refuses;
/// a managed ROM's firmware views are never executed and the comparison is
/// logged. An image the port cannot read or recognize is not evidence of a
/// raise, so it is logged rather than refused — but a *target* image that
/// cannot be read at all is an incomplete payload and is refused.
fn anti_rollback(env: &Arc<dyn Env>, rom: &Rom, class: &dyn Class) -> Result<()> {
    let target = class.read_target_image("xbl_config");
    let current = class.read_current_image("xbl_config");
    if rom.number < 2 {
        let target = target.map_err(|error| {
            refuse(
                env,
                format!("target xbl_config for the anti-rollback check: {error:#}"),
            )
        })?;
        let current = match current {
            Ok(image) => image,
            Err(error) => {
                platform::log(&format!("xbl_config of the running letter: {error:#}"));
                return Ok(());
            }
        };
        let (Some(new), Some(old)) = (arb::scan(&target), arb::scan(&current)) else {
            platform::log("xbl_config anti-rollback metadata is unreadable; not refusing");
            return Ok(());
        };
        if new.arb > old.arb {
            return Err(refuse(
                env,
                format!(
                    "anti-rollback raised: xbl_config ARB {} > {}",
                    new.arb, old.arb
                ),
            ));
        }
        platform::log(&format!(
            "xbl_config ARB {} ({}.{}) vs {} ({}.{})",
            new.arb, new.major, new.minor, old.arb, old.major, old.minor
        ));
        return Ok(());
    }

    let new = target.ok().and_then(|image| arb::scan(&image));
    let old = current.ok().and_then(|image| arb::scan(&image));
    match (new, old) {
        (Some(new), Some(old)) => platform::log(&format!(
            "xbl_config ARB {} vs {} (not executed by this ROM)",
            new.arb, old.arb
        )),
        _ => platform::log("xbl_config anti-rollback metadata is unreadable; not refusing"),
    }
    Ok(())
}

/// Copy every staged base image back into the ESP image and verify it, retrying
/// a mismatch once.
///
/// A mismatch means the bytes on the ESP are not the bytes that ran, so the
/// promote may not finish; the caller keeps `Stage` at `Promote` and the next
/// start tries again. The closures are the caller's, which is what lets the
/// retry policy be tested without a block device.
fn copy_back(
    bases: &[&str],
    copy: impl Fn(&str) -> Result<()>,
    verify: impl Fn(&str) -> Result<bool>,
) -> Result<()> {
    for base in bases {
        for attempt in 0..2 {
            copy(base)?;
            if verify(base)? {
                break;
            }
            if attempt == 1 {
                bail!("promote verify failed: {base}");
            }
            platform::log(&format!(
                "promote verify mismatch for {base}; copying again"
            ));
        }
    }
    Ok(())
}

/// Read one physical partition of a letter through the esd tree.
fn read_partition(base: &str, letter: u8) -> Result<Vec<u8>> {
    let node = esd_by_name(&format!("{base}{}", suffix(letter)));
    read_partition_node(base, Path::new(&node))
}

fn read_partition_node(base: &str, node: &Path) -> Result<Vec<u8>> {
    if base == "boot" {
        platform::read_boot_node(node)
    } else {
        platform::read_node(node, platform::physical_limit())
    }
}

/// ROM 1: the update writes the physical partitions of the target letter, so
/// the only thing this ROM stages is the takeover payload.
pub struct Rom1 {
    current: u8,
    target: u8,
}

impl Rom1 {
    /// The class of one ROM 1 transaction.
    pub fn new(rom: &Rom) -> Self {
        Self {
            current: rom.current,
            target: rom.target,
        }
    }
}

impl Class for Rom1 {
    fn read_target_image(&self, base: &str) -> Result<Vec<u8>> {
        read_partition(base, self.target)
    }

    fn read_current_image(&self, base: &str) -> Result<Vec<u8>> {
        read_partition(base, self.current)
    }

    /// ROM 1 has no staging set: the projection routes the target letter to the
    /// physical partitions the updater writes directly.
    fn prepare(&self) -> Result<()> {
        Ok(())
    }

    fn teardown(&self) -> Result<()> {
        Ok(())
    }

    /// The firmware switch belongs to Surfacer; the promote is the payload
    /// rename, which the shared transaction performs.
    fn promote(&self) -> Result<()> {
        Ok(())
    }
}

/// A managed ROM `>= 2`: every declared base image is staged in its own thick
/// logical volume and served through the per-base switch device.
pub struct RomN {
    id: String,
    number: u32,
    current: u8,
    target: u8,
    /// The switch devices serve the letter this boot runs from, so the promote
    /// has to put the running letter back on the promoted image.
    booted_staged: bool,
    bases: Vec<&'static str>,
}

impl RomN {
    /// The class of one managed ROM transaction, over its declared bases.
    pub fn new(rom: &Rom, bases: Vec<&'static str>) -> Self {
        Self {
            id: rom.id.clone(),
            number: rom.number,
            current: rom.current,
            target: rom.target,
            booted_staged: rom.booted_staged,
            bases,
        }
    }

    /// The ESP image file of one base.
    fn base_image(&self, base: &str) -> std::path::PathBuf {
        platform::base_image(&self.id, base)
    }

    /// The staging LV node of one base, as esud publishes it.
    fn stage_node(&self, base: &str) -> String {
        esd_lv(&stage_lv_name(self.number, base))
    }

    /// Sectors of one base image: the length of every table and copy.
    fn sectors(&self, base: &str) -> Result<u64> {
        exact_sectors(&self.base_image(base))
    }

    /// Bytes of one base image.
    fn image_bytes(&self, base: &str) -> Result<u64> {
        Ok(self.sectors(base)? * SECTOR_SIZE)
    }

    fn declared(&self, base: &str) -> bool {
        self.bases.contains(&base)
    }

    /// Create, activate and prefill the staging set of every declared base.
    fn stage(&self) -> Result<()> {
        let sizes = platform::lvm_sizes()?;
        for base in &self.bases {
            let bytes = self.image_bytes(base)?;
            let lv = stage_lv_name(self.number, base);
            match sizes.get(&lv) {
                // A set left behind by an interrupted attempt is reused, but
                // only when it can hold the whole image: the switch device
                // covers the image, not the extent-rounded logical volume.
                Some(size) if *size >= bytes => {}
                Some(_) => {
                    platform::lvm(&["lvremove", "-f", &format!("rom/{lv}")])?;
                    create_lv(&lv, bytes)?;
                }
                None => create_lv(&lv, bytes)?,
            }
            platform::lvm(&["lvchange", "-ay", &format!("rom/{lv}")])?;
        }
        platform::esd_refresh()?;

        let mut mapper = platform::mapper()?;
        for base in &self.bases {
            let sectors = self.sectors(base)?;
            let device = platform::mapper_number(&stage_dm_name(self.number, base))?;
            mapper
                .reload(
                    &ota_dm_name(self.number, base),
                    &[linear(device, sectors)],
                    false,
                )
                .map_err(anyhow::Error::msg)?;
            // The staged set starts as the running image, so the updater's
            // verifier and its incremental patches see the bytes they expect.
            copy_range(
                &File::open(self.base_image(base))?,
                &OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(self.stage_node(base))?,
                self.image_bytes(base)?,
            )?;
        }
        Ok(())
    }

    /// Stop serving the staged bytes and remove the logical volumes.
    fn remove_set(&self) -> Result<()> {
        let sizes = platform::lvm_sizes()?;
        for base in &self.bases {
            let lv = stage_lv_name(self.number, base);
            if sizes.contains_key(&lv) {
                platform::lvm(&["lvremove", "-f", &format!("rom/{lv}")])?;
            }
        }
        platform::esd_refresh()
    }

    /// Point every switch device at a read-only loop of the promoted image, so
    /// the letter this boot runs keeps reading the bytes it booted.
    fn keep_running(&self) -> Result<()> {
        let mut mapper = platform::mapper()?;
        for base in &self.bases {
            let device = platform::attach_read_only(&self.base_image(base))?;
            mapper
                .reload(
                    &ota_dm_name(self.number, base),
                    &[linear(device.number, self.sectors(base)?)],
                    true,
                )
                .map_err(anyhow::Error::msg)?;
            // Autoclear must not detach until reload has opened the loop.
            drop(device);
        }
        Ok(())
    }
}

impl Class for RomN {
    fn read_target_image(&self, base: &str) -> Result<Vec<u8>> {
        if self.declared(base) {
            platform::read_node(Path::new(&self.stage_node(base)), self.image_bytes(base)?)
        } else {
            read_partition(base, self.target)
        }
    }

    fn read_current_image(&self, base: &str) -> Result<Vec<u8>> {
        if self.declared(base) {
            platform::read_node(&self.base_image(base), self.image_bytes(base)?)
        } else {
            read_partition(base, self.current)
        }
    }

    fn prepare(&self) -> Result<()> {
        match self.stage() {
            Ok(()) => Ok(()),
            Err(error) => {
                // Leave nothing half-created: the next attempt starts clean.
                if let Err(teardown) = self.teardown() {
                    platform::log(&format!(
                        "staging teardown after a failed prepare: {teardown:#}"
                    ));
                }
                Err(error)
            }
        }
    }

    fn teardown(&self) -> Result<()> {
        let sizes = platform::lvm_sizes()?;
        let mut mapper = platform::mapper()?;
        for base in &self.bases {
            let name = ota_dm_name(self.number, base);
            if platform::mapper_exists(&name) {
                // An error target fails reads loudly instead of serving stale
                // bytes: nothing staged means nothing to serve.
                mapper
                    .reload(&name, &[error_target(self.sectors(base)?)], true)
                    .map_err(anyhow::Error::msg)?;
            }
            let lv = stage_lv_name(self.number, base);
            if sizes.contains_key(&lv) {
                platform::lvm(&["lvremove", "-f", &format!("rom/{lv}")])?;
            }
        }
        platform::esd_refresh()
    }

    fn promote(&self) -> Result<()> {
        platform::writable(|| {
            copy_back(
                &self.bases,
                |base| {
                    if !Path::new(&self.stage_node(base)).exists() {
                        // A resumed promote whose copy already ran: the staging
                        // volumes are the only thing that removes them, and the
                        // running letter is still reading the promoted image.
                        platform::log(&format!(
                            "promote: {} is gone; the staged bytes are already in place",
                            self.stage_node(base)
                        ));
                        return Ok(());
                    }
                    let bytes = self.image_bytes(base)?;
                    copy_range(
                        &File::open(self.stage_node(base))?,
                        &OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(self.base_image(base))?,
                        bytes,
                    )
                },
                |base| {
                    if !Path::new(&self.stage_node(base)).exists() {
                        return Ok(true);
                    }
                    let bytes = self.image_bytes(base)?;
                    verify_equal(
                        &File::open(self.stage_node(base))?,
                        &File::open(self.base_image(base))?,
                        bytes,
                    )
                },
            )
        })?;
        if self.booted_staged {
            self.keep_running()?;
        }
        self.remove_set()
    }
}

/// Create one staging logical volume, deactivated: the switch device is loaded
/// before the volume is activated.
fn create_lv(name: &str, bytes: u64) -> Result<()> {
    platform::lvm(&[
        "lvcreate",
        "--yes",
        "-an",
        "-Z",
        "n",
        "-W",
        "n",
        "-L",
        &format!("{bytes}b"),
        "-n",
        name,
        ota_core::VG,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    /// A synthetic `xbl_config`: a little-endian ELF64 whose one non-executable
    /// program header describes a HASH segment with the given ARB index.
    fn xbl_config(arb: u32) -> Vec<u8> {
        const PHENT: usize = 56;
        let phoff = 64usize;
        let table = phoff + PHENT;
        let mut segment = vec![0u8; 36 + 12];
        segment[0..4].copy_from_slice(&1u32.to_le_bytes());
        segment[16..20].copy_from_slice(&32u32.to_le_bytes());
        for (index, value) in [3u32, 0, arb].iter().enumerate() {
            segment[36 + index * 4..40 + index * 4].copy_from_slice(&value.to_le_bytes());
        }

        let mut image = vec![0u8; table];
        image[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        image[4] = 2;
        image[5] = 1;
        image[0x20..0x28].copy_from_slice(&(phoff as u64).to_le_bytes());
        image[0x36..0x38].copy_from_slice(&(PHENT as u16).to_le_bytes());
        image[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes());
        image[phoff + 4..phoff + 8].copy_from_slice(&4u32.to_le_bytes());
        image[phoff + 8..phoff + 16].copy_from_slice(&(table as u64).to_le_bytes());
        image[phoff + 32..phoff + 40].copy_from_slice(&(segment.len() as u64).to_le_bytes());
        image.extend_from_slice(&segment);
        image
    }

    /// An Android boot image v4 with one uncompressed kernel block carrying a
    /// KMI banner (the layout `ota_core`'s KMI test pins).
    fn boot_image(banner: &[u8]) -> Vec<u8> {
        let mut kernel = b"plain kernel bytes".to_vec();
        kernel.extend_from_slice(banner);
        kernel.extend_from_slice(&[0u8; 64]);

        let mut image = vec![0u8; 4096];
        image[..8].copy_from_slice(b"ANDROID!");
        image[8..12].copy_from_slice(&(kernel.len() as u32).to_le_bytes());
        image[20..24].copy_from_slice(&1584u32.to_le_bytes());
        image[40..44].copy_from_slice(&4u32.to_le_bytes());
        image.extend_from_slice(&kernel);
        image.resize(4096 + kernel.len().next_multiple_of(4096), 0);
        image
    }

    /// Everything the recorder saw and how the fake storage behaves.
    struct Inner {
        stage: StageState,
        selected: u8,
        current: u8,
        bases: Vec<&'static str>,
        boot: Vec<u8>,
        /// The target letter's `xbl_config`, whose ARB index decides a ROM 1
        /// refusal.
        xbl_target: Vec<u8>,
        /// The running letter's `xbl_config`.
        xbl_current: Vec<u8>,
        modules: Option<BTreeMap<String, Vec<u8>>>,
        payload: bool,
        committed: bool,
        promote_failures: usize,
        calls: Vec<String>,
        receipts: Vec<String>,
    }

    /// The platform a test drives the transaction with.
    struct Fake {
        inner: Arc<Mutex<Inner>>,
    }

    impl Fake {
        fn new(current: u8, stage: StageState, selected: u8, bases: Vec<&'static str>) -> Self {
            Self {
                inner: Arc::new(Mutex::new(Inner {
                    stage,
                    selected,
                    current,
                    bases,
                    boot: boot_image(b"Linux version 6.12.23-android16-6-g1a2b3c4d (x) #1 SMP"),
                    xbl_target: xbl_config(3),
                    xbl_current: xbl_config(3),
                    modules: Some(BTreeMap::from([(
                        "lib/kernelesp.ko".to_owned(),
                        b"\x7fELF module".to_vec(),
                    )])),
                    payload: false,
                    committed: false,
                    promote_failures: 0,
                    calls: Vec::new(),
                    receipts: Vec::new(),
                })),
            }
        }

        fn with(self, change: impl FnOnce(&mut Inner)) -> Self {
            change(&mut self.inner.lock());
            self
        }

        fn env(&self) -> Arc<dyn Env> {
            Arc::new(FakeEnv {
                inner: Arc::clone(&self.inner),
            })
        }

        fn stage(&self) -> StageState {
            self.inner.lock().stage
        }

        fn calls(&self) -> Vec<String> {
            self.inner.lock().calls.clone()
        }

        fn receipts(&self) -> Vec<String> {
            self.inner.lock().receipts.clone()
        }

        fn payload(&self) -> bool {
            self.inner.lock().payload
        }

        fn committed(&self) -> bool {
            self.inner.lock().committed
        }
    }

    struct FakeEnv {
        inner: Arc<Mutex<Inner>>,
    }

    struct FakeClass {
        inner: Arc<Mutex<Inner>>,
    }

    impl Env for FakeEnv {
        fn stage(&self, _id: &str) -> std::io::Result<StageState> {
            Ok(self.inner.lock().stage)
        }

        fn write_stage(&self, _id: &str, state: StageState) -> std::io::Result<()> {
            let mut inner = self.inner.lock();
            inner.stage = state;
            inner.calls.push(format!("write_stage({state:?})"));
            Ok(())
        }

        fn selected(&self, _id: &str) -> std::io::Result<u8> {
            Ok(self.inner.lock().selected)
        }

        fn payload_file(&self, relative: &str) -> Result<Vec<u8>> {
            self.inner
                .lock()
                .calls
                .push(format!("payload_file({relative})"));
            Ok(match relative {
                "bin/esuinit" => b"\x7fELF esuinit".to_vec(),
                "build-id" => b"0123456789ab\n".to_vec(),
                other => bail!("unexpected payload file {other}"),
            })
        }

        fn modules(&self, kmi: &Kmi) -> Result<BTreeMap<String, Vec<u8>>> {
            let mut inner = self.inner.lock();
            inner
                .calls
                .push(format!("modules({}-{})", kmi.branch, kmi.generation));
            inner
                .modules
                .clone()
                .ok_or_else(|| anyhow!("no module set for KMI {}-{}", kmi.branch, kmi.generation))
        }

        fn write_stage_payload(&self, _id: &str, bytes: &[u8]) -> Result<()> {
            let mut inner = self.inner.lock();
            inner.payload = !bytes.is_empty();
            inner.calls.push("write_stage_payload".to_owned());
            Ok(())
        }

        fn remove_stage_payload(&self, _id: &str) -> Result<()> {
            let mut inner = self.inner.lock();
            inner.payload = false;
            inner.calls.push("remove_stage_payload".to_owned());
            Ok(())
        }

        fn commit_payload(&self, _id: &str) -> Result<()> {
            let mut inner = self.inner.lock();
            inner.committed = true;
            inner.calls.push("commit_payload".to_owned());
            Ok(())
        }

        fn deny(&self, reason: &str) {
            let mut inner = self.inner.lock();
            inner.calls.push(format!("deny({reason})"));
            inner.receipts.push(reason.to_owned());
        }

        fn detach(&self, job: Box<dyn FnOnce() + Send + 'static>) {
            self.inner.lock().calls.push("detach".to_owned());
            job();
        }

        fn class(&self, rom: &Rom) -> Result<Box<dyn Class>> {
            let mut inner = self.inner.lock();
            ensure!(
                rom.current == inner.current,
                "class asked for a different booted letter"
            );
            inner.calls.push(format!("class({})", rom.number));
            Ok(Box::new(FakeClass {
                inner: Arc::clone(&self.inner),
            }))
        }
    }

    impl Class for FakeClass {
        fn read_target_image(&self, base: &str) -> Result<Vec<u8>> {
            let mut inner = self.inner.lock();
            inner.calls.push(format!("read_target_image({base})"));
            if inner.bases.contains(&base) {
                return Ok(inner.boot.clone());
            }
            Ok(match base {
                "xbl_config" => inner.xbl_target.clone(),
                other => bail!("no target image {other}"),
            })
        }

        fn read_current_image(&self, base: &str) -> Result<Vec<u8>> {
            let mut inner = self.inner.lock();
            inner.calls.push(format!("read_current_image({base})"));
            if inner.bases.contains(&base) {
                return Ok(inner.boot.clone());
            }
            Ok(match base {
                "xbl_config" => inner.xbl_current.clone(),
                other => bail!("no current image {other}"),
            })
        }

        fn prepare(&self) -> Result<()> {
            self.inner.lock().calls.push("class.prepare".to_owned());
            Ok(())
        }

        fn teardown(&self) -> Result<()> {
            self.inner.lock().calls.push("class.teardown".to_owned());
            Ok(())
        }

        fn promote(&self) -> Result<()> {
            let mut inner = self.inner.lock();
            inner.calls.push("class.promote".to_owned());
            if inner.promote_failures > 0 {
                inner.promote_failures -= 1;
                bail!("promote verify failed: boot");
            }
            Ok(())
        }
    }

    /// A ROM `number` record whose current slot health is `successful`.
    fn slot_record(number: u32, current: u8, selected: u8, pending: u8, successful: bool) -> Gbs1 {
        let mut record = Gbs1::initial(number, current).unwrap();
        record.selected = selected;
        record.pending = pending;
        record.slots[usize::from(current)].successful = successful;
        record
    }

    fn rom(number: u32) -> RomId {
        RomId {
            id: format!("rom{number}"),
            number,
        }
    }

    #[test]
    fn the_staged_letter_follows_the_boot_state() {
        assert_eq!(staged_letter(0, 1, StageState::None), None);
        assert_eq!(staged_letter(0, 1, StageState::Staging), Some(1));
        assert_eq!(staged_letter(1, 0, StageState::Staging), Some(0));
        assert_eq!(staged_letter(0, 0, StageState::Sealed), Some(0));
        assert_eq!(staged_letter(0, 1, StageState::Promote), Some(1));
        assert_eq!(suffix(0), "_a");
        assert_eq!(suffix(1), "_b");
    }

    #[test]
    fn rom_one_stages_nothing_of_its_own() {
        let context = Rom {
            id: "rom1".to_owned(),
            number: 1,
            current: 0,
            target: 1,
            booted_staged: false,
        };
        let class = Rom1::new(&context);
        class.prepare().unwrap();
        class.teardown().unwrap();
        class.promote().unwrap();
    }

    #[test]
    fn an_idle_unbootable_prepares_the_set_and_records_staging() {
        let fake = Fake::new(0, StageState::None, 0, vec!["boot", "dtbo"]);
        let env = fake.env();
        let mut txn = Txn::new(0);
        let record = slot_record(2, 0, 0, NO_PENDING, true);
        txn.start(&env, &rom(2), &record, MergeStatus::None)
            .unwrap();

        txn.commit(
            &env,
            &rom(2),
            &record,
            MergeStatus::None,
            Operation::SetUnbootable(1),
        )
        .unwrap();

        assert_eq!(
            fake.calls(),
            ["class(2)", "class.prepare", "write_stage(Staging)"]
        );
        assert_eq!(fake.stage(), StageState::Staging);
    }

    #[test]
    fn a_booted_staged_letter_never_restarts_the_set() {
        let fake = Fake::new(0, StageState::Sealed, 0, vec!["boot"]);
        let env = fake.env();
        let mut txn = Txn::new(0);
        // Not successful and no merge: the start must not promote either.
        let record = slot_record(2, 0, 0, NO_PENDING, false);
        txn.start(&env, &rom(2), &record, MergeStatus::Merging)
            .unwrap();

        // update_engine's post-merge `MarkSlotUnbootable(old)`.
        txn.commit(
            &env,
            &rom(2),
            &record,
            MergeStatus::Merging,
            Operation::SetUnbootable(1),
        )
        .unwrap();

        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        assert_eq!(fake.stage(), StageState::Sealed);
    }

    #[test]
    fn a_rom_one_boot_larger_than_the_firmware_probe_limit_seals() {
        use std::io::Write;
        use std::os::fd::FromRawFd;

        let mut image = boot_image(b"Linux version 6.12.23-android16-6-g1a2b3c4d (x) #1 SMP");
        let kernel_size = 25 * 1024 * 1024u32;
        image[8..12].copy_from_slice(&kernel_size.to_le_bytes());
        image.resize(4096 + kernel_size as usize, 0);
        // SAFETY: memfd_create takes a live NUL-terminated name and no pointers it retains.
        let fd = unsafe { libc::memfd_create(c"large-boot-fixture".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "{}", std::io::Error::last_os_error());
        // SAFETY: the successful call returned a new descriptor; this File owns it once.
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        file.write_all(&image).unwrap();
        let path = std::path::PathBuf::from(format!("/proc/self/fd/{fd}"));
        let boot = read_partition_node("boot", &path).unwrap();
        assert_eq!(boot, image);
        let firmware_probe = read_partition_node("xbl_config", &path).unwrap();
        assert_eq!(firmware_probe.len() as u64, platform::physical_limit());
        assert_eq!(
            kmi_from_boot(&firmware_probe).unwrap_err().to_string(),
            "truncated boot image"
        );

        let fake = Fake::new(0, StageState::None, 0, vec!["boot"]).with(|inner| inner.boot = boot);
        let env = fake.env();
        let mut txn = Txn::new(0);
        let record = slot_record(1, 0, 0, NO_PENDING, true);
        txn.start(&env, &rom(1), &record, MergeStatus::None)
            .unwrap();
        txn.commit(
            &env,
            &rom(1),
            &record,
            MergeStatus::None,
            Operation::SetActive(1),
        )
        .unwrap();
        assert_eq!(fake.stage(), StageState::Sealed);
        assert!(fake.payload());
        assert!(fake.receipts().is_empty());
    }

    #[test]
    fn an_active_request_seals_the_update_and_a_denial_leaves_the_record() {
        let fake = Fake::new(0, StageState::None, 0, vec!["boot"]);
        let env = fake.env();
        let mut txn = Txn::new(0);
        let record = slot_record(2, 0, 0, NO_PENDING, true);
        txn.start(&env, &rom(2), &record, MergeStatus::None)
            .unwrap();

        txn.commit(
            &env,
            &rom(2),
            &record,
            MergeStatus::None,
            Operation::SetActive(1),
        )
        .unwrap();

        let calls = fake.calls();
        assert_eq!(calls[0], "class(2)");
        assert!(calls.contains(&"read_target_image(boot)".to_owned()));
        assert!(calls.contains(&"read_target_image(xbl_config)".to_owned()));
        assert!(calls.contains(&"read_current_image(xbl_config)".to_owned()));
        assert!(calls.contains(&"modules(android16-6.12-6)".to_owned()));
        assert!(calls.contains(&"payload_file(bin/esuinit)".to_owned()));
        assert!(calls.contains(&"payload_file(build-id)".to_owned()));
        assert!(calls.contains(&"write_stage_payload".to_owned()));
        assert_eq!(fake.stage(), StageState::Sealed);
        assert!(fake.payload());

        // A payload without a module set for the target KMI is a denial: the
        // receipt names it and the record is never written.
        let denied = Fake::new(0, StageState::None, 0, vec!["boot"]).with(|inner| {
            inner.modules = None;
        });
        let env = denied.env();
        let mut txn = Txn::new(0);
        let record = slot_record(2, 0, 0, NO_PENDING, true);
        txn.start(&env, &rom(2), &record, MergeStatus::None)
            .unwrap();
        let error = txn
            .commit(
                &env,
                &rom(2),
                &record,
                MergeStatus::None,
                Operation::SetActive(1),
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("no module set for KMI android16-6.12-6"),
            "{error}"
        );
        assert_eq!(denied.receipts().len(), 1);
        assert_eq!(denied.stage(), StageState::None);
        assert!(!denied.payload());
    }

    #[test]
    fn a_rom_one_arb_raise_is_refused_and_an_equal_index_is_not() {
        // The target firmware raises the index: ROM 1 executes what it writes,
        // so the update is refused with a receipt.
        let raised = Fake::new(0, StageState::None, 0, vec!["boot"]).with(|inner| {
            inner.xbl_target = xbl_config(9);
        });
        let env = raised.env();
        let mut txn = Txn::new(0);
        let record = slot_record(1, 0, 0, NO_PENDING, true);
        txn.start(&env, &rom(1), &record, MergeStatus::None)
            .unwrap();
        let error = txn
            .commit(
                &env,
                &rom(1),
                &record,
                MergeStatus::None,
                Operation::SetActive(1),
            )
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "anti-rollback raised: xbl_config ARB 9 > 3"
        );
        assert_eq!(raised.receipts(), [error.to_string()]);
        assert_eq!(raised.stage(), StageState::None);
        assert!(!raised.payload());

        // An equal index is not a raise: the seal proceeds.
        let equal = Fake::new(0, StageState::None, 0, vec!["boot"]).with(|inner| {
            inner.xbl_target = xbl_config(3);
        });
        let env = equal.env();
        let mut txn = Txn::new(0);
        let record = slot_record(1, 0, 0, NO_PENDING, true);
        txn.start(&env, &rom(1), &record, MergeStatus::None)
            .unwrap();
        txn.commit(
            &env,
            &rom(1),
            &record,
            MergeStatus::None,
            Operation::SetActive(1),
        )
        .unwrap();

        assert_eq!(equal.stage(), StageState::Sealed);
        assert!(equal.receipts().is_empty());
    }

    #[test]
    fn a_cancel_while_idle_drops_the_set() {
        for stage in [StageState::Staging, StageState::Sealed] {
            let fake = Fake::new(0, stage, 1, vec!["boot"]);
            let env = fake.env();
            let mut txn = Txn::new(0);
            let record = slot_record(2, 0, 1, NO_PENDING, false);
            txn.start(&env, &rom(2), &record, MergeStatus::Merging)
                .unwrap();

            txn.commit(
                &env,
                &rom(2),
                &record,
                MergeStatus::Merging,
                Operation::SetActive(0),
            )
            .unwrap();

            assert_eq!(
                fake.calls(),
                [
                    "remove_stage_payload",
                    "class(2)",
                    "class.teardown",
                    "write_stage(None)"
                ],
                "{stage:?}"
            );
            assert_eq!(fake.stage(), StageState::None);
        }
    }

    #[test]
    fn a_successful_boot_of_the_staged_letter_promotes() {
        let fake = Fake::new(0, StageState::Sealed, 0, vec!["boot"]);
        let env = fake.env();
        let mut txn = Txn::new(0);
        // The merge is still running, so the start does not promote yet.
        let record = slot_record(2, 0, 0, NO_PENDING, false);
        txn.start(&env, &rom(2), &record, MergeStatus::Merging)
            .unwrap();
        assert!(fake.calls().is_empty());

        txn.commit(
            &env,
            &rom(2),
            &record,
            MergeStatus::None,
            Operation::MarkSuccessful(0),
        )
        .unwrap();

        assert_eq!(
            fake.calls(),
            [
                "class(2)",
                "write_stage(Promote)",
                "detach",
                "class.promote",
                "commit_payload",
                "write_stage(None)"
            ]
        );
        assert!(fake.committed());
        assert_eq!(fake.stage(), StageState::None);
    }

    #[test]
    fn a_finished_merge_promotes_the_running_staged_letter() {
        let fake = Fake::new(0, StageState::Sealed, 0, vec!["boot"]);
        let env = fake.env();
        let mut txn = Txn::new(0);
        let record = slot_record(2, 0, 0, NO_PENDING, true);
        txn.start(&env, &rom(2), &record, MergeStatus::Merging)
            .unwrap();
        assert!(fake.calls().is_empty());

        txn.merge_completed(
            &env,
            &rom(2),
            &record,
            MergeStatus::Merging,
            MergeStatus::None,
        )
        .unwrap();

        assert!(fake.calls().contains(&"class.promote".to_owned()));
        assert!(fake.committed());
        assert_eq!(fake.stage(), StageState::None);
    }

    #[test]
    fn a_start_in_promote_resumes_the_promote() {
        let fake = Fake::new(0, StageState::Promote, 0, vec!["boot"]);
        let env = fake.env();
        let mut txn = Txn::new(0);
        let record = slot_record(2, 0, 0, NO_PENDING, true);

        txn.start(&env, &rom(2), &record, MergeStatus::Merging)
            .unwrap();

        assert!(fake.calls().contains(&"class.promote".to_owned()));
        assert!(fake.committed());
        assert_eq!(fake.stage(), StageState::None);
    }

    #[test]
    fn a_failed_promote_keeps_the_promote_state() {
        let fake = Fake::new(0, StageState::Promote, 0, vec!["boot"]).with(|inner| {
            inner.promote_failures = 1;
        });
        let env = fake.env();
        let mut txn = Txn::new(0);
        let record = slot_record(2, 0, 0, NO_PENDING, true);

        txn.start(&env, &rom(2), &record, MergeStatus::Merging)
            .unwrap();

        assert!(fake.calls().contains(&"class.promote".to_owned()));
        assert!(!fake.calls().contains(&"commit_payload".to_owned()));
        assert!(!fake.committed());
        assert_eq!(fake.stage(), StageState::Promote);
    }

    #[test]
    fn a_rom_one_boot_of_the_other_letter_cancels_the_staged_payload() {
        // Confirmed switch, but this boot still runs the old letter: the staged
        // payload is never used, so it is dropped.
        let fake = Fake::new(0, StageState::Sealed, 1, Vec::new());
        let env = fake.env();
        let mut txn = Txn::new(0);
        let record = slot_record(1, 0, 1, NO_PENDING, true);

        txn.start(&env, &rom(1), &record, MergeStatus::None)
            .unwrap();

        assert_eq!(
            fake.calls(),
            [
                "remove_stage_payload",
                "class(1)",
                "class.teardown",
                "write_stage(None)"
            ]
        );
        assert_eq!(fake.stage(), StageState::None);
    }

    #[test]
    fn a_promote_retries_one_verify_mismatch_and_then_fails() {
        let mismatches = std::cell::Cell::new(1u32);
        let copies = std::cell::Cell::new(0u32);
        let copy = |_base: &str| {
            copies.set(copies.get() + 1);
            Ok(())
        };
        let verify = |_base: &str| Ok(mismatches.replace(mismatches.get().saturating_sub(1)) == 0);
        copy_back(&["boot"], copy, verify).unwrap();
        assert_eq!(copies.get(), 2, "one retry after the first mismatch");

        let always = |_base: &str| Ok(false);
        let error = copy_back(&["boot"], copy, always).unwrap_err();
        assert_eq!(error.to_string(), "promote verify failed: boot");
        assert_eq!(copies.get(), 4, "two attempts per base, no more");

        let two = std::cell::Cell::new(0);
        let error = copy_back(
            &["boot", "dtbo"],
            |_base| {
                two.set(two.get() + 1);
                Ok(())
            },
            |base| Ok(base == "dtbo"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "promote verify failed: boot");
        assert_eq!(two.get(), 2);
    }
}
