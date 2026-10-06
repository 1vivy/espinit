# esu

esu is an early-boot substrate forked from [KernelSU](https://github.com/tiann/KernelSU), not an Android root product. The kernel module, Rust PID-1 loader and Android daemon retain the generic KernelSU lifecycle and SELinux machinery. There is no Manager APK, application-root API, module ZIP installer, profile service or mutable module store.

Source and host checks are not proof of an enforcing device boot. The Cuttlefish device lane remains paused. Device overlayfs, stock HAL transitions and shared-credential isolation require separate phone evidence; no installation is implied by this README.

## Architecture and identity

- `esuinit` runs as PID 1, loads the ramdisk kernel modules, prepares the selected ROM view and hands off to the saved `/init.real`.
- `kernelesp.ko` provides the core UAPI, strict module relocation loader, KernelSU SELinux rules and boot-mode-gated init RC injection.
- `thin.ko` and `gpt.ko` provide thin storage and an in-memory projected partition view. `efivarfs.ko` exposes bdsvars through the standard efivarfs file API.
- `esud` runs directly from `/dev/esp/esu/bin/esud`. Android init mounts the ESP read-only at `/dev/esp` and efivarfs read-write at `/dev/efivars`, with `context=u:object_r:esu_file:s0`. The daemon does not mount either again.
- Runtime ROM identity is `BootedRom` in bdsvars. `Slot-<id>` supplies the authoritative number, 1 through 5. PID 1 temporarily mounts efivarfs at `/efivars`; the daemon and Boot HAL use `/dev/efivars`. Missing identity (or `direct`) is unmanaged; a managed identity with missing or malformed Slot fails closed. No bootconfig selector or default ROM number substitutes for it.
- The daemon's only writable persistent state is `/data/adb/esu/log`, created at post-fs-data. Modules, binaries and configuration never come from `/data/adb` or `/metadata/esu`.

The SELinux domain/type are `esu`/`esu_file`. The kernel installs upstream KernelSU rules with those names, plus the context-mount allowances. The Boot HAL is an ordinary ESP module, not kernel-specific policy or a private label/bind helper.

## Payload layout

```text
ESP /
  rom/<id>/esu.cpio
  esu/
    build-id
    manifest.toml
    roms/<id>.toml
    bin/{esuinit,esud,busybox,thin-activate,fw-views}
    modules/
      boot-hal/
        module.prop
        attrs
        sepolicy.rule
        vendor/bin/hw/android.hardware.boot-service.qti
      thin/{module.prop,pid1.sh,pid1-recovery.sh,recovery-ok}
      fw-views/{module.prop,pid1.sh,pid1-recovery.sh,recovery-ok}
    receipts/

per-ROM newc takeover archive (legacy-LZ4):
  init                         static esuinit, 0755
  init.real                    preserved stock init, 0755
  esu-build-id                  12 lowercase hex + LF
  lib/{kernelesp,thin,gpt,efivarfs}.ko
```

Kernel modules are ramdisk-only. Any `.ko` inside an ESP source payload is rejected by the host packager. PID 1 uses `/debug_ramdisk/esp` before handoff; Android uses the separate init-owned `/dev/esp` mount.

## Manifest and ROM configuration

See [`esu/manifest.example.toml`](esu/manifest.example.toml) and [`esu/rom.example.toml`](esu/rom.example.toml). Both reject unknown fields. The manifest requires:

- `schema_version = 1` and `rom = "roms"`;
- `modules`: the ordered kernel-module list, with `name`, `path = "lib/<name>.ko"` and parameters;
- `modules_order`: ESP KernelSU module IDs, for example `["boot-hal", "thin", "fw-views"]`. Earlier IDs have higher overlay precedence.

The ROM file requires `schema_version`, `id`, and `managed`; `partitions` and `firmware_views` are optional as permitted by managed-mode validation. `generation`, `rom_number`, `[platform]` and `recovery_packages` are not compatibility aliases and are rejected. ROM files are selected by bdsvars identity, not a filename inferred from a number. Number-dependent firmware validation runs against the Slot record, not host packaging guesses.

A managed ROM projects complete whole-device backends through `gpt` APPLY and verifies the exact QUERY result. Backend syntax recognizes exact sysfs by-name partitions, `/dev/mapper/<name>`, existing `/dev/loopN`, and preallocated `esp-file:<relative-path>` paths; the latter are **not admitted at runtime** below. Whole-LU devices, offsets and extent/FIEMAP APIs are not accepted. Firmware views use reserved thin IDs `(rom_number << 16) | index` and matching `/dev/mapper/rom<N>-fw-<name>` backends.

**ESP-file admission is blocked in this kernel/module build.** A loop-backed
file pins PID 1's contextless ESP vfat superblock through handoff. Android
init then cannot remount the same superblock with `context=esu_file`:
the disposable phone loop proof
`gbl-bds-lab/records/20261006T091345Z-phone-pinned-esp-mount` observed
`EINVAL` and `Same superblock, different security settings`. PID 1 rejects
*any* `esp-file:` backend with `EspFileMountUnqualified` before loop
attachment or GPT publication, whether the mapping is RO or RW. ROM 1's
current mapper/by-name-only configuration is unaffected. ROM ≥ 2
`esp-file:` boot needs an owner-approved kernel lifecycle that creates
one correctly labeled retained ESP superblock and a per-mount-RO Android
module view; additional SELinux allow rules or a second FAT mount cannot
repair the current design. This source tree does **not** qualify ROM ≥ 2.

`gpt.ko` never writes disk GPT metadata or changes partition boundaries. It is a naming/projection facility, not a hostile-root isolation boundary: raw whole-LU access is not filtered. Shadowed physical backends retain native access modes so DM/LVM projections can use them.

## Boot stages and modules

PID 1 checks the core UAPI and readiness, obtains bdsvars identity, loads the other kernel modules, runs `pid1.sh` (or `pid1-recovery.sh`), and applies the GPT projection. Script environment includes `ESU_ROM` and `ESU_ROM_NUMBER`. The thin helper validates the physical `userdata` LVM2 metadata and activates the `rom` VG without invoking a shell or mutating LVM metadata. `fw-views` creates external-origin firmware devices for secondary ROMs; see [its module documentation](esu/modules/fw-views/README.md).

Before init handoff PID 1 concatenates `modules/<id>/initrc/*.rc` in module order, prefixes each file with its source name, caps the result at 65536 bytes and sends the root-only set-once module-RC ioctl. It sends an empty buffer too: unset is not equivalent to empty. Recovery includes only modules carrying `recovery-ok`. No metadata-staged RC exists.

The core boot-mode ioctl is root-only and set-once (`1` Android, `2` recovery). Init's synchronous `esu-early` service runs before early HAL startup and uses `reboot_on_failure`. `esud early`:

1. Reads every selected module's `sepolicy.rule` strictly. Malformed rules or failed kernel updates propagate as boot failure.
2. Stages partition trees on executable tmpfs `/dev/esu`, mode 0700, without `nosuid`. Supported roots are `system`, `vendor`, `product`, `system_ext`, and `odm`; there is no `system/vendor` remapping.
3. Applies per-inode attrs from lines `/<partition>/<path> <octal-mode> <uid> <gid> <SELinux-context>`. Without an explicit entry it copies mode, owner and label from the existing target using no-follow metadata/xattr reads. A missing target/label is `OverlayAttrsMissing`, not an invented label.
4. Mounts one read-only, lowerdir-only overlay per partition: ordered module trees followed by the stock partition. There is no unobserved bind-mount fallback.
5. Runs `early.sh` with ESP BusyBox in module order, failing on nonzero status, then applies ROM isolation.

Post-fs, post-fs-data, services, boot-completed and recovery run the corresponding KernelSU scripts from the ESP in module order. Stages may be re-run from a root shell and are gated by the core boot mode, not manager state or a one-shot service event. `esud platform reload` reapplies policies and mounts missing overlays without running scripts.

ROM isolation preserves the shared `/metadata/shared/password_slots` bind, managed vold key-preservation property and numbered GSI password-slot identity. ROMs numbered 2 or higher deny boot HAL writes/ioctls to the UFS BSG node. The Boot HAL persists Slot and MergeStatus using efivarfs and retains the existing misc VAB mirror.

## Boot HAL module

The payload assembler places the built `gblbds-boot-hal` at `modules/boot-hal/vendor/bin/hw/android.hardware.boot-service.qti`. The checked-in attrs use `hal_bootctl_default_exec`, preserving the stock `vendor.boot-qti` service and domain transition. The module grants exactly the three efivarfs rules in [`sepolicy.rule`](esu/modules/boot-hal/sepolicy.rule); it introduces no new SELinux type or block-node access.

This fixed QTI target is device-specific. Merely assembling it does not establish another device's service compatibility. If device evidence later proves a tmpfs association denial, evaluate that specific rule then; no speculative fallback is shipped.

## Kernel compatibility and builds

All four phone LKMs use the shared KMI builder with the published **trimmed** ACK `android16-6.12` generation-6 output. This KMI generation is an upstream ABI identifier, not the removed payload generation system.

```sh
export KMI_SRC=/path/to/android16-6.12/source
export KMI_OUT=/path/to/complete/trimmed/gki-output
make -C kernel phone JOBS=13
make -C modules/thin JOBS=13
make -C modules/gpt JOBS=13
make -C modules/efivarfs JOBS=13
python3 scripts/kmi_modules.py verify --kmi-out "$KMI_OUT" \
  --module kernel/kernelesp.ko --module modules/thin/thin.ko \
  --module modules/gpt/gpt.ko --module modules/efivarfs/efivarfs.ko
```

Admission verifies architecture/type, module name, MODVERSIONS CRCs against `Module.symvers`, and non-versioned imports against `System.map`. Schema-2 compatibility receipts bind the module hash and all KMI reference input hashes. Do not repair CRCs, vermagic or receipts by hand. PID 1 and `esud insmod` share the strict runtime loader; unavailable imports fail before insertion.

Build Android userspace with a clean NDK r29 and the aarch64 Rust target. PID 1, thin-activate and fw-views must be static; esud and the HAL may use Android's dynamic runtime. `cargo ndk -t arm64-v8a --platform 35 build --release -p esud` builds the daemon. The HAL has its own [`build-android.sh`](payloads/boot-hal/build-android.sh).

## Host packaging and build identity

```sh
cargo run --locked -p esud -- boot-patch \
  --esuinit /build/esuinit --payload /build/esu \
  --modules-dir /build/modules --kmi-out "$KMI_OUT" \
  --rom rom1 --boot /build/stock/init_boot.img --out /build/new-output
```

The builder captures explicit regular-file inputs, verifies all four LKMs and receipts, preserves stock init, and publishes `esp/`, byte-identical top-level/per-ROM `esu.cpio`, unsigned `patched.img`, and `receipt.json`. It never discovers or opens a phone. See [the complete contract](userspace/esud/README.md).

Build identity is the first 12 lowercase hex digits of SHA256 over the lexicographically sorted artifact SHA256 values, each followed by one LF; duplicate hashes remain. `receipt.build_id_inputs` records the complete input set. The ESP `esu/build-id` and cpio `/esu-build-id` contain exactly that ID plus LF; JSON has no LF. PID 1 logs both and warns on disagreement without refusing boot. Identity is diagnostic, not a signature or an ABI gate.

## Failure evidence and diagnostic boundaries

Managed parse, module, projection, policy or handoff failures stop boot rather than falling back to stock. Early failure JSON under ESP `/esu/receipts/failure.json` records schema, nullable build_id, stage, component, stable error and bounded detail. A bounded RW-remount/temp-file/fsync/rename/fsync/RO-remount window preserves prior evidence until replacement succeeds. Receipt failure is also logged to kmsg; it never permits partial handoff.

`androidboot.init_fatal_panic=true` requests the AOSP sysrq panic path after recording failure, with sync/reboot/park fallback. `androidboot.esu.apss_minidump=true` preloads the vendor minidump dependency closure before ESP discovery. Lab-only `androidboot.esu.probe=<stage>` and the daemon boot watchdog diagnose stage reachability/stalls; they are not proof of an otherwise successful boot. Hang files are written only once post-fs-data has created the log directory.

The explicit `androidboot.mode=recovery` plus `androidboot.esu.recovery_passthrough=true` rescue path bypasses managed payload work and hands off to stock recovery. Normal recovery retains projection. Recovery OTA and device enforcing overlay behavior remain separate verification requirements.

## License and provenance

This fork retains KernelSU history and copyright notices. The core remains GPL-3.0; [`LICENSE`](LICENSE) is unchanged. `thin` and `gpt` are separate GPL-2.0-only modules with their own provenance. The Boot HAL, vendored varstore and lvm2-meta retain Apache-2.0; the daemon, PID-1, helpers and shared platform code retain their subtree licenses. See [`SECURITY.md`](SECURITY.md).
