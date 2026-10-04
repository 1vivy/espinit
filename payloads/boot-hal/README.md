# Managed boot-control HAL

**Status (2026-10-04)** - Ported AIDL V1 service and per-ROM state/storage adapter.
The original state/storage tests are retained. Verification of this espinit port
is a separate gate; no phone, Binder guest, OTA or recovery execution is claimed.

## Build and Binder choice

From the product worktree:

```sh
cargo +nightly-2026-08-08 test --manifest-path payloads/boot-hal/Cargo.toml --locked --offline --jobs 3
ESPINIT_NDK=/path/to/android-ndk-r29 ESPINIT_GENERATION=release-1 \
  bash payloads/boot-hal/build-android.sh
```

The build uses the explicitly supplied NDK, API 35 and the installed
`aarch64-linux-android` Rust target. Output:
`payloads/boot-hal/target/aarch64-linux-android/release/gblbds-boot-hal`.
The ESP package installs it as
`/metadata/espinit/modules/boot-hal/android.hardware.boot-service.gblbds`.
It carries the same retained `.note.espinit` generation as PID1/espinitd/tiny-espsu.

Rust, the state machine and the vendored `varstore` are **statically linked**. Binder
uses the platform `libbinder_ndk.so` C ABI; no AOSP build tree, generated AIDL
shared library, vendor QTI library, C++ runtime or downloaded Rust crate is
needed. `android.rs` implements the frozen V1 transaction order, status headers,
Boolean/string/int replies and the two stable-interface metadata transactions.
NDK headers supply the public ABI; four platform-only symbols are resolved with
checked `dlsym`, since app-NDK stubs omit service registration/thread-pool/VINTF
entrypoints. The service retains its binder object/class for process lifetime.
The main thread is the only Binder worker.

**This is not a fully static ELF.** The explicitly allowed dynamic Binder choice
requires Android's dynamic linker/bionic. The upstream artifact was observed as
AArch64 ELF64 PIE, interpreter `/system/bin/linker64`, with direct dependencies
`libbinder_ndk.so`, `libc.so`, `libdl.so`. Recheck the port's produced ELF. A literal no-PT_INTERP/static-bionic
requirement is incompatible with this approach: static bionic does not support
loading the platform Binder library. Do not advertise this artifact as a
fully static executable or deploy it before the system linker is available.
The HAL runs as `early_hal`, after system/vendor mounts, not as ramdisk PID 1.

Retained tests cover record/confirm versus record-only selection,
cancellation/retry/success, malformed slots/state, source-slot VAB reversion,
and durable append/readback preserving other-ROM, BCB and bootloader-control
contents. These host tests do not prove Binder or power-cut behavior.

## Installation and identity

The ESP `boot-hal/module.toml` declares the executable and `boot-gblbds.rc`.
PID1 installs the exact-generation package atomically after GPT projection and
generates `/metadata/espinit/initrc/modules.rc`. The core appends this override
while Android parses init.rc. Its mandatory synchronous on-init service runs
`espinitd early` after ueventd coldboot and before `class early_hal` can start.
tiny-espsu labels the actual source inode and binds it over
`/vendor/bin/hw/android.hardware.boot-service.qti`; it does not modify vendor
storage or use a shell, arbitrary command, app-root API or generic policy loader.

- Init name: **`vendor.boot-qti`**, `override`, `class early_hal`, root/root.
- Binder identity: **`android.hardware.boot.IBootControl/default`**.
- Version: **1**; hash: **`2400346954240a5de495a1debc81429dd012d7b7`**.
- Keep the existing VINTF manifest unchanged; no second service is registered.
- Label the bound source inode `gblbds_hal_exec`; policy transitions init into
  existing `hal_bootctl_default`. Do not use an explicit rc `seclabel` to bypass
  the entrypoint contract. Label the actual bdsvars block inode
  `gblbds_bdsvars_block_device`, not just its symlink; allow read/write, getattr,
  open and cooperative file locking for this HAL. Existing misc access remains.
- espinit must project validated `/dev/block/by-name/bdsvars` and
  `/dev/block/by-name/misc` onto their intended backends. The HAL never opens a
  whole LU, GPT, UFS sysfs node, boot partition or other firmware partition.

The immutable `ro.boot.espinit.rom` property (from `androidboot.espinit.rom`)
selects the catalogue id (1-59 ASCII letters/digits/`-_.`; the filename adds
`.toml` within a 64-byte path-component limit). `ro.boot.slot_suffix` must be
exactly `_a` or `_b`; it is
**the only current-slot authority**, never the pending/selected record. ROM
number/class comes from the provisioned record, not a guess from the id string.
Missing properties, missing/malformed variables or initial misc failure abort
startup before registration; there is no guessed default, automatic format or
fallback to the stock physical writer.

