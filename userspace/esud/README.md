# esud

The Android binary is generic KernelSU lifecycle/policy support without the manager. esuinit detaches its ESP and executable tmpfs before handing off to stock init. After the root switch, init reuses a writable `/debug_ramdisk`, or mounts a parent tmpfs only over the observed empty read-only directory, then recreates the contextless ESP with esuinit's selected device identity and RO/RW mode. A populated read-only parent fails closed. Init copies `esu/bin` onto fresh `/debug_ramdisk/esu` tmpfs and relabels it with stock toybox chcon in the esu domain. esud executes from `/debug_ramdisk/esu/bin/esud`; before running helpers or module scripts it verifies every staging label is exactly `esu_file` and the ESP root is exactly `vfat`. It creates a per-mount read-only, nosuid/nodev/noexec bind at `/dev/esp`; module home remains `/dev/esp/esu/modules`. Init also owns efivarfs. Writable persistent state is limited to `/data/adb/esu/log`.

Android commands: `early`, `post-fs`, `post-fs-data`, `services`, `boot-completed`, `recovery`, `sepolicy`, `insmod`, `resetprop`, `core set-boot-mode`, and `platform reload`. The executable also recognizes the `resetprop` invocation name. There is no install/uninstall, manager, module mutation, metamodule or profile command. Direct kernel hook publication makes `kernelesp` boot-resident; no daemon unload command stops services before a module removal that must return `EBUSY`.

Stages respect the core boot mode and upstream KernelSU safe mode (Android safe-mode properties or the kernel volume-down ioctl). They can be re-run by root. Safe mode skips module work without writing flags onto the ESP. `platform reload` reapplies admitted policies and mounts missing overlays without executing scripts. The same upstream-style plain `exec` RC callbacks serve Android and recovery; no callback has `reboot_on_failure`. Recovery therefore remains available after second-stage module errors. `post-fs-data` and `boot-completed` stay defined but never fire where recovery has no `/data` root or completed boot. Identity and ROM number come from bdsvars through `esu_platform::efivars`; an invalid selected Slot is a required-core error, not a default to ROM 1. Configuration is `/dev/esp/esu/roms/<id>.toml`.

`early.sh`, post-fs, post-fs-data and recovery scripts share a 35-second deadline
per stage. Optional spawn, exit and timeout failures are logged; later modules
run while time remains. An admitted module's regular `critical` marker opts
into stopping normal Android on observed failure. Recovery logs critical
module errors and continues. PID1 critical failures still block handoff in both
modes, independently of this second-stage rescue policy.

Recovery admits only modules with `recovery-ok`, including their scripts,
policies and overlays. `disable` and `remove` skip a module entirely;
`skip_mount` skips only its partition overlays. A timed-out process group is
killed and its shell reaped. `service.sh` and `boot-completed.sh` remain
detached, so their later exit status is not monitored. Scripts have separate
process groups and escape init's service cgroups before exec; background jobs
from a successful script survive the stage. Best-effort bootlog captures use
the same cgroup escape; logging failure never aborts a stage.

## ESP modules

Manifest `modules_order` defines policy/script ordering and overlay precedence (first is highest). Modules use `module.prop`, optional `sepolicy.rule`, `attrs`, lifecycle scripts, `initrc/*.rc`, and partition trees `system`, `vendor`, `product`, `system_ext`, `odm`. There is no remapping of `system/vendor`. Modules are optional unless marked `critical`; supplied `thin` and `fw-views` are critical, while `boot-hal` and `avb-graft` are optional. Final required storage and identity checks are not module policy and cannot be disabled with a flag.

Early processing applies admitted policies, stages usable trees into tmpfs `/dev/esu/<id>/<partition>` (private root 0700, no nosuid), mounts read-only lowerdir-only overlays, runs `early.sh` through staged BusyBox, then applies required ROM isolation. Malformed policy, failed staging, missing labels and overlay failures are reported under the module's optional/critical policy; no guessed-label or bind fallback is introduced. Recovery overlays only partitions already mounted. There is no product boot watchdog or forced-crash path (the opt-in lab `watchdog` module is separate). Attr lines are:

```text
/vendor/bin/hw/example 0755 0 2000 u:object_r:vendor_file:s0
```

Directory attrs may name `/vendor` itself. Every staged inode takes explicit attrs or existing target lstat/SELinux xattr metadata. Missing metadata is `OverlayAttrsMissing`; vfat labels/modes are never inherited. The affected optional layer is skipped rather than aborting every other module. Module source symlinks and special inodes are not supported by the host package contract.

