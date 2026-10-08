# esu

esu is an early-boot substrate forked from [KernelSU](https://github.com/tiann/KernelSU), not an Android root product. The kernel module, Rust PID-1 loader and Android daemon retain the generic KernelSU lifecycle and SELinux machinery. There is no Manager APK, application-root API, module ZIP installer, profile service or mutable module store.

Source and host checks are not proof of an enforcing device boot. The Cuttlefish device lane remains paused. Device overlayfs, stock HAL transitions and shared-credential isolation require separate phone evidence; no installation is implied by this README.

## Architecture and identity

- `esuinit` runs as PID 1, loads the ramdisk kernel modules, prepares the selected ROM view and hands off to the saved `/init.esureal`, preserving any existing init wrapper and its `/init.real`.
- `kernelesp.ko` provides the core UAPI, strict module relocation loader, KernelSU SELinux rules and boot-mode-gated init RC injection. Ownership-checked direct wrappers for `execve`, `execveat` and `setresuid` call their saved originals; they do not compete with KernelSU's `sys_enter` redirect for one rewritten syscall number.
- `thin.ko` and `gpt.ko` provide thin storage and an in-memory projected partition view. Pristine upstream `efivarfs.ko` is the filesystem frontend; separate `efivar_store.ko` (a C module linking the Rust EFVS engine) serves the EFVS checkpoint/log on the bdsvars partition. Firmware initializes a blank partition; Linux never formats or compacts it. `kernelesp.ko` exports `efivar_store_io_enter/leave` (GPL) so backing-device I/O runs under the `esu` credential regardless of the calling domain; `efivar_store` resolves them with `symbol_get` because the relocating loader binds only vmlinux symbols.
- `esud` runs from `/debug_ramdisk/esu/bin/esud` on executable tmpfs. esuinit detaches its ESP and staging tmpfs before `/init.esureal`; second-stage init reuses a writable `/debug_ramdisk`, or mounts a parent tmpfs only when the read-only directory is empty, then recreates both child mounts and copies the ESP binaries with stock toybox. A populated read-only parent fails closed instead of hiding content. The core relabels the staged tree in the `esu` domain. esud verifies the selected device and stock `vfat` label, then binds the ESP read-only at `/dev/esp`. Only efivarfs uses `context=u:object_r:esu_file:s0`.
- Runtime ROM identity is `BootedRom` in bdsvars. `Slot-<id>` supplies the authoritative number, 1 through 5. PID 1 temporarily mounts efivarfs at `/efivars`; the daemon and Boot HAL use `/dev/efivars`. Missing identity (or `direct`) is unmanaged; a managed identity with missing or malformed Slot fails closed. No bootconfig selector or default ROM number substitutes for it.
- The daemon's only writable persistent state is `/data/adb/esu/log`, created at post-fs-data. Modules, binaries and configuration never come from `/data/adb` or `/metadata/esu`.

The SELinux domain/type are `esu`/`esu_file`. The kernel retains upstream KernelSU domain behavior; its concrete wildcard vfat grants are narrowed to data read/search and bind-remount access. No new vfat execution permission is granted to init. `esu_file` tmpfs association supports staged executable xattrs; the context-mount allowances apply only to efivarfs. The Boot HAL is an ordinary ESP module, not kernel-specific policy or a private label/bind helper.

## Payload layout

```text
ESP /
  rom/<id>/esu.cpio
  esu/
    build-id
    manifest.toml
    roms/<id>.toml
    bin/{esuinit,esud,busybox,thin-activate,fw-views,avb-graft,esu-bootctl}
    modules/
      boot-hal/
        module.prop
        sepolicy.rule
        initrc/boot-hal.rc
      thin/{module.prop,pid1.sh,pid1-recovery.sh,recovery-ok,critical}
      fw-views/{module.prop,pid1.sh,pid1-recovery.sh,recovery-ok,critical}
      avb-graft/{module.prop,pid1.sh,pid1-recovery.sh,recovery-ok}
    receipts/

per-ROM newc takeover archive (legacy-LZ4):
  init                         static esuinit, 0755
  init.esureal                 preserved prior init or wrapper, 0755
  esu-build-id                  12 lowercase hex + LF
  lib/{kernelesp,thin,gpt,efivarfs,efivar_store}.ko
```

Kernel modules are ramdisk-only. Any `.ko` inside an ESP source payload is rejected by the host packager. esuinit mounts `/debug_ramdisk/esp` without SELinux context options and detaches it before stock init can cover that path. Open loop backing files retain their references. Second-stage init mounts the same device with the same RO/RW mode; `/dev/esp` is a read-only, nosuid/nodev/noexec bind view whose per-mount RO flag does not freeze writable loop backing.

## Manifest and ROM configuration

See [`esu/manifest.example.toml`](esu/manifest.example.toml) and [`esu/rom.example.toml`](esu/rom.example.toml). Both reject unknown fields. The manifest requires:

- `schema_version = 1` and `rom = "roms"`;
- `modules`: the ordered kernel-module list, with `name`, `path = "lib/<name>.ko"` and parameters;
- `modules_order`: ESP KernelSU module IDs, for example `["boot-hal", "thin", "fw-views", "avb-graft"]`. Earlier IDs have higher overlay precedence.

The ROM file requires `schema_version`, `id`, and `managed`; `partitions` and `firmware_views` are optional as permitted by managed-mode validation. `generation`, `rom_number`, `[platform]` and `recovery_packages` are not compatibility aliases and are rejected. ROM files are selected by bdsvars identity, not a filename inferred from a number. Number-dependent firmware validation runs against the Slot record, not host packaging guesses.

A managed ROM projects complete whole-device backends through `gpt` APPLY and verifies the exact QUERY result. Backend syntax recognizes exact sysfs by-name partitions, `/dev/mapper/<name>`, existing `/dev/loopN`, and preallocated `esp-file:<relative-path>` paths; the latter use the detached-mount lifecycle below. Whole-LU devices, offsets and extent/FIEMAP APIs are not accepted. Firmware views use reserved thin IDs `(rom_number << 16) | index` and matching `/dev/mapper/rom<N>-fw-<name>` backends.

**Host packaging (2026-10-07):** `esud boot-patch` accepts
`esp-file:rom/<selected-id>/...` references without requiring those image files
inside the esu payload: `rom-bootgen` installs the ROM-owned images on the ESP.
Other ROM IDs remain refused. `esp-file:esu/...` backends still require a
nonempty file in the supplied payload; configuration path validation is unchanged.

**ESP-file lifecycle (2026-10-07): device-proven for managed ROM1 normal boot.**
The disposable phone proof `20261006T091345Z-phone-pinned-esp-mount` established
that an ESP-file loop pins the original contextless FAT superblock, so adding
`context=esu_file` on a later mount fails `EINVAL`. Retaining the visible mount
was not a solution: `20261007T014108Z-phone-loop` panicked in stock init's
SwitchRoot after init overmounted `/debug_ramdisk`.

esuinit verifies one owned contextless ESP before projection, stages binaries
on executable tmpfs for PID-1 scripts, then detaches both mounts before exec.
Through the existing set-once RC handoff it publishes one fail-fast `early-init`
shell action carrying the selected major/minor and RO/RW mode. After the root
switch, the action reuses a writable `/debug_ramdisk`; when the stock root
provides the observed empty read-only directory, it mounts an owned parent
tmpfs. Existing content is never covered. Stock toybox then recreates the block
node, mounts the ESP with matching access and no security context override,
creates a new executable staging tmpfs, and copies binaries. The core's existing
`on init` action labels staging `u:object_r:esu_file:s0` with toybox before
executing `esud early`. Bootstrap and admitted module RC share the existing
65536-byte limit.

esud checks the staging filesystem and every inode label, selected ESP device
identity and exact `u:object_r:vfat:s0` root label before creating the
per-mount-RO `/dev/esp` bind. Optional module failures are reported without
rebooting Android; see the critical-module policy below. Disposable namespace
runs passed the post-switch parent cases: empty read-only replacement, populated
read-only refusal without hiding content, and existing writable-mount reuse.
The real generation-6 arm64 GKI also loaded this kernelesp beside the stock
KernelSU module and exercised both modules' overlapping exec/setresuid paths.
Phone record `20261007T081340Z-phone-esu-repair` then proved the full path:
Android booted Enforcing; active policy retained both `esu`/`esu_file` and
`ksu`/`ksu_file`; second stage cached the esu SIDs; the expected parent, ESP and
staging mounts were present; and every esud callback through boot-completed ran
as UID/GID 0 in `u:r:esu:s0` and exited 0. ROM >= 2 remains unproven.

Direct syscall publication pins `kernelesp.ko` for the boot lifetime. A later
hook can retain a raw saved-original pointer, and the init-RC file-operation
proxies have no release handshake, so table ownership plus an RCU grace period
cannot prove unload safety. Installation failures keep `ESU_STATE_READY` clear;
PID 1 rejects the retained but unready core. The former destructive `esud
unload` command was removed rather than stopping Android services before a
predictable `EBUSY`.

`gpt.ko` never writes disk GPT metadata or changes partition boundaries. It is a naming/projection facility, not a hostile-root isolation boundary: raw whole-LU access is not filtered. Shadowed physical backends retain native access modes so DM/LVM projections can use them.

## Boot stages and modules

PID 1 checks the core UAPI and readiness, obtains bdsvars identity, loads the other kernel modules, runs `pid1.sh` (or `pid1-recovery.sh`), and applies the GPT projection. Script environment includes `ESU_ROM` and `ESU_ROM_NUMBER`. The thin helper validates the physical `userdata` LVM2 metadata and activates the `rom` VG without invoking a shell or mutating LVM metadata. `fw-views` creates external-origin firmware devices for secondary ROMs; see [its module documentation](esu/modules/fw-views/README.md).

The generic [`avb-graft` tool/module](esu/modules/avb-graft/README.md) seeds configured `[[partitions]].metadata` on writable ROM-local views after firmware views and before GPT projection. It never writes physical origins; existing thin/ESP state wins over stale metadata. The module documentation includes host `apply`/`extract` usage, a real avbtool smoke recipe, drop interaction and release package inputs.

Before init handoff PID 1 concatenates admitted `modules/<id>/initrc/*.rc` in module order and sends the root-only set-once module-RC ioctl. Bootstrap space is reserved within the 65536-byte total; an invalid or oversized optional module's RC is skipped as a unit, never truncated. Missing optional RC never makes stock init's own RC unreadable. Recovery includes only modules carrying `recovery-ok`. No metadata-staged RC exists.

The core boot-mode ioctl is root-only and set-once (`1` Android, `2` recovery). Both modes use upstream-style synchronous `exec` callbacks, including `esud early` before early HAL startup; there is no blanket `reboot_on_failure` service. Modules are optional unless their directory contains a regular `critical` marker. The supplied `thin` and `fw-views` modules carry it; `boot-hal` and `avb-graft` do not. An admitted critical module's PID-1 failure blocks handoff in either mode. After stock init starts, an observed critical-module failure stops normal Android through the generic reboot/park path; recovery reports it and continues. Optional failure does not suppress Android's own AVB or init failure behavior.

`disable` and `remove` skip a module without modifying the read-only ESP. `recovery-ok` admits it in recovery; `skip_mount` skips its partition overlays, not its scripts. Daemon safe mode follows KernelSU's Android properties and volume-down ioctl and skips module work; it does not rewrite ESP flags. Required kernel modules, ROM identity and final backend/GPT validation remain independent safety checks. `esud early`:

1. Applies each admitted module's `sepolicy.rule`. Invalid rules or failed updates are reported under that module's optional/critical policy; explicit `esud sepolicy` commands still return errors.
2. Stages partition trees on executable tmpfs `/dev/esu`, mode 0700, without `nosuid`. Supported roots are `system`, `vendor`, `product`, `system_ext`, and `odm`; there is no `system/vendor` remapping.
3. Applies per-inode attrs from lines `/<partition>/<path> <octal-mode> <uid> <gid> <SELinux-context>`. Without an explicit entry it copies mode, owner and label from the existing target using no-follow metadata/xattr reads. A missing target/label is `OverlayAttrsMissing`, not an invented label.
4. Mounts read-only, lowerdir-only overlays from usable module trees followed by the stock partition. Optional policy/staging failures omit that module's layers; a failed combined partition mount leaves the stock partition visible. Normal Android stops if a failed mount has a critical contributor. No guessed labels or bind fallback is used. Recovery overlays only partitions already mounted.
5. Runs bounded `early.sh` scripts with staged tmpfs BusyBox in module order, reporting optional failures and continuing while the stage deadline permits, then applies required ROM isolation.

Post-fs, post-fs-data, services, boot-completed and recovery run the corresponding KernelSU scripts from the ESP in module order. Stages may be re-run from a root shell and respect boot mode, safe mode and module flags. Service and boot-completed scripts remain detached, not monitored daemons. `esud platform reload` reapplies policies and mounts missing overlays without running scripts.

ROM isolation preserves the shared `/metadata/shared/password_slots` bind, managed vold key-preservation property and numbered GSI password-slot identity. ROMs numbered 2 or higher deny boot HAL writes/ioctls to the UFS BSG node. The Boot HAL persists Slot and MergeStatus using efivarfs and retains the existing misc VAB mirror.

## Boot HAL module

When a Boot HAL executable is supplied, the payload carries it as `esu/bin/esu-bootctl` and the `boot-hal` module defines its own init service, `esu.bootctl` (`class early_hal`, `critical`, enforced domain `esu_bootctl`), in [`initrc/boot-hal.rc`](esu/modules/boot-hal/initrc/boot-hal.rc); it can also assemble without this module. Nothing is overlaid onto `/vendor`. An `on post-fs` exec of `esu-bootctl --stop-stock` in the `esu` domain stops every init service whose command carries the stock `hal_bootctl_default_exec` label (`ctl.stop`, selected by label, no stored names or paths) before `class_start early_hal`, so the stock HAL never starts; the serving process never waits on `ctl.stop`, because at `late-fs` init can sit in `mount_all` while vold waits on IBootControl. [`sepolicy.rule`](esu/modules/boot-hal/sepolicy.rule) grants `esu_bootctl` only what serving needs (binder, its own `/proc` entries, logd, `servicemanager.ready`, misc) and removes `service_manager`/`hwservice_manager` `add` from the stock `hal_bootctl_server` attribute so a later restart of the stock HAL cannot take the name. kernelesp's `deny` only clears the exact avtab key, so the rule names the attribute AOSP grants through as well as the concrete stock domain.

The Boot HAL is one module, `esu/modules/boot-hal/`: the [generic-bootctl](https://github.com/1vivy/generic-bootctl) repository is its `generic-bootctl` submodule (pinned by gitlink), used only as a serving library (`bootctl_unified::serve`: AIDL V1 first, HIDL 1.0-1.2 only if refused; slot health policy and the frozen AIDL V1 dispatch). How the stock HAL is replaced is each consumer's payload method: generic-bootctl's own standalone module overlays the stock path, esu stops the stock service and runs its own. The `layer/` crate supplies the esu `Service` (`EsuBackend`, the GBS1/GBM1 wire records, the misc VAB mirror, an always-writable gate and the preserve-nonzero success policy) and the label-based stock stop. It links `bootctl-unified` with `default-features = false`, so the NDK build stays `--locked --offline` and the produced ELF keeps exactly `libbinder_ndk.so`, `libc.so` and `libdl.so`; see [`layer/README.md`](esu/modules/boot-hal/layer/README.md).

## Kernel compatibility and builds

All four phone LKMs use the shared KMI builder with the published **trimmed** ACK `android16-6.12` generation-6 output. This KMI generation is an upstream ABI identifier, not the removed payload generation system.

```sh
export KMI_SRC=/path/to/android16-6.12/source
export KMI_OUT=/path/to/complete/trimmed/gki-output
make -C kernel phone JOBS=13
make -C modules/thin JOBS=13
make -C modules/gpt JOBS=13
make -C modules/efivarfs JOBS=13
EFIVAR_STORE_REPO=/path/to/efivar-store make -C modules/efivar_store JOBS=4
python3 scripts/kmi_modules.py verify --kmi-out "$KMI_OUT" \
  --module kernel/kernelesp.ko --module modules/thin/thin.ko \
  --module modules/gpt/gpt.ko --module modules/efivarfs/efivarfs.ko \
  --module modules/efivar_store/efivar_store.ko
```

Admission verifies architecture/type, module name, MODVERSIONS CRCs against `Module.symvers`, and non-versioned imports against `System.map`. Schema-2 compatibility receipts bind the module hash and all KMI reference input hashes. Do not repair CRCs, vermagic or receipts by hand. PID 1 and `esud insmod` share the strict runtime loader; unavailable imports fail before insertion.

The backend build checks out only `modules/efivar_store/SOURCE_REVISION` in an
ignored private clone; `EFIVAR_STORE_REPO` overrides the default public repository
for local integration. Its schema-2 receipt binds the final module and matching
KMI inputs. See [frontend provenance](modules/efivarfs/PROVENANCE.md) and
[backend build/toolchain provenance](modules/efivar_store/PROVENANCE.md).
The backend is a C kbuild module that links the freestanding Rust EFVS engine, so it
needs no kernel Rust crates (the phone's GKI exports different ones). Its toolchain is
the pinned nightly with `-Z build-std` plus `LLVM=/usr/bin/` (`EFVS_LLVM` overrides);
see the provenance document.

PID 1 loads `kernelesp`, then the efivarfs frontend, then the EFVS backend with
the discovered bdsvars `dev=major:minor`, before mounting efivarfs or reading
BootedRom. A missing partition is unmanaged; a malformed/blank store fails
closed without writing it. ESP manifests declare both EFI modules, but their
images remain cpio-only.

Build Android userspace with a clean NDK r29 and the aarch64 Rust target. PID 1, thin-activate, fw-views and avb-graft must be static; esud and the HAL may use Android's dynamic runtime. `cargo ndk -t arm64-v8a --platform 35 build --release -p esud` builds the daemon. The HAL has its own [`build-android.sh`](esu/modules/boot-hal/layer/build-android.sh), which builds the layer against the generic-bootctl submodule (`git submodule update --init`, then `ESU_NDK=/path/to/android-ndk-r29 bash esu/modules/boot-hal/layer/build-android.sh`).

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

Required managed-boot identity, kernel-module, storage/projection or handoff failures stop boot rather than falling back to a different filesystem view. Admitted ESP module failures follow the explicit optional/critical policy above. PID-1 failure JSON under ESP `/esu/receipts/failure.json` records schema, nullable build_id, stage, component, stable error and bounded detail. A bounded RW-remount/temp-file/fsync/rename/fsync/RO-remount window preserves prior evidence until replacement succeeds. Receipt failure is also logged to kmsg; it never permits partial handoff.

A classified PID-1 failure also records the failure path in the misc bootloader message before the panic-or-restart branch: `bootonce-bootloader` in command[0..32) and `esu:<stage>:<component>` in status[32..64), written in place with every byte from offset 64 onward untouched, so the next ordinary restart enters the one-shot fastboot path through Surfacer and GBL instead of a silent hang. The partition is resolved from the kernel's sysfs `PARTNAME` and opened through a node esu creates, never from `/dev/block/by-name`, which does not exist yet at that point. The reboot stays a plain restart, because `RESTART2 bootloader` would skip Surfacer and GBL. The BCB write and the receipt are both best effort: a missing partition, node or device is logged to kmsg and never delays the stop path.

Generic evidence remains Android logging (`esu` tag), fatal error details in kmsg, and best-effort `logcat.log`/`dmesg.log` captures under `/data/adb/esu/log` after post-fs-data. Capture failure does not fail a boot stage. The product ships no stage-crash probes, APSS minidump preloader, fatal SysRq override and no boot watchdog: the lab-only [`watchdog`](esu/modules/watchdog/README.md) ESP module is an opt-in payload module, absent from the default `modules_order`, and the only deadline-driven reset in the tree — on its deadline it writes the kernel-log tail to ESP `esu/receipts/watchdog.txt`, records `esu:watchdog:boot_completed` in the same BCB command and restarts. Crash forcing and Qualcomm 900e collection belong to the external lab. AOSP init may still honor its own `androidboot.init_fatal_panic` setting after handoff.

The explicit `androidboot.mode=recovery` plus `androidboot.esu.recovery_passthrough=true` rescue path bypasses managed payload work and hands off to stock recovery. Normal recovery retains projection. Recovery OTA and device enforcing overlay behavior remain separate verification requirements.

## License and provenance

This fork retains KernelSU history and copyright notices. The core remains GPL-3.0; [`LICENSE`](LICENSE) is unchanged. `thin` and `gpt` are separate GPL-2.0-only modules with their own provenance. The Boot HAL, vendored varstore, lvm2-meta and esu-config retain Apache-2.0 (esu-config is vendored from gobbl, see [`userspace/esu-config/PROVENANCE.md`](userspace/esu-config/PROVENANCE.md)); the daemon, PID-1, helpers and shared platform code retain their subtree licenses. See [`SECURITY.md`](SECURITY.md).