Recovery retains its projection and recovery scripts but stages only explicitly
listed `platform.recovery_packages`; the normal HAL and tiny-espsu are excluded.
No native writer is invoked as a fallback. The source capture also contained
HIDL 1.0-1.2 implementations: recovery AIDL/HIDL parity and device-specific
suppression of stock activation routes are unproven integration prerequisites.
This port does not intercept OTA payload writes; espinit's partition projection
is mandatory before any managed OTA.

The built-in policy deliberately gives neither new object type `file_type` nor
`dev_type` attributes. In addition to the source CIL, integration requires:
`blk_file lock` for the unchanged `flock(LOCK_EX)` transaction adapter;
`filesystem associate` from the HAL type to labeledfs and the bdsvars type to
tmpfs; and `init -> hal_bootctl_default:process2 nosuid_transition` because the
bound HAL source lives on nosuid metadata. The packaged CIL mirrors built-in
second-stage policy, rather than serving as a late installation path.

## Variable namespace and wire layout

Vendor namespace GUID: **`7a5e4b1c-0d3f-4e62-9b8a-1c2d3e4f5a6b`**.
`VENDOR_GUID` uses EFI mixed-endian bytes:
`1c 4b 5e 7a 3f 0d 62 4e 9b 8a 1c 2d 3e 4f 5a 6b`.
This is the **variable namespace**, not the GPT bdsvars partition type GUID.
Names use edk2 UTF-16LE plus its terminating NUL, handled by `crates/varstore`.
Attributes are exactly **7 (NV | BS | RT)**. The 1 MiB partition is an edk2 FV;
provisioning uses `Layout::Authenticated` for `virt-fw-vars` interoperability.
These particular variables are ordinary unauthenticated NV variables inside
that store. The adapter accepts either layout validated by `crates/varstore`.

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
guard belong to Surfacer, not this userspace HAL.

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
stock `bootloader_control` bytes are touched. Startup mirrors the selected
booted ROM's stored raw status/source; setters persist that ROM's variable
before flushing/readback of the physical mirror. A mirror failure returns an
error but leaves the authoritative variable committed; retry/startup reconciles
it. No other ROM's variable is inferred from or overwritten by misc.

### Provisioning seed

Generate the generic JSON consumed by `provision bdsvars --seed <file>`:

```sh
cargo +nightly-2026-08-08 run --manifest-path payloads/boot-hal/Cargo.toml \
  --locked --offline --jobs 3 --example seed -- rom1 1 _b > boot-hal-seed.json
```

`_b` is only an example (the cited capture booted `_b`); supply the current slot
from the installation receipt. The generator reads no device. It emits both
variables, using `State::initial` and `Merge::encode`, with the current slot
priority 15/tries 7/successful, the other slot unbootable, no pending switch and
merge NONE. An inactive slot is not presumed usable merely because it exists.
For ROM ≥2 supply its own number, catalogue id and selected seeded image slot.
Never seed merge NONE over an existing OTA transaction; this is fresh-install
state, not a repair or runtime migration command.

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

ROM 1 records a request even if another ROM is merging: the deliberate policy
choice is **record now, guard at Surfacer confirmation**, never bypass the
all-ROM merge guard. Repeated setActive is a retry and resets target health.
Getters do not mutate selection. Invalid slot-taking operations return stable
AIDL service-specific `INVALID_SLOT=-1`, except getSuffix's empty string.
Storage/validation failures return `COMMAND_FAILED=-2`; native parcel failures
retain Binder status. Unknown transaction codes return `STATUS_UNKNOWN_TRANSACTION`.

## Durability and evidence limits

`Storage` takes an exclusive `flock` on bdsvars for each transaction and reloads
its bytes, so cooperative Linux writers preserve each other's variables. All
other Linux bdsvars writers must honor that lock. Two allocated image buffers
are reused; there is no new 1 MiB allocation per method. The shared varstore
crate exclusively owns parsing/encoding. The persistence adapter writes old
record TRANSITION, new header, HEADER_VALID, payload, ADDED, then old DELETED,
with `fsync` between phases and full readback comparison. It never writes a
final image wholesale. Unchanged updates avoid another record. Full stores
return an error; reclaim belongs to Surfacer/provisioning, never implicit Android
erase-and-rewrite. Torn/malformed media fails closed; no power-cut durability or
physical sector atomicity claim follows from the regular-file test.

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

Ported from `gbl-bds-rs` worktree `pink-dormouse`, source revision
`fb00461d451ec87ca21617e2da39072dbf64244d` plus its working-tree payload sources
as supplied on 2026-10-04: `payloads/boot-hal` and `crates/varstore`.
The policy source was `payloads/qshim/sepolicy/qshim.cil` (historical provenance
only, no compatibility property or path). `LICENSE` preserves Apache-2.0.
The vendored parser/encoder and storage/state wire formats are unchanged;
the port changes packaging, generation embedding and the ROM property spelling.
