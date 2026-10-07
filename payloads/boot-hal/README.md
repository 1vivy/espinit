# Managed boot-control HAL

**Status (2026-10-06)** - AIDL V1 service uses shared `esu-platform::efivars`.
Registration is never gated on efivarfs, BootedRom, managed Slot state or misc.
Host verification does not claim phone, Binder guest, OTA or recovery execution.

## Build and Binder choice

From the product worktree:

```sh
cargo +nightly-2026-08-08 test --manifest-path payloads/boot-hal/Cargo.toml --locked --offline --jobs 3
ESU_NDK=/path/to/android-ndk-r29 bash payloads/boot-hal/build-android.sh
```

The build uses the explicitly supplied NDK, API 35 and the installed
`aarch64-linux-android` Rust target. Output:
`payloads/boot-hal/target/aarch64-linux-android/release/gblbds-boot-hal`.
When supplied, the packager installs it as an optional read-only ESP module at
`/esu/modules/boot-hal/vendor/bin/hw/android.hardware.boot-service.qti`.
There is no ELF generation note; `esu/build-id` and cpio `/esu-build-id`
identify the complete payload, and a mismatch is logged rather than pinned.

Rust, the state machine and shared `esu-platform` are **statically linked**. Binder
uses the platform `libbinder_ndk.so` C ABI; no AOSP build tree, generated AIDL
shared library, vendor QTI library or C++ runtime is needed.
`android.rs` implements the frozen V1 transaction order, status headers,
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

Tests cover record/confirm versus record-only selection, cancellation/retry/success,
malformed state, source-slot VAB reversion, lazy identity recovery, AIDL storage
failures, byte-exact efivarfs writes and preservation of other-ROM variables,
BCB/bootloader-control bytes and valid V2 VAB reserved bytes.

## Installation and identity

The ESP `boot-hal/module.prop` declares this ordinary KernelSU module.
It has no `critical` marker. Its policy or overlay failure is reported without
making the generic module loader reboot Android; omitting or disabling it
leaves the stock target executable in place.
`esu/modules/boot-hal/attrs` supplies mode 0755, root:shell ownership and
the stock target's `hal_bootctl_default_exec` label. During `esud early`,
the file is copied to `/dev/esu/boot-hal/vendor/bin/hw/` on tmpfs and a
read-only overlay is mounted over `/vendor` (module lowerdir first, stock
`/vendor` last). Stock `vendor.boot-qti` init rc and VINTF manifest remain
unchanged, so the stock entrypoint and `hal_bootctl_default` domain transition
apply; there is no override rc, bind-mounted metadata executable or
`tiny-espsu`. The module's `sepolicy.rule` grants that existing domain
read/write access to `esu_file` efivarfs variables, not the raw bdsvars
block device.

- Init service: stock **`vendor.boot-qti`**, `class early_hal`, root/root.
- Binder identity: **`android.hardware.boot.IBootControl/default`**.
- Version: **1**; hash: **`2400346954240a5de495a1debc81429dd012d7b7`**.
- Keep the existing VINTF manifest unchanged; no second service is registered.
- The HAL opens only project efivarfs variables and `/dev/block/by-name/misc`;
  never a whole LU, GPT, UFS sysfs node, boot partition or firmware partition.

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

Recovery retains its projection and `pid1-recovery.sh` scripts for modules
marked `recovery-ok`. The core `on init` path now runs there too, so the Boot
HAL overlay is applied in recovery whenever its `vendor` target is already a
mount point; a target recovery mounts later is left untouched.
No native writer is invoked as a fallback. The source capture also contained
HIDL 1.0-1.2 implementations: recovery AIDL/HIDL parity and device-specific
suppression of stock activation routes remain unproved integration prerequisites.
This port does not intercept OTA payload writes; esu's partition projection
is mandatory before any managed OTA.

Integration must permit project efivarfs reads/writes and existing misc access.
The HAL uses only an in-process Mutex, not block-device `flock`. Its executable
on the tmpfs lowerdir must retain the stock `hal_bootctl_default_exec` label;
device SELinux/overlay proof remains open.

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
stock `bootloader_control` bytes are touched. Startup attempts to mirror the
booted ROM's raw status/source; failure is retried before a state-dependent
transaction. Setters persist that ROM's variable before flushing/readback of
the physical mirror. Mirror failure leaves authority committed for reconciliation.
No other ROM's variable is inferred from or overwritten by misc.

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

The HAL Mutex serializes its read-modify-write operations within this process.
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

Ported from `gbl-bds-rs` worktree `pink-dormouse`, source revision
`fb00461d451ec87ca21617e2da39072dbf64244d` plus its working-tree payload sources
as supplied on 2026-10-04: `payloads/boot-hal` and `crates/varstore`.
The policy source was `payloads/qshim/sepolicy/qshim.cil` (historical provenance
only, no compatibility property or path). `LICENSE` preserves Apache-2.0.
The private vendored parser/encoder was removed in the efivarfs cutover.
State/merge payload layouts and frozen AIDL V1 transactions remain unchanged.
