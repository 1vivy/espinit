# Cuttlefish integration lane (espinit)

This directory is the **Cuttlefish-only** integration lane for espinit: it
produces the payload the lab consumes through `--espinit-payload`, plus the
`thin-activate` boot helper that builds the thin-provisioned `userdata_lp`
device the `gpt` backend projection expects.

Nothing here is part of the managed-ESP contract, the phone path, or a general
LVM abstraction. `thin-activate` is a deliberately narrow Cuttlefish helper and
the assembler is a packaging tool: neither one boots, verifies or validates a
Cuttlefish run. **No stage of this lane is a claim that the payload boots.**

## Boundary

| Owned here | Owned by the lab lane |
| --- | --- |
| `tools/cuttlefish/assemble.py`: build `init_boot.img`, `esp.img`, `payload.json` from explicit inputs | paused assembly, GPT retype of `cuttlefish_example_custom` to ESP, per-ROM config generation, boot/resume, apply/probe/cow-proof/reboot/merge/cancel/rollback evidence |
| `tools/cuttlefish/thin-activate.c` + `build-thin-activate.sh`: the x86_64 Android static helper the ESP runs | module builds, kernel selection, device/kernel-version acceptance, all Cuttlefish and Docker execution |
| Deterministic manifest/placeholder rendering and digest receipts | every gate: builds, tests, linters, formatters, Cuttlefish |

## Assemble the payload

Required inputs, all explicit paths (`--flag` above each file):

| Flag | Meaning |
| --- | --- |
| `--stock-init-boot` | stock AVB-signed Cuttlefish `init_boot.img`; its size is preserved |
| `--avbtool`, `--avb-key` | pinned AVB tool and RSA-4096 key that verify the stock image and sign its replacement |
| `--espinit` | static PID-1 binary, installed as `/espinit` in the initramfs |
| `--espinitd` | daemon binary, installed as `espinit/bin/espinitd` (install source, not the executed path) |
| `--boot-hal` | generation-noted boot HAL executable, packaged under `modules/boot-hal/` |
| `--tiny-espsu` | same-generation narrow bind/label helper, packaged under `modules/tiny-espsu/` |
| `--metadata-filesystem` | explicit `ext4` or `f2fs` for the projected metadata mount; no filesystem fallback |
| `--busybox` | static interpreter for ESP scripts, `espinit/bin/busybox` |
| `--thin-activate` | output of `build-thin-activate.sh`, `espinit/bin/thin-activate` |
| `--core-module`, `--thin-module`, `--gpt-module` | `espinit.ko`, `thin.ko`, `gpt.ko` built for the session kernel |
| `--kernel-src`, `--kernel-out`, `--kernel-config` | exact source/output and independent target config; shared LKM admission requires MODVERSIONS, matching imports/export CRCs, vermagic/BTF, and build receipts |
| `--generation` | one identifier, `[A-Za-z0-9._-]{1,63}`, written into every generation-bearing artifact |
| `--rom-id` | required catalogue ID matching `androidboot.espinit.rom`; generates `espinit/roms/<id>.toml` with matching `id`, not a global/default ROM config |
| `--output-dir` | target directory; must be empty (or hold only previous artifacts with `--overwrite`) |
| `--esp-size-mib` | optional ESP image size in MiB (default 64). The lab lane copies this exact file over the pinned disposable `cuttlefish_example_custom.img` and regenerates that GPT entry from the file size, so any size the payload needs is acceptable |
| `--overwrite` | replace `init_boot.img`, `esp.img`, `payload.json` in an output directory that holds them |

