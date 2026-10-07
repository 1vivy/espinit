# `fw-views`: per-ROM firmware views on the shared thin pool

`fw-views` is an ESP KernelSU module, not a kernel module. `modules_order`
lists it after `thin`; PID1 runs `modules/fw-views/pid1.sh` before GPT APPLY.
Recovery requires `recovery-ok` and runs `pid1-recovery.sh`. The helper reads
`ESU_ROM` and `ESU_ROM_NUMBER` exported by PID1 from efivarfs, validates the
selected ESP ROM config, and returns an error on failure. This module ships a
`critical` marker: an admitted PID1 failure blocks the managed handoff in Android
and recovery. Removing the marker makes helper failure best effort, but the
final GPT/backend checks still refuse missing required firmware views.

## What it does

For every `firmware_views` entry of the selected ROM it creates one thin device
of the pool `thin-activate` already published:

```
rom<N>-fw-<name>   0 <partition sectors> thin <rom-pool-tpool maj:min> <thin_id> <origin maj:min>
```

The origin is the physical partition resolved by its sysfs `PARTNAME` before any
projection runs, and the kernel opens it read-only. The result is the mechanism
the ROM sees as firmware:

* **unwritten blocks** read the physical firmware partition's exact bytes, so a
  ROM that never touches a partition boots against the real firmware;
* **this ROM's writes** provision private blocks in the shared pool, so an OTA
  that "flashes" `xbl`, `modem`, `tz`, … only changes this ROM's view;
* **the pool is the scratch space**, and deleting the view's thin id returns the
  partition's physical bytes (drop).

Thin ids are reserved as `(rom_number << 16) | index`, `index` being the 1-based
position in `firmware_views`, so an id can never collide with an LVM2-owned id
(below `0x10000`, enforced by `lvm2-meta`) and never with another ROM. A view id
created by an earlier boot is reported by the pool as "already exists" and the
boot continues: the device, not the message, is the state the ROM owns.

## ROM config contract

Every view must appear in the same ROM file as one of its projections, with the
exact backend and access the helper publishes. This example assumes the
authoritative Slot record supplies `ESU_ROM_NUMBER=2`; no `rom_number` field
is permitted in the ROM TOML:

```toml
[[firmware_views]]
name = "xbl_a"       # physical sysfs PARTNAME, `<base>_a` or `<base>_b`
thin_id = 131073     # (2 << 16) | 1

[[partitions]]
name = "xbl_a"
backend = "/dev/mapper/rom2-fw-xbl_a"
read_only = false
```

`validate_rom` rejects a view on ROM 1 or on an unmanaged ROM, a name that is not
a `<base>_a`/`<base>_b` PARTNAME, a base from the seven the kernel itself selects
from the current slot (`boot`, `init_boot`, `vendor_boot`, `dtbo`, `vbmeta`,
`vbmeta_system`, `vbmeta_vendor`), a duplicated name, anything other than the
reserved id of that list position, and a missing or read-only projection.

## Authoring rule for a ROM >= 2

One view per physical `<base>_a`/`<base>_b` pair on LU0–LU5, except the seven
kernel-set bases and `super`; a view that has no provisioned block yet simply
serves the physical partition. On this phone that is about 41 bases × 2 = 82
views, plus the 14 kernel-set `esp-file:` projections, plus
`super`→`super_N`, `metadata`, `userdata`, `metadata_shared`, `bdsvars` and
`misc`: about 103 projections, within the 128-projection gpt ABI.

The 14 kernel-set bases stay physical but writable through their preallocated
images on the ESP: a managed ROM `>= 2` may project `esp-file:` backends with
`read_only = false`, and the loader then holds the ESP read-write for that boot
so the loops can rewrite the images. ROM 1 and every other payload keep the ESP
read-only and `esp-file:` read-only.

## Seal interplay

The `gpt` APPLY that follows seals physical storage this view does not project
(`GPT_APPLY_FLAG_SEAL`, managed ROM `>= 2` only). A view survives the seal
because it is created *before* APPLY: the kernel already holds the origin
partition through the thin table, and the ESP superblock is already open for the
writable `esp-file` loops. The ESP partition itself stays sealed — no new raw
open is taken for it — so expect one `bio_check_ro` warning per device when a
loop writes through an already-open superblock; that warning is the documented
limit of a partition-level seal, not an error.

## Drop

Deleting a view is an explicit operation outside the boot path, because it is the
ROM's storage that disappears:

```sh
dmsetup remove rom2-fw-xbl_a
dmsetup message rom-pool-tpool 0 "delete 131073"
```

The next boot recreates the id in `create_thin`, and the view reads the physical
partition again.

## Build

There is no compiled payload-generation note or runtime generation gate.
Build it with an NDK whose
`libc.a` bundles no Rust std members (r29; a contaminated or r30 NDK fails the
static link with a duplicate `rust_eh_personality`), for example by pointing the
target linker at one:

```sh
NDK=/path/to/clean/android-ndk-r29
RUSTFLAGS="-C target-feature=+crt-static" \
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android35-clang" \
  cargo +nightly-2026-08-08 build --locked --release \
  --target aarch64-linux-android -p fw-views --bin fw-views
```

Copy the static binary to `bin/fw-views` on the ESP and ship this directory's
`pid1.sh`, `pid1-recovery.sh` and `recovery-ok`. Do not run the PV's LVM tooling (`lvconvert --repair`, `thin_restore`)
without re-running this module afterwards: those tools drop devices whose ids are
not in the metadata.
