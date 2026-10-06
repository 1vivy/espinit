# esud

The Android binary is generic KernelSU lifecycle/policy support without the manager. It executes directly from `/dev/esp/esu/bin/esud`; module home is the read-only `/dev/esp/esu/modules`. Init owns the ESP and efivarfs mounts. Writable state is limited to `/data/adb/esu/log`, created at post-fs-data.

Android commands: `early`, `post-fs`, `post-fs-data`, `services`, `boot-completed`, `recovery`, `sepolicy`, `insmod`, `unload`, `resetprop`, `core set-boot-mode`, `platform reload`, and the internal boot watchdog. The executable also recognizes the `resetprop` invocation name. There is no install/uninstall, manager, module mutation, metamodule or profile command.

Stages use the core boot mode rather than a manager/safe-mode/one-shot gate. They can be re-run by root. `platform reload` reapplies strict policy and mounts missing overlays, without executing scripts. Identity and ROM number come from bdsvars through `esu_platform::efivars`; a selected ROM with an invalid Slot fails, rather than defaulting to number 1. Configuration is `/dev/esp/esu/roms/<id>.toml`.

## ESP modules

Manifest `modules_order` defines policy/script ordering and overlay precedence (first is highest). Modules use `module.prop`, optional `sepolicy.rule`, `attrs`, lifecycle scripts, `initrc/*.rc`, and partition trees `system`, `vendor`, `product`, `system_ext`, `odm`. There is no remapping of `system/vendor`.

Early processing applies policies strictly, stages trees into tmpfs `/dev/esu/<id>/<partition>` (private root 0700, no nosuid), mounts read-only lowerdir-only overlays, executes every `early.sh` through ESP BusyBox, then runs ROM isolation. Attr lines are:

```text
/vendor/bin/hw/android.hardware.boot-service.qti 0755 0 2000 u:object_r:hal_bootctl_default_exec:s0
```

Directory attrs may name `/vendor` itself. Every staged inode takes explicit attrs or existing target lstat/SELinux xattr metadata. Missing metadata fails `OverlayAttrsMissing`; vfat labels/modes are never inherited. No bind fallback is implemented without device overlayfs EINVAL evidence. Module source symlinks and special inodes are not supported by the host package contract.

The Boot HAL's ordinary module is generated from its built executable using the checked-in `esu/modules/boot-hal/{module.prop,attrs,sepolicy.rule}`. It replaces the fixed stock QTI executable without changing the service name or SELinux transition.

## Linux host boot-patch

The Linux binary exposes only `boot-patch`. It packages explicit files, never mounts, flashes, discovers a phone, executes an input binary/script, patches a kernel or enables a shell.

```sh
cargo build --locked --release -p esud --target x86_64-unknown-linux-gnu
esud boot-patch \
  --esuinit /build/esuinit \
  --payload /build/payload \
  --modules-dir /build/modules \
  --kmi-out /build/kmi-out \
  --rom rom1 \
  --boot /build/stock/init_boot.img \
  --out /build/new-output
```

`--payload` is the contents of ESP `/esu`, not the ESP root. It contains strict schema-1 `manifest.toml`, `roms/<id>.toml`, `bin/esud`, static `bin/busybox`, static `bin/thin-activate`, any additional script helpers, and the declared module directories. The ROM's required ID must match the filename. There are no payload generation pins, binary generation notes or `[platform]` package manifests. Number-dependent runtime ROM constraints remain the responsibility of the authoritative Slot record.

`--modules-dir` must contain `kernelesp.ko`, `thin.ko`, `gpt.ko`, `efivarfs.ko` and each corresponding schema-2 `.ko.compat.json`. All four must be declared as `lib/<name>.ko`. The embedded unmodified `scripts/kmi_modules.py verify` runs with Python 3.11+ against explicit `--kmi-out`; stale receipts, absent imports and CRC mismatches fail closed. Compatibility receipts are inputs, not ESP kernel modules. Any `.ko` anywhere inside the payload is rejected.

All executables must match the PID-1 ELF architecture. PID 1, BusyBox and early helpers must be static. No supplied executable is run. Inputs must be regular files; symlink ancestors/entries, traversal, FAT case collisions, unsafe names and special inodes fail. The output must not exist, its parent must exist, and it must be outside the source tree. A preexisting `bin/esuinit` must match `--esuinit` byte-for-byte.

The builder captures inputs before validation and publishes only after success. Output:

```text
new-output/
  receipt.json
  patched.img                 unsigned conventional host-test image
  esu.cpio                    canonical legacy-LZ4 stream
  esp/
    rom/rom1/esu.cpio          byte-identical canonical stream
    esu/
      build-id
      manifest.toml
      roms/rom1.toml
      bin/...
      modules/...
      receipts/
```

The stream contains one normalized newc overlay: `init` (esuinit), `init.real` (effective stock init), both 0755; `lib/` and the four modules (0644); and `esu-build-id` (0644). The stock boot/init_boot must be v3/v4 with a valid static effective init. Existing `init.real` collisions fail. The patched image preserves stock ramdisks and removes old rdinit/ROM-selector cmdline tokens; runtime selection uses bdsvars. It omits stale GKI/AVB signatures and is not a signed deployment artifact.

## Receipt and build ID

`receipt.json` schema 1 retains tool/version/verifier identity, selected ROM, archive path, boot contract, complete `sources`, `artifacts` (`sha256`, `size`, `mode`), directories and KMI verification report. It replaces payload/tool generation fields with `build_id` and `build_id_inputs`.

The stable build-ID serialization is:

1. Collect the captured source artifact hashes: all payload files, supplied PID 1, all files captured from `--modules-dir` (including compatibility receipts), and stock boot image. Their logical names and hashes are recorded in `build_id_inputs`.
2. Sort **hash values** lexicographically, retaining duplicates. Encode each lowercase 64-hex SHA256 followed by one ASCII LF, including the last value.
3. SHA256 that byte stream; take its first 12 lowercase hex characters.

Generated `esu/build-id`, takeover archives, patched image and output receipt are excluded to avoid recursion. The source payload must not already contain `build-id`. ESP `esu/build-id` and cpio `/esu-build-id` contain exactly the 12 characters plus one LF. JSON `build_id` has no LF. The recipe is independent of input enumeration order; changes to any captured artifact affect identity. This is a diagnostic fingerprint, not authentication. PID 1 warns on cpio/ESP mismatch without failing boot.

## Verification

Host tests construct synthetic ELF, KMI receipts and stock init_boot fixtures but exercise the real boot-patch path and embedded verifier, then inspect output ESP files, receipt hashes and decoded newc members. They also cover deterministic build-ID framing, rejected inputs, preserved stock init, malformed strict policy and overlay attr parsing/failure propagation. They do not establish real-kernel compatibility or device boot success.

Run workspace fmt/clippy/tests, Android esud clippy with NDK r29, `python3 -m unittest tools.cuttlefish.test_assemble`, real module KMI verification, and separate device gates before deployment.