When supplied, the optional Boot HAL module ships `esu/bin/esu-bootctl` plus checked-in `esu/modules/boot-hal/{module.prop,sepolicy.rule,initrc/boot-hal.rc}`: its own `esu.bootctl` init service in the `esu_bootctl` domain, which stops the stock boot-HAL service by its exec label and then serves `IBootControl/default`. No `/vendor` overlay or attrs are used. The Cuttlefish assembler also accepts payloads without a Boot HAL. That executable is built by `esu/modules/boot-hal/layer/build-android.sh` against the generic-bootctl submodule in the same module directory (`esu/modules/boot-hal/generic-bootctl`, pinned by its gitlink, linked as a serving library only); the esu backend, the GBS1/GBM1 wire records, the misc mirror and the stock stop live in the layer crate. Only regular files at the module root and `initrc/` ship; the submodule and `layer/` stay in the source tree.

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

The packager reports invalid optional module metadata/policy without treating
it as a core payload failure; admitted critical modules remain strict.
`disable` and `remove` skip module validation. Supplied executable architecture,
path containment and input integrity checks remain mandatory.

`--modules-dir` must contain `kernelesp.ko`, `thin.ko`, `gpt.ko`, `efivarfs.ko`, `efivar_store.ko` and each corresponding schema-2 `.ko.compat.json`. All five must be declared as `lib/<name>.ko`. The frontend and the EFVS backend (a C module linking the Rust engine) are separate images; PID 1 loads the frontend, then the backend with its device parameter, before mounting. `kernelesp.ko` must already be loaded when the backend loads: the backend resolves its optional I/O credential provider (`efivar_store_io_enter/leave`) with `symbol_get` at init, and the manifest's core-first ordering guarantees that. The embedded `scripts/kmi_modules.py verify` runs with Python 3.11+ against explicit `--kmi-out`; stale receipts, absent imports and CRC mismatches fail closed. Compatibility receipts are inputs, not ESP kernel modules. Any `.ko` anywhere inside the payload is rejected.

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

The stream contains one normalized newc overlay: `init` (esuinit), `init.esureal` (effective prior init or wrapper), both 0755; `lib/` and the four modules (0644); and `esu-build-id` (0644). The source boot/init_boot must be v3/v4 with a valid executable effective init. Existing `init.esureal` collisions fail; an existing `init.real` and other wrapper files remain untouched. Use the ROM's actual init_boot, including its root wrapper, rather than a cleaned image that bypasses it. The patched image preserves original ramdisks and removes old rdinit/ROM-selector cmdline tokens; runtime selection uses bdsvars. It omits stale GKI/AVB signatures and is not a signed deployment artifact.

## Receipt and build ID

`receipt.json` schema 1 retains tool/version/verifier identity, selected ROM, archive path, boot contract, complete `sources`, `artifacts` (`sha256`, `size`, `mode`), directories and KMI verification report. It replaces payload/tool generation fields with `build_id` and `build_id_inputs`.

The stable build-ID serialization is:

1. Collect the captured source artifact hashes: all payload files, supplied PID 1, all files captured from `--modules-dir` (including compatibility receipts), and stock boot image. Their logical names and hashes are recorded in `build_id_inputs`.
2. Sort **hash values** lexicographically, retaining duplicates. Encode each lowercase 64-hex SHA256 followed by one ASCII LF, including the last value.
3. SHA256 that byte stream; take its first 12 lowercase hex characters.

Generated `esu/build-id`, takeover archives, patched image and output receipt are excluded to avoid recursion. The source payload must not already contain `build-id`. ESP `esu/build-id` and cpio `/esu-build-id` contain exactly the 12 characters plus one LF. JSON `build_id` has no LF. The recipe is independent of input enumeration order; changes to any captured artifact affect identity. This is a diagnostic fingerprint, not authentication. PID 1 warns on cpio/ESP mismatch without failing boot.

## Verification

Host tests construct synthetic ELF, KMI receipts and stock init_boot fixtures but exercise the real boot-patch path and embedded verifier, then inspect output ESP files, receipt hashes and decoded newc members. They cover deterministic build-ID framing, rejected unsafe inputs, preserved stock init, module admission/escalation, and strict policy/attribute parsing without imposing blanket boot failure on optional modules. They do not establish real-kernel compatibility or device boot success.

Run workspace fmt/clippy/tests, Android esud clippy with NDK r29, `python3 -m unittest tools.cuttlefish.test_assemble`, real module KMI verification, and separate device gates before deployment.
