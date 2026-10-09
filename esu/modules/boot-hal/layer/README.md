# Managed boot-control HAL

**Status (2026-10-08)** - One module, `esu/modules/boot-hal/`: the generic-bootctl
submodule (`../generic-bootctl`) is linked only as a serving library, and this
directory is the esu layer beside it. generic-bootctl serves the service (AIDL V1 first,
HIDL 1.0-1.2 only if servicemanager refuses AIDL), owns slot health policy and the frozen
AIDL V1 dispatch; `EsuBackend` adapts the project efivarfs records and the misc VAB mirror
to its `Backend` trait, and drives the ROM OTA transaction (`src/txn.rs`) from the same
mutations. Replacing the stock HAL is this module's payload method: its own
init service stops the stock service by exec label, then serves. Registration is never
gated on efivarfs, BootedRom, managed Slot state or misc. Verified on a phone for AIDL V1
(normal ROM1 boot); HIDL, recovery and the OTA transaction on a device are not claimed
(the transaction's state machine is host-tested, its storage side is not).

```
esu/modules/boot-hal/
  module.prop sepolicy.rule initrc/ the ESP module files the packager ships
  generic-bootctl/                  submodule: pinned upstream, never edited here
  layer/                            this crate: backend, wire records, stock stop,
                                    OTA transaction, platform, entry point
```

## Architecture

```
bootctl-unified (lib)  serve(service): AIDL first, HIDL 1.2/1.1/1.0 if refused
generic-bootctl-core   Service: slot health policy, -1/-2 mapping, write gate
        |              Backend trait: slot_count / prepare / read_state / commit / read_merge / write_merge
gobbl-boot-hal         EsuBackend + GBS1/GBM1 wire records + misc mirror + stock stop + the esu Service
        |
        +-- txn::Txn   the OTA transaction: prepare / seal / cancel / promote
        |     txn::Env      records, payload tree, denial receipt, detached promote
        |     txn::Class    Rom1 (physical partitions) / RomN (staging LVs + switch devices)
        +-- android::Android  the served Env: efivarfs, ESP payload tree, module sets
        +-- platform          ESP mount and RW window, loop attach, lvm/esud, log, notify
```

`src/backend.rs` implements `generic_bootctl_core::Backend`:

- `slot_count` is the constant 2, so `getNumberSlots`/`getSuffix` answer without
  reading efivarfs or resolving `BootedRom`.
- `prepare` is the startup/next-operation reconciliation: resolve the booted
  catalogue id, validate this ROM's `Slot-<id>` record, then mirror its
  `MergeStatus-<id>` into misc. A failure is retried by the next state-dependent
  transaction; the shared service calls it for the state-dependent transactions
  only (not for current slot, slot count, suffix or the two metadata calls).
- `read_state`/`commit` project the 24-byte GBS1 record; `read_merge`/`write_merge`
  persist the 8-byte GBM1 record and then flush/readback the 64-byte misc mirror.
- Writes go to this booted ROM's variables only; the record's ROM number, not the
  catalogue id, selects the ROM 1 versus ROM >= 2 policy.

`src/wire.rs` owns the native layouts and the projection rules they imply:

- GBS1 has no separate bootable bit: **priority 0 is unbootable**. The shared
  model expresses that as `tries == 0`, so the encoder writes priority 0 exactly
  when `tries` is 0 and at least 1 otherwise. A foreign record holding priority 0
  with stale nonzero tries reads as unbootable, exactly as this HAL always
  reported it, and loses those stale tries the next time that record is rewritten.
- The shared service derives `getActiveBootSlot` from the highest-priority slot.
  GBS1's own boot target (pending request, else confirmed/selected slot) always
  holds the strict maximum priority for every record this HAL, Surfacer or the
  provisioners write, so both answers agree; `Gbs1::booted_slot` documents that
  record-derived value and is asserted against the service in the host tests.
- Success uses `HealthOnSuccess::PreserveNonZero`: `markBootSuccessful` keeps an
  existing nonzero try count (7 stays 7) and raises a zero priority to 1, which is
  the GBS1 policy Surfacer reads. The AOSP misc backends keep the shared core's
  reset-to-one policy; neither affects the other.
- `setSlotAsUnbootable` is the shared zeroing mutation plus the projection's
  priority clear, and it clears a matching pending request without selecting an
  alternate slot.
- ROM 1 keeps `selected` and records a `pending` firmware request (requesting the
  confirmed slot cancels it); ROM >= 2 selects immediately and never carries a
  pending byte.

**Write gate.** The esu HAL is always writable: the service is constructed with a
gate that returns `true`. Writes are gated outside this process by the install
decision and ROM isolation, and the esu AIDL contract has no read-only rollout phase.
`persist.generic_bootctl.rw` belongs to the generic standalone module, which this
payload does not use.

## OTA transaction

The same AIDL mutations that move slot health also drive the ROM OTA, because the
updater's own sequence is what the transaction has to hook: it marks the target
letter unbootable, writes the target, seals it, boots it, marks it successful and
finally merges. `src/txn.rs` owns that state machine and is host-tested end to end
against a recorder; `src/android.rs` and `src/platform.rs` are the real records,
paths and storage behind it.

**Letters and state.** Nothing about the staged set is remembered in this process:
the booted letter comes from `ro.boot.slot_suffix`, the selected letter from
`Slot-<id>` byte 8, and the transaction state from the bdsvars `Stage-<id>` record
(`esu_platform::stage`). The staged letter `L` is `other(current)` while `Staging`,
`selected` once `Sealed`/`Promote`, and none otherwise. A malformed `Stage-<id>`
record is `InvalidData`, so the state-dependent transaction fails closed instead of
guessing. The one thing that *is* snapshotted is `booted_staged` = `Stage` is
`Sealed`/`Promote` and `selected == current`, taken once at service start: while the
switch devices serve the letter this boot runs from, removing the staging set they
point at would turn the running Android's own partitions into I/O errors.

**Per-mutation actions** (`Operation` → side effects, before the `Slot-<id>` write):

| mutation | ROM ≥ 2 | ROM 1 |
| --- | --- | --- |
| `SetUnbootable(t)`, `t != current` | `booted_staged` ⇒ log only (also the post-merge mark of the old slot); `None` ⇒ create the staging set and record `Staging`; `Staging` ⇒ resume; `Sealed` ⇒ cancel then stage; `Promote` ⇒ log only | `L == t` ⇒ cancel |
| `SetActive(t)`, `t != current` | refuse when `booted_staged`; a re-request of the letter already sealed is a retry; otherwise seal the takeover archive and record `Sealed` | same |
| `SetActive(current)` | `Staging`/`Sealed` and not `booted_staged` ⇒ cancel; while `booted_staged` the staged set is what this boot runs, so the cancel is logged and the promote path owns it | same |
| `MarkSuccessful` | `Sealed`, `current == L`, merge NONE ⇒ promote | same, and only after Surfacer confirmed the switch (`pending` cleared) |
| `write_merge(NONE)` after `MERGING` | `Sealed`, slot successful, `current == L` ⇒ promote | same |
| service start | `Promote` ⇒ resume the promote; `Sealed` + successful + merge NONE + `current == L` ⇒ promote | `Sealed`, no pending, `current != L` ⇒ cancel |

Slot activation preserves the successful bit, matching the stock Android misc
backend; `SetUnbootable` clears it before a new target is written. In particular,
selecting the running slot to cancel must not erase its confirmed boot health:
update_engine caches its boot-flags action and otherwise waits indefinitely for
a second `markBootSuccessful` before starting the next update.

A refusal (no module set for the target KMI, a proven ROM 1 anti-rollback raise, a
staged set that would be restaged while this boot runs it) returns the error the
service maps to `COMMAND_FAILED` **before** the record write, so the `Slot-<id>`
record still describes the letter this boot runs; the reason goes to the receipt,
the kernel log and a notification.

**Storage (ROM ≥ 2).** Every declared base image of the ROM is staged in its own
thick logical volume `rom<N>-stage-<base>` inside the `rom` volume group, created
with the static payload `lvm` (`/debug_ramdisk/esu/bin/lvm`, every invocation passing
`--config` with the shipped `esu/bin/lvm.conf` text) and pre-filled with the running
image so the updater's verifier and incremental patches see the bytes they expect.
The switch device `rom<N>-ota-<base>` — created by `ota-stage` at PID 1 — is reloaded
to a linear map over the staging LV (writable, because the target letter is the one
the updater writes), and back to an error target of the image's exact size when the
set is removed. `esud esd refresh` re-publishes the device-name tree after every
create/remove, and the nodes are opened as
`/dev/block/esd/lv/<lv>`, `/dev/block/esd/by-name/<PARTNAME>` and
`/dev/block/esd/mapper/control`.
Promotion resolves each allocated loop using `/sys/block/loop<N>/dev`,
requires major 7, and opens or creates Android's `/dev/block/loop<N>` with the
kernel-published minor (including `loop.max_part` partition geometry)
without following symlinks. An attachment guard holds its autoclear descriptor
until the switch reload acquires a reference; dropping it before reload would
detach the image while the running letter still needs it.

**Seal.** The target's kernel identity comes from the *staged* `boot` image
(`kmi_from_boot`), the module set for that KMI is selected from the payload
(`<esp>/esu/kmi/<branch>-<generation>/`, verified against `set.json`), the takeover
overlay is rebuilt from `<esp>/esu/bin/esuinit`, `<esp>/esu/build-id` and those
modules, and it replaces `<esp>/rom/<id>/esu.stage.cpio` through a temporary file
and a rename inside one read-write window on the otherwise read-only ESP. A missing
module set is a denial, never a fallback to the running kernel's set. ROM 1 only
executes the firmware the update writes, so only ROM 1 refuses a target
`xbl_config` whose anti-rollback index is higher than the running one; for a managed
ROM the comparison is logged. An image the port cannot read or recognize is logged
rather than refused (the refusal is defined as a *proven* raise), except a target
`xbl_config` that cannot be read at all, which is an incomplete payload.

For ROM 1, sealing reads the physical `boot_<letter>` through the esd tree,
bounded by that partition's `BLKGETSIZE64` size (regular-file length in host
tests). It must not use the separate 20 MiB `xbl_config`/ARB probe limit:
the OnePlus 15 boot kernels exceed it, and a capped boot image fails KMI parsing
as truncated before any slot or Stage record is written.

