# `ota`: switch devices and device names for the ROM OTA transaction

**Status (2026-10-08)** — The `ota` module is the PID-1 half of the ROM OTA
staging design. It ships a `critical` marker: a managed boot refuses the Android
handoff when `ota-stage` fails, because the `gpt` projection that follows names
the switch devices this module creates. It also ships `recovery-ok`, so recovery
boots publish the same devices (with no transaction in flight they are all error
targets). `modules_order` lists it after `thin` and before `gpt`.

## What it does

`pid1.sh`/`pid1-recovery.sh` run the static `ota-stage` helper. For every base
image the selected ROM declares through a `rom-image:<base>` partition role it
creates one device-mapper device:

```
rom<N>-ota-<base>   0 <image sectors> error
rom<N>-ota-<base>   0 <image sectors> linear <staging LV maj:min> 0   (read-only iff the staged letter is the booted one)
```

The length is `ota_core::copy::exact_sectors` of
`/debug_ramdisk/esp/rom/<id>/<base>.img`, so an idle switch has exactly the image
geometry the ROM config's generated `SOURCE_COPY` checks compare against, and a
read of an unstaged letter fails with an I/O error instead of serving stale
bytes. While a staged letter runs the switch is linear over that base's active
staging LV, `rom-rom<N>--stage--<base>`, which the earlier `thin` entry
activated. An identical existing table is accepted, so the helper is idempotent.

A ROM 1 does nothing and exits 0: its kernel images are physical partitions the
Surfacer-confirmed slot switch already routes. Every other failure exits 1 with a
one-line reason on stderr; PID 1 treats a critical module's failure as fatal.

## `early.sh`

Runs at esud's `early` stage, before any `early_hal` service, and does two
things:

1. Creates the runtime LVM command names next to the static `lvm` helper in the
   executable tmpfs `/debug_ramdisk/esu/bin` (which esud puts on `PATH`):
   `lvcreate`, `lvremove`, `lvchange`, `lvs`, `vgs`, `pvs` are symlinks to
   `lvm`. The boot HAL runs those names when it prepares and tears down a
   staging set.
2. `exec esud esd refresh`, which publishes `/dev/block/esd/` — the physical
   userdata PV (`pv/a`), every projected-away original partition
   (`by-name/<PARTNAME>`), every active `rom-` device mapper node (`lv/<lv>`)
   and device-mapper's control node (`mapper/control`). The HAL addresses
   storage by those names and runs the same refresh after it creates or removes
   a staging LV.

## `sepolicy.rule`

Declares the `esu_blk_device` type every `/dev/block/esd` node is labelled with
(`u:object_r:esu_blk_device:s0`) and allows the `esu` domain — esud's runtime
domain, which owns that tree — to create, label, read and remove those nodes.
The type itself is declared here, not in the boot HAL's rules: the two modules'
rules are concatenated, and a `type` declared twice fails the whole policy
update.

## `boot-completed.sh`

When the boot HAL denies a transaction it writes
`/debug_ramdisk/esp/esu/receipts/ota-denied.txt` and tries
`cmd notification post -S bigtext -t "esu OTA" esu.ota` itself. Under enforcing
that exec can be refused, so this script posts the receipt's text from the
`boot-completed` stage and deletes the receipt when the post succeeds. A missing
receipt, a refused `cmd` or a read-only ESP only logs a warning; the stage never
fails.

## Build

`ota-stage` must be static, like the other PID-1 helpers. Build it with a clean
NDK whose `libc.a` bundles no Rust std members (r29; a contaminated or r30 NDK
fails the static link with a duplicate `rust_eh_personality`):

```sh
NDK=/path/to/clean/android-ndk-r29
RUSTFLAGS="-C target-feature=+crt-static" \
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android35-clang" \
  cargo +nightly-2026-08-08 build --locked --release \
  --target aarch64-linux-android -p ota-stage --bin ota-stage
```

The CI payload lane builds it with the same `-C target-feature=+crt-static`, the
`libclang_rt.builtins-aarch64-android.a` resource object and API level 26. Copy
the static binary to `bin/ota-stage` on the ESP and ship this directory's
`pid1.sh`, `pid1-recovery.sh`, `early.sh`, `boot-completed.sh`, `sepolicy.rule`,
`critical` and `recovery-ok`. `file` must report the binary as statically
linked.

Host checks:

```sh
cargo test -p ota-stage
cargo clippy -p ota-stage --all-targets -- -D warnings
ANDROID_NDK_HOME=/path/to/android-ndk-r29 cargo ndk -t arm64-v8a clippy -p ota-stage -- -D warnings
```
