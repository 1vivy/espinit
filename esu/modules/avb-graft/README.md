# `avb-graft`: generic AVB metadata grafts

**Status (2026-10-07)** — Optional module by default; host/build verification and device qualification are separate gates. This module does not authorize physical writes or manufacture a valid AVB signature.

Install the static `avb-graft` executable at `esu/bin/avb-graft` and this module at `esu/modules/avb-graft`. Add `avb-graft` after `fw-views` in `modules_order`. All PID1 scripts finish before GPT projection; normal and recovery scripts run the same helper, and `recovery-ok` opts into recovery/fastbootd. The helper returns errors without bypassing AVB checks. By default esu reports a failed graft and continues; Android may still reject the resulting view. Add a regular `critical` marker to this module to make an admitted PID1 failure stop the managed handoff.

## Generic host operations

```sh
avb-graft extract IMAGE NEW_METADATA
avb-graft apply IMAGE METADATA NEW_OUTPUT
```

`extract` reads the fixed-position AVB footer and its validated AVB0 extent; it never scans payload bytes for markers. `apply` validates the standalone metadata and layout, copies a regular source image to a new output, and writes only differing metadata/footer bytes. Destinations must not already exist, so the source cannot be accidentally overwritten. Partition size and payload remain unchanged. Invalid footer geometry or metadata that does not fit fails with a concrete error. A geometrically valid footer may point to an empty VB region for initialization. Images without a valid footer are refused.

The canonical allocation-free parser/layout is `userspace/avb-graft`; this executable owns only host/Linux I/O. Metadata input is bounded to 16 MiB. Grafting is not payload hashing or signature verification: use avbtool or the actual AVB admission path for those checks.

### Real avbtool smoke recipe (host only)

Run in a fresh temporary directory with the built CLI and AOSP `avbtool` on PATH:

```sh
mkdir smoke && cd smoke
truncate -s 4096 payload.img
cp payload.img original.img
avbtool add_hash_footer --image original.img --partition_name recovery \
  --partition_size 1048576 --algorithm NONE --salt 00
avb-graft extract original.img original.vbmd
cp payload.img replacement.img
avbtool add_hash_footer --image replacement.img --partition_name recovery \
  --partition_size 1048576 --algorithm NONE --salt 01
avb-graft extract replacement.img replacement.vbmd
avb-graft apply original.img replacement.vbmd grafted.img
avbtool info_image --image grafted.img
avbtool verify_image --image grafted.img
avb-graft extract grafted.img grafted.vbmd
cmp replacement.vbmd grafted.vbmd
cmp -n 4096 original.img grafted.img
test "$(stat -c %s original.img)" = "$(stat -c %s grafted.img)"
avb-graft apply grafted.img replacement.vbmd unchanged.img
cmp grafted.img unchanged.img
# Both must fail, without overwriting the input or existing output:
! avb-graft apply original.img replacement.vbmd grafted.img
truncate -s 4096 footerless.img
! avb-graft apply footerless.img replacement.vbmd refused.img
```

The `NONE` algorithm here is a structural smoke, not signed-image admission evidence. Changing the salt changes real avbtool-generated descriptors while keeping the same payload, allowing `verify_image` to verify the resulting hash descriptor. For signed qualification repeat with an owner-selected key and supported algorithm.

## Installed ROM configuration

`avb-graft module` accepts no path overrides. It reads the mounted ESP's current `esu/manifest.toml`, resolves the selected ROM through `ESU_ROM` and `ESU_ROM_NUMBER` exported by PID1, and validates the shared schema. Each configured `[[partitions]].metadata` is an ESP-root-relative `.vbmd` path:

```toml
[[firmware_views]]
name = "recovery_a"
thin_id = 131073 # example only: use the reserved id for its list position

[[partitions]]
name = "recovery_a"
backend = "/dev/mapper/rom2-fw-recovery_a"
read_only = false
metadata = "rom/rom2/recovery.vbmd"
```

Only writable `esp-file:` images and the selected ROM's thin COW mapper views are eligible. Direct physical partitions, loops, linear mapper targets, another ROM's mapper, and read-only projections fail closed. ESP files are grafted once by `provision esu`, not by this runtime module. Mapper backends use PID1's sysfs resolver rather than trusting a by-name symlink.

## Current-state and OTA interaction

No promotion/drop operation, menu, history, or receipt is added. Existing `fw-views` retains external-origin thin devices across boots. The module queries each thin target's live mapped-sector count: **any private mapped block means valid existing current state wins**, so OTA/fastboot changes are never overwritten by reapplying old configured metadata. A pristine zero-map view is seeded with the configured graft; subsequent boots preserve it. If valid preserved metadata differs, it logs `preserving current view; drop to reseed` without replacing it. Unusable current footer/metadata stops managed boot with `current metadata unusable; drop the view to reseed`, including an interrupted seed. Seed writes put metadata first, footer last, then fsync, block-cache flush for COW devices, and compare metadata/footer readback. This is deliberately whole-view conservative, not a claim that every private block is an OTA metadata write.

Existing explicit drop removes the mapper and deletes its reserved thin id; `fw-views` recreates an empty view on the next boot and this module seeds it again. Physical origins remain read-only. No promotion workflow exists in the public helper; promotion outside this helper is unchanged. A view already containing private data before this feature is installed is preserved rather than forcibly initialized.

ESP files are grafted once during provisioning, including when the tree-built image already contains structurally valid unsigned metadata. The runtime module skips them entirely, and Surfacer passes their current file bytes as-is to GBL; OTA/promotion writes win. An intentional host re-graft uses explicit `apply` to a new output, followed by the owner's normal installation workflow. No automatic drift reconciliation or marker sidecar is used. For physical-origin recovery execution GBL overlays the configured selected-slot metadata on the physical image; UEFI cannot read thin COW. Pending-OTA divergence between physical execution and Linux COW is the existing fake-rw model, not permission to erase COW state.

## Build and release inputs

```sh
cargo +nightly-2026-08-08 build --locked --release -p avb-graft-cli
NDK=/path/to/clean/android-ndk-r29
RUSTFLAGS="-C target-feature=+crt-static" \
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android35-clang" \
  cargo +nightly-2026-08-08 build --locked --release \
  --target aarch64-linux-android -p avb-graft-cli --bin avb-graft
```

Release CI builds `fw-views` and `avb-graft` with the existing static phone helpers. `esu-storage-helpers-aarch64-linux-android.tar.gz` includes `bin/{thin-activate,fw-views,avb-graft}`, `modules/{thin,fw-views,avb-graft}`, and the example manifest. Merge those inputs into the full ESP payload; they do not replace its ROM configuration, busybox, daemon, boot HAL, kernel modules or ROM-local `.vbmd` files. `esud boot-patch` requires `bin/avb-graft` to be a static target-architecture ELF when present.