**Promote.** `Stage` becomes `Promote` first, so a process that dies mid-copy
resumes the same step at the next start. The staged bytes are copied back over the
ESP base images inside one read-write window and verified; a mismatch is retried
once and then leaves `Stage` at `Promote` with the reason logged. While this boot
runs the staged letter, each switch device is then reloaded to a read-only loop over
the promoted ESP image, so the running Android keeps reading the bytes it booted
(`LO_FLAGS_READ_ONLY | LO_FLAGS_AUTOCLEAR`, released when the switch device stops
referencing it). The staging LVs are removed afterwards, and the archive is renamed
`esu.stage.cpio` → `esu.cpio` before `Stage` returns to `None`. Every step is
idempotent, so a promote interrupted between the removal and the rename converges on
the next start instead of stalling at `Promote`: a staging volume that is already
gone means the copy already ran, and a staged archive that is already gone means the
rename did. A promote copies whole images, so it runs on a detached thread and never
blocks the Binder thread.

**ROM 1** has no staging set: the projection already routes the target letter to the
physical partitions the updater writes, and this HAL's `Rom1` class is a no-op for
prepare/teardown/promote. What ROM 1 does stage is the takeover payload, which
Surfacer boots for the selected letter once the firmware switch is confirmed.

**Paths.** `/debug_ramdisk/esp` is the runtime ESP mount (the loader's
`esuinit::esp::ESP_MOUNT_POINT`), `/debug_ramdisk/esp/esu` the payload tree,
`/debug_ramdisk/esu/bin` the executable tmpfs the payload's `lvm`/`esud` run from.
`src/platform.rs` pins those against the loader's constants and bakes the shipped
`lvm.conf` in with `include_str!`, so an invocation cannot depend on a readable
confdir. The command argument drops full-line comments and joins the shipped
lines once: LVM copies argv into its metadata description without escaping
newlines, so passing a multiline `--config` would corrupt the next VG read.

