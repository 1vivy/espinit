# Cuttlefish integration lane (esu)

This directory is the **Cuttlefish-only** integration lane for esu: it
produces the payload the lab consumes through `--esu-payload`, plus the
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

The lane remains paused; packaging tests do not claim guest compatibility.
All inputs are explicit:

```sh
tools/cuttlefish/assemble.py \
  --stock-init-boot /build/stock/init_boot.img \
  --avbtool /tools/avbtool --avb-key /keys/cuttlefish.pem \
  --esuinit /build/esuinit --esud /build/esud \
  --busybox /build/busybox \
  --thin-activate /build/thin-activate --fw-views /build/fw-views \
  --core-module /build/kernelesp.ko --thin-module /build/thin.ko \
  --gpt-module /build/gpt.ko --efivarfs-module /build/efivarfs.ko \
  --kmi-out /build/kmi-out --metadata-filesystem ext4 \
  --rom-id rom1 --output-dir /build/cf-payload
```

`--boot-hal /build/gobbl-boot-hal` is the one optional input: supply the built
replacement binary to package the Boot HAL module exactly as before, or omit it
to assemble a payload without a Boot HAL directory, module-order entry, binary
or required-file check. Every other input is required, and an explicitly
supplied HAL must be a nonempty regular file like the rest.

The four modules require schema-2 `.ko.compat.json` receipts beside them.
The shared KMI verifier runs before image publication. It currently admits
AArch64 phone modules; that is not x86_64 Cuttlefish guest compatibility.
`--metadata-filesystem` validates the lane's requested ext4/f2fs selection;
it no longer creates a platform staging manifest.

Outputs are `init_boot.img`, `esp.img`, and `payload.json`. The stock image must
be kernel-free v3/v4 and the supplied key must authenticate it before re-signing.
The ramdisk retains stock archives and appends `/esuinit`, `/esu-build-id`, and
`/lib/{kernelesp,thin,gpt,efivarfs}.ko`. Compression and header fields are retained.
The daemon lives directly in ESP `esu/bin/esud`; no metadata installation occurs.

The ESP contains schema-1 manifest/ROM config with `modules_order =
["thin", "fw-views"]` — prepended with `"boot-hal"` only when `--boot-hal` is
supplied — no payload generation, no ROM number and no platform package table.
Every regular file a shipped module declares at its root is copied to
`esu/modules/<id>/`, except `pid1.sh`/`pid1-recovery.sh`, which the assembler
generates itself. Flag files therefore travel by presence: `thin` and `fw-views`
ship `critical`, which makes them critical, and `boot-hal` ships none, so it
stays optional; `disable`, `remove` and `skip_mount` would be carried the same
way.
With `--boot-hal`, the ordinary Boot HAL module has `module.prop`, `attrs`,
`sepolicy.rule`, and `vendor/bin/hw/android.hardware.boot-service.qti` copied
from the built HAL; without it none of those paths exist in the ESP.
Its fixed stock QTI target is not automatically compatible with a CF image.
Kernel modules never enter the ESP filesystem.

The managed ROM placeholder intentionally has an impossible backend; the lab
must replace it with real projection data. Runtime selection and number are
bdsvars BootedRom/Slot authority, not bootconfig or ROM TOML defaults.

`payload.json` records schema 1, `build_id`, `build_id_inputs`, and each image's
SHA256 and size. Build ID hashes the sorted input SHA256 values (duplicates
retained, each 64hex value followed by LF), taking the first 12 lowercase hex.
Both ESP `esu/build-id` and cpio `/esu-build-id` contain those 12 characters plus
LF. Input hashes include source files, generated configuration/scripts and the
copied module metadata. FAT timestamps need not be reproducible.

Host tools are invoked by argument vector, not a shell: `avbtool`,
`unpack_bootimg`, `mkbootimg`, `cpio`, gzip/lz4 and mtools. `--esp-size-mib`
overrides the default 64 MiB. `--overwrite` only replaces the three known
artifacts. Temporary files live beside the output and are removed on exit.

## `thin-activate`

Built with one bounded NDK invocation:

```sh
ANDROID_NDK_ROOT=/opt/android-ndk tools/cuttlefish/build-thin-activate.sh /tmp/thin-activate
# x86_64-linux-android26-clang -static -O2 -std=gnu11 -Wall -Wextra -Werror
```

The helper reads exactly one key, from `/proc/cmdline` or `/proc/bootconfig`
(disagreement is fatal):

```
androidboot.esu.thin=<PARTUUID>:<metadata sectors>:<data sectors>:<thin id>:<volume sectors>
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
`thin-pool` and `thin` must be registered with the kernel (ramdisk `thin.ko`
provides the last two) or the helper stops before creating anything.

Failures are one concrete stderr line and a non-zero exit: malformed or
overflowing tuple fields, a PARTUUID that matches no partition, several
matches, a whole-disk match, a geometry exceeding the partition, a missing
device-mapper target, an ioctl error, and a pre-existing `userdata_thin_meta`,
`userdata_thin_data`, `userdata_thin_pool` or `userdata_lp` whose type, length
or parameter string is not exactly this stack. Devices created by a failed run
are removed again; a pre-existing matching stack is reused, so a reboot of an
unchanged pool is idempotent. `/dev/mapper/userdata_lp` is published best
effort: esu resolves the backend name from
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