```sh
tools/cuttlefish/assemble.py \
    --stock-init-boot  <pinned stock init_boot.img> \
    --avbtool          <pinned avbtool> \
    --avb-key          <matching Cuttlefish AVB key> \
    --espinit          <espinit PID-1> \
    --espinitd         <espinitd> \
    --boot-hal         <gblbds-boot-hal> \
    --tiny-espsu       <tiny-espsu> \
    --metadata-filesystem ext4 \
    --busybox          <static busybox> \
    --thin-activate    <thin-activate> \
    --core-module      <espinit.ko> \
    --thin-module      <thin.ko> \
    --gpt-module       <gpt.ko> \
    --kernel-src       <exact kernel source> \
    --kernel-out       <complete exact kernel output> \
    --kernel-config    <independent target config> \
    --generation       <generation> \
    --rom-id           <catalogue ROM ID> \
    --esp-size-mib     <pinned custom partition size> \
    --output-dir       <empty directory>
```

The assembler runs the repository's shared `scripts/phone_modules.py verify`
before creating or replacing any payload image. Each module needs its matching
`<name>.ko.compat.json` beside it. The same gate applies to phone and lab
payloads: unversioned modules, stale receipts and config mismatches are not
accepted as a Cuttlefish exception. See [build notes](../../README.md#build-notes).

Host tools used, each through a checked subprocess argument list (never a
shell): the supplied `avbtool`, plus `unpack_bootimg`, `mkbootimg`, `cpio`,
`gzip`/`lz4`, `mformat`, `mmd`, and `mcopy`. Temporary files are created beside
the output directory for same-filesystem publication and removed on exit; a
failed run leaves no partial artifact because the three files are moved into
place only after every step succeeded.

### Output contract

`init_boot.img` - the stock image with the static PID-1 binary installed as
root member `/espinit` (mode 0755). The ramdisk keeps its original compression
(legacy LZ4, gzip or uncompressed are preserved; a frame-format LZ4 ramdisk is
refused, because the lane kernel's `lib/decompress_unlz4.c` accepts only the
legacy magic) and every stock archive byte-for-byte, followed by the `/espinit`
archive. `kernel_size`, `header_version`, `header_size`, `cmdline`,
`os_version`/`os_patch_level` and the page layout are unchanged. The supplied
AVB key must verify the stock image; the replacement receives a
`SHA256_RSA4096` `init_boot` hash footer, is verified with the same key, and is
padded by `avbtool` to the exact stock partition size.

`esp.img` - a FAT image (VFAT long names, volume label `ESPINIT`) containing
exactly:

```
/espinit/manifest.toml              schema_version = 1, the supplied generation,
                                    rom = "roms", modules espinit, thin, gpt
/espinit/roms/<id>.toml             selected managed placeholder, replaced by the lab
/espinit/bin/busybox                static interpreter
/espinit/bin/thin-activate          x86_64 Android static helper
/espinit/bin/espinitd               daemon install source
/espinit/modules/espinit.ko
/espinit/modules/thin.ko
/espinit/modules/gpt.ko
/espinit/modules/thin/early.sh      #!/bin/sh, set -eu, exec thin-activate
/espinit/modules/boot-hal/module.toml
/espinit/modules/boot-hal/android.hardware.boot-service.gblbds
/espinit/modules/boot-hal/boot-gblbds.rc
/espinit/modules/tiny-espsu/module.toml
/espinit/modules/tiny-espsu/tiny-espsu
/espinit/modules/tiny-espsu/install.sh
/espinit/modules/tiny-espsu/policy.cil
/espinit/receipts/                  directory; a missing receipt store is a
                                    hard managed-boot failure
```

The placeholder `roms/<id>.toml` is structurally valid (`schema_version = 1`, the
supplied generation and matching `id`, `managed = true`, one projection) but its backend is
deliberately impossible, so an un-replaced payload fails closed instead of
booting with a guessed partition view. The lab writes the real file with the
same ID and generation before paused assembly, and supplies
`androidboot.espinit.rom=<id>` when booting. Missing, duplicate or conflicting
boot selections fail; no global-file alias is accepted.

The assembler stamps the checked-in package manifests with the supplied
generation and validates the ELF generation notes of espinitd, boot HAL and
tiny-espsu without executing them. PID1 repeats that validation and installs
files with the exact modes in `module.toml`, independent of FAT modes. A real
normal ROM config must project writable metadata, bdsvars and misc. Recovery
uses a separate explicitly selected package list and excludes the normal HAL.
The checked-in helper/RC target the source device's exact QTI executable and
`vendor.boot-qti` service; a Cuttlefish build without that layout is not a
runtime-compatible HAL target merely because the assembler accepts its ELF.
Do not guess another bind destination. See the root README's platform contract.

Executable intent: the initramfs copy of the PID-1 binary is the only member
that carries a POSIX mode (root `/espinit`, 0755, written by `cpio` with
`--owner=0:0`). A FAT image stores no POSIX modes at all, and it does not need
them: PID 1 runs each ESP script explicitly through the ESP busybox
(`busybox sh <payload>/modules/<name>/early.sh`), so `bin/` and
`modules/thin/early.sh` are ordinary FAT members. `mmd` creates
`/espinit/receipts` as a real directory, because espinit treats a missing
receipt store as a hard managed-boot failure.

`payload.json` - `{"schema_version": 1, "generation": "<generation>",
"images": {"init_boot.img": {"sha256", "size"}, "esp.img": {"sha256",
"size"}}}`.

The manifest and the placeholder are deterministic for a given generation; the
ESP image is not required to be reproducible (FAT timestamps).

## `thin-activate`

Built with one bounded NDK invocation:

```sh
ANDROID_NDK_ROOT=/opt/android-ndk tools/cuttlefish/build-thin-activate.sh /tmp/thin-activate
# x86_64-linux-android26-clang -static -O2 -std=gnu11 -Wall -Wextra -Werror
```

The helper reads exactly one key, from `/proc/cmdline` or `/proc/bootconfig`
(disagreement is fatal):

```
androidboot.espinit.thin=<PARTUUID>:<metadata sectors>:<data sectors>:<thin id>:<volume sectors>
```

with the pinned lab tuple `...:131072:16646144:1:16777216`, meaning:

| Field | Value | Use |
| --- | --- | --- |
| PARTUUID | userdata GPT partition UUID | resolved by exact PARTUUID from `/sys/class/block` |
| metadata sectors | 131072 | `linear <part> 0` -> `userdata_thin_meta` |
| data sectors | 16646144 | `linear <part> 131072` -> `userdata_thin_data` |
| thin id | 1 | `thin <pool> <id>`; an existing id is reopened by dm-thin |
| volume sectors | 16777216 | length of `userdata_lp` |

It then builds `thin-pool` over the two linear devices with `128`-sector data
blocks, a `128`-block low water mark and `1 skip_block_zeroing` (mandatory in
this fork: a pool that would zero newly provisioned blocks is rejected), and
finally `userdata_lp` as a thin volume of the tuple's length. `linear`,
`thin-pool` and `thin` must be registered with the kernel (the ESP `thin.ko`
provides the last two) or the helper stops before creating anything.

Failures are one concrete stderr line and a non-zero exit: malformed or
overflowing tuple fields, a PARTUUID that matches no partition, several
matches, a whole-disk match, a geometry exceeding the partition, a missing
device-mapper target, an ioctl error, and a pre-existing `userdata_thin_meta`,
`userdata_thin_data`, `userdata_thin_pool` or `userdata_lp` whose type, length
or parameter string is not exactly this stack. Devices created by a failed run
are removed again; a pre-existing matching stack is reused, so a reboot of an
unchanged pool is idempotent. `/dev/mapper/userdata_lp` is published best
effort: espinit resolves the backend name from
`/sys/class/block/dm-*/dm/name`, and Android's ueventd creates the node after
handoff.

## Verification boundary

- Building and inspecting the helper and payload proves only their host-side
  shape; the lab lane must boot the payload and prove the guest-visible result.
- The default `--esp-size-mib=64` is only a default: the lab lane regenerates the
  `cuttlefish_example_custom` GPT entry from the emitted file size, so pass the
  size the payload needs.
- The stock image must be an AVB-signed, kernel-free `init_boot` with header v3
  or v4; the supplied RSA-4096 key must verify it.