**Denial receipt.** `esu/receipts/ota-denied.txt` under the runtime payload root
(`/debug_ramdisk/esp/esu/receipts/ota-denied.txt`), written tmp+fsync+rename inside a
read-write window, followed by `__android_log_print` under the `esu-bootctl` tag and
a non-blocking `/system/bin/cmd notification post -S bigtext -t "esu OTA" esu.ota`.
The `ota` module's `boot-completed.sh` posts from the receipt and deletes it when the
notification command itself was refused.

**Policy.** The transaction's allows in `esu/modules/boot-hal/sepolicy.rule` are
written against the platform type names the code opens (`esu_blk_device` for the esd
tree, `vfat` for the ESP, `dm_device`/`loop_control_device`/`loop_device` for the
block layer, `sysfs_type` for the device-mapper name scan, `esu_file` for the payload
binaries, `system_file`/`system_server` for the notification) and are **unverified on
a device**: the real set is harvested from a permissive boot before this module is
deployed enforcing. The `esu_blk_device` type itself is declared by the `ota`
module, so the two modules' rules are loaded together.

**Host tests.** `src/txn.rs` drives the whole table above with a recorder: prepare,
resume, booted-staged, seal, denial (record untouched + receipt), cancel, promote
from `MarkSuccessful` and from a finished merge, resume from `Promote`, a failed
promote staying at `Promote`, the ROM 1 cancel, the ROM 1 anti-rollback refusal and
the per-base verify retry. `tests/efivarfs.rs` keeps the record/mirror/service
contract with a benign platform behind it. The storage classes' own bodies (lvm, dm,
loop, copies) need a device and are not covered here.

**Replacing the stock HAL.** `initrc/boot-hal.rc` runs `esu-bootctl --stop-stock` as an
`on post-fs` exec in the platform's `esu` domain: `src/stock.rs` stops every init service
whose command carries `hal_bootctl_default_exec` (`ctl.stop`; a not-yet-started service is
disabled), so the stock HAL never starts at `class_start early_hal`. Discovery
includes partition init directories and each mounted APEX's `etc` and `etc/init`
directories: APEX service definitions are imported before post-fs too. Selection
still uses the executable's SELinux label, never a service-name heuristic.
Reading init scripts and sending `ctl.stop` are platform work; from a confined
`esu_bootctl` the scan was refused by the device's blanket `dontaudit domain
file_type:{dir,file}` rules and silently found nothing. Init answers control messages while an
exec runs, but not while its main thread sits in `mount_all` at `late-fs` with vold waiting
on IBootControl; a stop issued by the serving process there deadlocked an enforcing boot
(2026-10-08, 900e dump `20261008T105049Z-900e-2`: servicemanager's `interface_start` for
IBootControl dropped once a second as "Too many pending control messages"). The service
itself, `esu.bootctl` (`class early_hal`, `critical`, `seclabel u:r:esu_bootctl:s0`), only
serves. The module's `sepolicy.rule` creates the enforced `esu_bootctl` domain in
`hal_bootctl_server`: its own `add`/`find`, init's nosuid transition from the tmpfs binary,
and the per-type grants the compiled policy expanded away from a type created after
compilation (binder device and binder ioctls, own `/proc` entries, the logd socket,
`servicemanager.ready`, misc for the VAB mirror). It clears `add` on the
`hal_bootctl_server` attribute key and the concrete stock domain, because kernelesp's
`deny` only edits the exact avtab key and AOSP grants `add` through the attribute.

The core additions this consumer needed - `Backend::slot_count`, the default
`Backend::prepare` hook, `HealthOnSuccess::{ResetToOne, PreserveNonZero}`, slot-index
validation before any storage access and the serving-only `bootctl_unified::serve`
entry - live upstream, so no slot or transport logic is forked here.

## Build and Binder choice

From the product worktree (the submodule must be checked out:
`git submodule update --init esu/modules/boot-hal/generic-bootctl`):

```sh
cargo +nightly-2026-08-08 test --manifest-path esu/modules/boot-hal/layer/Cargo.toml --offline --jobs 3
cargo +nightly-2026-08-08 clippy --manifest-path esu/modules/boot-hal/layer/Cargo.toml --all-targets -- -D warnings
ESU_NDK=/path/to/android-ndk-r29 bash esu/modules/boot-hal/layer/build-android.sh            # aarch64
ESU_NDK=/path/to/android-ndk-r29 bash esu/modules/boot-hal/layer/build-android.sh x86_64     # Cuttlefish
```

The build uses the explicitly supplied NDK, API 35 and the installed
`aarch64-linux-android`/`x86_64-linux-android` Rust target; it links `-llog` for the
`__android_log_print` the denial path logs with. Output:
`esu/modules/boot-hal/layer/target/<triple>/release/gobbl-boot-hal`.
When supplied, the packager installs it as `/esu/bin/esu-bootctl`, run by the module's
`esu.bootctl` init service. There is no ELF generation note; `esu/build-id` and cpio
`/esu-build-id` identify the complete payload, and a mismatch is logged rather than pinned.

The upstream crates are path dependencies into the submodule, so nothing is fetched and
the build stays `--offline`. The layer links `bootctl-unified` with
`default-features = false`: the native AOSP/QTI backends and the vendor-library probe are
not part of this binary. The submodule pin is the provenance record; its commit is
unpublished until the owner pushes generic-bootctl.

Rust, the state machine and shared `esu-platform` are **statically linked**. Binder
uses the platform `libbinder_ndk.so` C ABI; no AOSP build tree, generated AIDL
shared library, vendor QTI library or C++ runtime is needed.
The vendored transport implements the frozen V1 transaction order, status headers,
Boolean/string/int replies and the two stable-interface metadata transactions.
NDK headers supply the public ABI; four platform-only symbols are resolved with
checked `dlsym`, since app-NDK stubs omit service registration/thread-pool/VINTF
entrypoints. The service retains its binder object/class for process lifetime.
The main thread is the only Binder worker.

**This is not a fully static ELF.** The explicitly allowed dynamic Binder choice
requires Android's dynamic linker/bionic. The artifact is an AArch64 ELF64 PIE,
interpreter `/system/bin/linker64`, with direct dependencies `libbinder_ndk.so`,
`libc.so` and `libdl.so` - unchanged by this cutover. A literal no-PT_INTERP/
static-bionic requirement is incompatible with this approach: static bionic does
not support loading the platform Binder library. Do not advertise this artifact as
a fully static executable or deploy it before the system linker is available.
The HAL runs as `early_hal`, after system/vendor mounts, not as ramdisk PID 1.

Tests cover record/confirm versus record-only selection, cancellation/retry/success,
malformed state, source-slot VAB reversion, lazy identity recovery, AIDL storage
failures, byte-exact efivarfs writes and preservation of other-ROM variables,
BCB/bootloader-control bytes, valid V2 VAB reserved bytes, storage-independent slot
queries, invalid-slot precedence and the GBS1 success/unbootable health policy, plus
the whole OTA transaction table above (staging, resume, seal, denial, cancel,
promote, resume-after-promote, verify retry, ROM 1 anti-rollback) and the pinned
platform layout (ESP paths, remount flags, loop status layout, lvm configuration).

## Installation and identity

The ESP `boot-hal/module.prop` declares this ordinary module. The module itself has no
`critical` marker (its policy failure is reported without the module loader rebooting
Android), but its init service is `critical`: if `esu.bootctl` exits more than four times
before `boot_completed`, init's fatal path runs (a panic under
`androidboot.init_fatal_panic=true`, otherwise a reboot to the bootloader), instead of
leaving vold waiting on `IBootControl` forever. Omitting or disabling the module leaves the
stock HAL untouched.

- Init service: **`esu.bootctl`**, `class early_hal`, root/root, `seclabel
  u:r:esu_bootctl:s0`, binary `/debug_ramdisk/esu/bin/esu-bootctl`.
- OTA transaction dependencies: the `ota` module (it declares the
  `esu_blk_device` type these rules use, publishes `/dev/block/esd/` at `early`
  and creates the `lvm` symlinks) and the payload files `esu/bin/lvm`,
  `esu/bin/lvm.conf`, `esu/bin/ota-stage`, `esu/bin/esuinit`, `esu/build-id`,
  `esu/roms/<id>.toml`, `esu/kmi/<branch>-<generation>/` and the ROM's
  `rom/<id>/<base>.img` images. Without them a managed ROM's transaction fails
  closed with `COMMAND_FAILED`; ROM 1 boots are unaffected.
- Binder identity: **`android.hardware.boot.IBootControl/default`**.
- Version: **1**; hash: **`2400346954240a5de495a1debc81429dd012d7b7`**.
- The device VINTF manifest is unchanged; the stock service is stopped, never redefined.
- The HAL opens project efivarfs variables, `/dev/block/by-name/misc` and, for the
  OTA transaction, the nodes esud publishes under `/dev/block/esd/` (staging
  volumes, projected-away partitions, the device-mapper control node) plus the ESP
  base images it promotes; never a whole LU, GPT, UFS sysfs node or boot partition.

`BootedRom` in the project EFI namespace selects the managed catalogue ID.
Missing identity or `direct` means unmanaged. Invalid/unavailable identity is
retried on state-dependent transactions. No `ro.boot.esu.rom` property is read.
`ro.boot.slot_suffix` must be exactly `_a` or `_b` and remains **the only
current-slot authority**, never the pending/selected record.

ROM number/class comes from the provisioned Slot record, never an ID guess.
The Phase 3.2 fail-closed contract deliberately overrides the generic Phase
3.5b missing-state default: without Slot there is no authoritative ROM number.
Missing/invalid managed Slot returns `COMMAND_FAILED`; missing merge defaults
to NONE/source current. Unmanaged identity also fails managed operations.
Storage/identity/misc failure never prevents registration: initial mirror repair
is best effort and deferred to the next state-dependent transaction on failure.
Current-slot, slot-count, suffix and frozen version/hash replies remain available.

Recovery retains its projection and `pid1-recovery.sh` scripts for modules marked
`recovery-ok`; this module is not `recovery-ok`, so recovery keeps its stock HAL.
No native writer is invoked as a fallback. Recovery AIDL/HIDL parity is an unproved
integration prerequisite. The OTA transaction intercepts the updater's writes: the
target letter of a managed ROM is projected onto the staging switch device, so esu's
partition projection remains mandatory before any managed OTA.

Integration must permit project efivarfs reads/writes and existing misc access.
The HAL uses only an in-process Mutex, not block-device `flock`. On a device the
service form is proved for an enforcing normal ROM1 boot with `esu_bootctl` itself
enforced (2026-10-08, payload `ef876a4dc2d5`, `20261008T113307Z-phone-loop`): the stop
exec ran as `u:r:esu:s0` and disabled `vendor.boot-qti` at 2.45 s, `esu.bootctl`
registered at 2.57 s, vold, update_verifier and update_engine used it, and no denial named
`esu_bootctl`; AIDL reads and `markBootSuccessful` passed on a kept-up boot
(`20261008T113504Z-phone-efvs-hal-mark`). Recovery and HIDL remain open.

## Variable namespace and wire layout

Vendor namespace GUID: **`7a5e4b1c-0d3f-4e62-9b8a-1c2d3e4f5a6b`**.
`esu-platform::efivars::PROJECT_GUID` is the shared namespace authority.
efivarfs filenames are `<variable-name>-<GUID>`; files contain a four-byte
little-endian attributes prefix followed by the variable payload. Attributes
are exactly **7 (NV | BS | RT)**. Store parsing, append/reclaim and persistence
belong to the EFI backend, not to this HAL.

### `Slot-<id>`: 24 bytes

| Byte range | Value |
| --- | --- |
| 0–3 | ASCII `GBS1` (schema 1) |
| 4–7 | nonzero ROM number, u32 little-endian; 1 is stock/record-and-confirm |
| 8 | selected slot 0/1: confirmed firmware slot for ROM 1; HLOS selection for ROM ≥2 |
| 9 | pending firmware request 0/1, or `ff` for none; only ROM 1 may have a pending request |
| 10–11 | zero reserved |
| 12,13,14,15 | slot A priority 0–15, tries 0–7, successful 0/1, zero reserved |
| 16,17,18,19 | slot B priority, tries, successful, zero reserved |
| 20–23 | zero reserved |

`PendingSwitch-<id>` is intentionally **not a second variable**. Embedding the
pending byte in `Slot-<id>` gives activation/cancellation a single edk2 record
commit rather than a torn multi-variable transaction. Surfacer must read this
same schema; only confirmed Surfacer activation updates ROM 1 byte 8 and clears
byte 9 after its GPT/UFS operation. Boot-attempt decrement and the all-ROM merge
guard belong to Surfacer, not this userspace HAL. Priority 0 is the unbootable
marker on this wire: the HAL never writes a nonzero try count with priority 0.

### `MergeStatus-<id>`: 8 bytes

| Byte range | Value |
| --- | --- |
| 0–3 | ASCII `GBM1` (schema 1) |
| 4 | AIDL MergeStatus: NONE=0, UNKNOWN=1, SNAPSHOTTED=2, MERGING=3, CANCELLED=4 |
| 5 | source slot 0/1, captured from current slot when status is set |
| 6–7 | zero reserved |

The physical VAB mirror occupies **misc byte 32768 through 32831 only**:
version 2 at +0; LE magic `0x56740ab0` at +1; status at +5; source at +6; 57
reserved bytes at +7. Existing valid V2 reserved bytes are preserved. No BCB or
stock `bootloader_control` bytes are touched. Startup attempts to mirror the
booted ROM's raw status/source; failure is retried before a state-dependent
transaction. Setters persist that ROM's variable before flushing/readback of
the physical mirror. Mirror failure leaves authority committed for reconciliation.
No other ROM's variable is inferred from or overwritten by misc.

### `Stage-<id>`: 8 bytes

| Byte range | Value |
| --- | --- |
| 0–3 | ASCII `GBT1` (schema 1) |
| 4 | transaction state: NONE=0, STAGING=1, SEALED=2, PROMOTE=3 |
| 5–7 | zero reserved |

Written and read by this HAL, by `ota-stage`/esuinit through
`esu_platform::stage` and by Surfacer, which mirrors the decoder. Letters are
never stored: they are derived from the booted slot and `Slot-<id>` byte 8, so a
staged set cannot disagree with the boot state about which slot it belongs to.
An absent variable and `NONE` are indistinguishable on purpose.

### Initial state

Host tools no longer write `bdsvars`. Surfacer creates `Slot-<id>` and
`MergeStatus-<id>` through SetVariable the first time its catalogue sees a ROM
without a record (gobbl `docs/boot/boot-control.md`). ROM 1 starts with the
current physical slot at priority 15, tries 7, successful, and the other slot
unbootable. ROM ≥2 starts with slot A selected, A at 15/7/successful and B at
14/7/successful. Both start with merge NONE. The HAL only consumes these records
and never initializes them.

## Method semantics

All numbers are Android slot indices 0/1, not partition numbers.

| Method | ROM 1: record-and-confirm | ROM ≥2: record-only |
| --- | --- | --- |
| getActiveBootSlot | pending target if present, otherwise confirmed selected slot; does not claim physical activation | selected HLOS slot |
| getCurrentSlot | immutable `ro.boot.slot_suffix` | same |
| getNumberSlots | 2 | 2 |
| getSuffix | `_a`/`_b`; empty string for invalid input, matching AIDL compatibility behavior | same |
| isSlotBootable | priority >0 and (successful or tries >0), from this ROM | same |
| isSlotMarkedSuccessful | this ROM's successful bit | same |
| markBootSuccessful | mark current successful, retain at least one try/nonzero priority; never confirm/clear a pending request | same, no other ROM changed |
| setActiveBootSlot | target priority15/tries7/unsuccessful; cap alternate priority14; persist pending target without changing confirmed slot; requesting confirmed slot cancels pending | same health reset; update selected slot immediately |
| setSlotAsUnbootable | zero target priority/tries/success; clear matching pending request; never silently activate alternate | same health clear; no implicit HLOS reselection |
| getSnapshotMergeStatus | read this ROM's merge variable; SNAPSHOTTED is reported NONE on its source slot (AOSP reversion rule), without changing raw stored value | same |
| setSnapshotMergeStatus | validate 0–4; persist this ROM status/current source, then physical VAB mirror | same |

getActiveBootSlot is answered by the shared service as the highest-priority slot
(ties resolved to the current slot); every record this HAL, Surfacer or the host
provisioners write gives the pending/selected target the strict maximum priority,
so it equals the record-derived answer. The record-derived value is
`Gbs1::booted_slot` and both are asserted in the host tests.

ROM 1 records a request even if another ROM is merging: the deliberate policy
choice is **record now, guard at Surfacer confirmation**, never bypass the
all-ROM merge guard. Repeated setActive is a retry and resets target health.
Getters do not mutate selection. Invalid slot-taking operations return stable
AIDL service-specific `INVALID_SLOT=-1` before any storage access, except
getSuffix's empty string.
Storage/validation failures return `COMMAND_FAILED=-2`; native parcel failures
retain Binder status. Unknown transaction codes return `STATUS_UNKNOWN_TRANSACTION`.

## Durability and evidence limits

The shared service's in-process Mutex serializes its read-modify-write operations;
the vendored transport serializes the single service instance.
Each read goes directly through shared efivarfs, without a private image cache;
each save performs one EFI set-variable operation (attributes 7 plus payload).
The backend owns synchronization and durable flushing; there is no block flock,
private append algorithm, whole-store rewrite or HAL reclaim.
Misc retains the same ordered write, sync and readback verification.
Host regular-file tests prove operation/error mapping and bytes, not EFI backend
power-cut durability, physical atomicity or inter-process read-modify-write safety.

Upstream source/evidence (not new port verification): lab record
`20261004T013922Z-phone-layout`, `analysis/layout-audit-boot.md` section 5 and
`analysis/ota-forensics.md` section 2. The upstream offline inspection reported
normal service
SHA-256 `59538b27b95916c7f620ad564e38dc38d38a103f9860c6a9e64b2547d1858177`
and library `b75da1baef43e26db61f9e731280871c539c9869fba5c3803a269dc799d11846`.
Library `get_current_slot` veneer 0x27a88 reaches 0xc8b0, which loads
`ro.boot.slot_suffix` (string 0x718c). Merge setter 0xe6e0 supplies current slot;
getter helper 0x1231c reads a 64-byte misc message at 0x8000 and reports NONE
when status2 and source match current (0x123c0–0x123d8). These establish stock
behavior parity at the interface boundary, not runtime execution of this HAL.

Upstream references:
[frozen V1 interface](https://raw.githubusercontent.com/LineageOS/android_hardware_interfaces/lineage-23.2/boot/aidl/aidl_api/android.hardware.boot/1/android/hardware/boot/IBootControl.aidl),
[hash](https://raw.githubusercontent.com/LineageOS/android_hardware_interfaces/lineage-23.2/boot/aidl/aidl_api/android.hardware.boot/1/.hash),
[AIDL error/suffix behavior](https://raw.githubusercontent.com/LineageOS/android_hardware_interfaces/lineage-23.2/boot/aidl/default/BootControl.cpp),
[AOSP misc merge helper](https://raw.githubusercontent.com/LineageOS/android_hardware_interfaces/lineage-23.2/boot/1.1/default/boot_control/libboot_control.cpp),
[VAB layout](https://raw.githubusercontent.com/LineageOS/android_bootable_recovery/lineage-23.2/bootloader_message/include/bootloader_message/bootloader_message.h).

## License and provenance

Apache-2.0. Ported from `gbl-bds-rs` worktree `pink-dormouse`, source revision:
`fb00461d451ec87ca21617e2da39072dbf64244d` plus its working-tree payload sources
as supplied on 2026-10-04: `payloads/boot-hal` and `crates/varstore`.
The policy source was `payloads/qshim/sepolicy/qshim.cil` (historical provenance
only, no compatibility property or path). `LICENSE` preserves Apache-2.0.
The private vendored parser/encoder was removed in the efivarfs cutover.

The AIDL transport, slot health policy and service dispatch now come from
[generic-bootctl](https://github.com/1vivy/generic-bootctl) `crates/core` and
`crates/aidl`, now the whole repository as the `../generic-bootctl` submodule;
upstream in turn adapted that transport from
`kernelesp/payloads/boot-hal@1d8413b8` (the earlier location of this payload). `src/service.rs`,
`src/storage.rs` and `src/android.rs` were deleted in this cutover; the GBS1/GBM1
layouts and frozen AIDL V1 transactions are unchanged.
