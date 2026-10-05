# espinit

espinit is an early-boot substrate forked from [KernelSU](https://github.com/tiann/KernelSU), not an Android root product. It retains KernelSU's kernel-module lifecycle, hook design, Rust PID-1/daemon split, and aarch64, x86_64, and riscv64 build shape where applicable. App-facing root grants, the Manager APK, and Manager-dependent packaging are outside the product.

**Status:** the public identity/source baseline, managed PID-1 loader, `thin.ko`, and `gpt.ko` are implemented. Host checks and an isolated Cuttlefish-kernel smoke prove module load/readiness, APPLY/QUERY, projected naming and I/O, read-only and metadata-write rejection, flush, and unload. Both modules build against the phone-matched kernel; `gpt.ko`'s five non-KMI imports are verified against its exact `vmlinux`. GBL packaging and physical-phone validation remain incomplete; this is not yet a supported installation procedure.

The Platform modules phase is implemented in source as described below; its
new host tests, Android artifacts, policy and boot ordering await verification.

## Architecture and identity

- `espinit` is the early PID-1 binary; it prepares the managed boot and eventually transfers control to Android's real init without changing PID.
- `espinit.ko` is the core kernel module. Retaining the inherited lifecycle and existing direct syscall-table hook facilities is intentional reuse of the boot substrate, not KernelSU Manager or root-product compatibility; do not introduce a second hook framework.
- `espinitd` is the Android-side daemon, not a `su` replacement or an application privilege broker. It is installed under the mutable state root and executed by Android init as `/metadata/espinit/espinitd`; the ESP holds only its read-only payload copy.
- `/debug_ramdisk/esp/espinit` is the runtime ESP payload root; `/metadata/espinit` is the persistent Android working root after successful handoff. Early failure receipts use the ESP, not projected metadata. Do not use KernelSU's `/data/adb/ksu` state.
- The SELinux domain/type are `espinit`/`espinit_file` (`u:r:espinit:s0`, `u:object_r:espinit_file:s0`); `espinitd` keeps `espinit_file` on its own binary. Staged package inodes start as `metadata_file`; tiny-espsu applies the dedicated HAL and bdsvars labels before use. Public module, ioctl/install magic, anonymous-inode, and socket identities are distinct from KernelSU: the anonymous inodes are `[espinit]` and `[espinit_fdwrapper]`, and the info surface reports only the LKM flag. Private `ksu_` implementation prefixes remain internal, not compatibility interfaces.
- `gpt.ko` is a later, separate ESP module, never another name for the core module.

A real KernelSU installation must not be detected as an espinit module, satisfy an espinit self-check, or share espinit state/control endpoints. Distinct identity is necessary, but simultaneous hook ownership still requires integration testing; coexistence is not promised by this baseline.

### Non-goals

No Manager APK, app-root API, `su` compatibility, allowlist/profile product, WebUI module marketplace, or KernelSU ZIP-module installation contract. No new general plugin/configuration framework. No disk repartitioning, GPT repair, raw-device access-control firewall, or claim that an in-memory view confines a privileged process. The core's GPL-3.0 license remains unchanged.

## Delivery stages

1. **Source baseline (implemented):** isolate public identity and remove root/Manager product surfaces while preserving reusable kernel/userspace architecture and multi-architecture build logic.
2. **Managed boot (implemented in source):** consume the layout and manifest below, enforce generation matching, load modules in order, check readiness, persist failure receipts, detach early mounts, and then hand off to Android init.
3. **ROM projection (in progress):** implement the separate `gpt.ko` in-memory view and consume the selected `roms/<id>.toml`; only then enable managed-ROM boot on validated devices.
4. **Platform modules (implemented in source):** port the AIDL V1 boot HAL, install exact-generation ESP packages atomically on metadata (projected for managed boot), and run the narrowly scoped tiny-espsu bind/label helper on init after ueventd coldboot. Host/Android/kernel verification and device ordering proof are separate gates, not claimed by the implementation.

These are dependency stages, not claims of device compatibility. Cuttlefish and phone integration remain required before deployment.

## Build notes

Every relocatable espinit kernel module must carry a present but **empty** `__versions` section before the module checker runs; `kernel/Makefile` invokes `check_symbol <module.ko> <vmlinux>` from [`kernel/tools/check_symbol.c`](kernel/tools/check_symbol.c). Missing `__versions`, or a section with nonzero size, is a hard failure, while an empty section proceeds to the undefined-symbol validation against `vmlinux`. A build with MODVERSIONS disabled may not emit `__versions` at all, so create the empty section explicitly before running `check_symbol`, for example by adding it from an empty file:

```sh
: > /tmp/__versions.empty
llvm-objcopy --add-section __versions=/tmp/__versions.empty \
    --set-section-flags __versions=noload,readonly espinit.ko
```

`check_symbol` never adds, rewrites, or relaxes the section itself; a module lacking the empty section, or one carrying a populated `__versions`, is rejected rather than repaired.

The Cuttlefish integration lane lives in [`tools/cuttlefish/`](tools/cuttlefish/README.md): `assemble.py` packs `init_boot.img`, `esp.img` and `payload.json` for the harness `--espinit-payload` input, and `thin-activate.c` (built by `build-thin-activate.sh`) creates the thin `userdata_lp` device from the bootconfig tuple. The assembler re-signs the modified `init_boot` with the explicitly supplied key after proving that key verifies the pinned stock image. That lane is packaging and boot plumbing only; it proves no boot by itself.

## Exact ESP and runtime layout

The filesystem mounted as the ESP carries the espinit payload and is normally mounted read-only. Mounting it at `/debug_ramdisk/esp` yields the runtime root below; configuration paths are relative to `/espinit` on that filesystem, not to `/metadata` or the host checkout. The ESP's only write window is the bounded failure-receipt replacement described below; runtime staging writes the separately mounted metadata filesystem.

```text
ESP filesystem /                      # normally read-only
└── espinit/
    ├── manifest.toml
    ├── roms/
    │   └── android-a.toml            # selected by androidboot.espinit.rom
    ├── bin/
    │   ├── espinit
    │   ├── espinitd                  # install source, not the executed path
    │   └── busybox                   # static interpreter for ESP scripts
    ├── modules/                      # kernel payload plus explicit Android packages
    │   ├── espinit.ko
    │   ├── espinit/early.sh
    │   ├── espinit/recovery.sh
    │   ├── gpt.ko                    # required only for managed-ROM projection
    │   ├── gpt/early.sh
    │   ├── gpt/recovery.sh
    │   ├── boot-hal/module.toml       # executable + initrc, exact generation
    │   └── tiny-espsu/module.toml     # fixed helper + install script + policy mirror
    └── receipts/
        └── failure.json              # last failed early managed boot
```

The mutable state root `/metadata/espinit/` is separate from the ESP. After successful projection, PID1 privately mounts the projected metadata device, stages and fsyncs a complete runtime snapshot, atomically publishes it, and unmounts metadata before handoff. It is never an early failure-receipt dependency. Normal Android boot must mount that same metadata in first stage, before parsing init.rc. Recovery/fastbootd receives no injected platform RC and need not mount metadata in its first stage.

```text
/metadata/espinit/
├── espinitd                          # installed daemon executed by init.rc
├── bin/
│   ├── espinitd                      # helper link to the installed daemon
│   └── busybox, resetprop            # extracted userspace helpers
├── modules/                          # selected generation's explicit ESP packages
├── modules_update/                   # retained internal machinery, not a ZIP product
├── module_configs/                   # persistent internal configuration
├── .feature_config                   # feature toggles
├── log/
├── initrc/
│   └── modules.rc                    # generated init.rc fragment
└── receipts/
    └── <runtime receipts>            # planned: daemon receipts after handoff
```

Kernel `.ko` loading remains exclusively controlled by `manifest.modules` in document order. Android package selection is exclusively `manifest.platform.packages` (or `recovery_packages`), with each `modules/<id>/module.toml` describing that package. The installed files are not a second boot source: every boot replaces executable/package/RC state from the selected ESP generation. Existing `log`, `receipts`, `module_configs` and `.feature_config` data are retained without copying their file contents; obsolete executables and package trees are not retained. `initrc/modules.rc` is generated before handoff, not deferred until post-fs-data.

The boot-image integration must place the matching static binary at ramdisk `/espinit` and select it with `rdinit=/espinit`; the ESP payload is discovered and mounted by that binary before manifest processing. Integration must preserve the real-init handoff target independently of ESP configuration; a manifest may not choose an arbitrary init executable. No `current` symlink, generation fallback directory, or implicit module discovery is part of this contract. `espinitd` starts through Android init only after successful handoff; it cannot repair an unsuccessful early-boot check.

## Manifest

See [`espinit/manifest.example.toml`](espinit/manifest.example.toml). TOML is used directly; no templating or executable configuration.

| Field | Type and meaning |
| --- | --- |
| `schema_version` | Integer `1`; other versions fail validation. |
| `generation` | Nonempty release identifier, case-sensitive ASCII letters/digits plus `.`, `_`, `-`; maximum 63 bytes. It identifies one complete, coordinated payload, not a kernel version. |
| `rom` | Relative directory of per-ROM configurations, e.g. `roms`; PID1 reads only `<rom>/<androidboot.espinit.rom>.toml`. No global-file/default-ROM compatibility path. |
| `modules` | Nonempty array of tables, processed strictly in document order. |
| `modules[].name` | Unique logical module name, identical to the loaded module name without `.ko`. The first entry must be `espinit`. |
| `modules[].path` | Relative regular-file path of the module file, written in full and rooted at the ESP `/espinit` subtree (e.g. `modules/espinit.ko`); no absolute paths, empty components, `.`/`..`, or symlink traversal. |
| `modules[].params` | String of Linux module parameters, passed as module parameters, never evaluated by a shell. Empty string means no parameters. |
| `platform.metadata_filesystem` | Explicit `ext4` or `f2fs`, required for every payload; no probing/fallback filesystem. |
| `platform.packages` | Unique IDs of normal Android packages; managed normal boot requires `boot-hal` and `tiny-espsu`. These two are excluded in unmanaged mode. Other selected packages remain mandatory. Unrelated to kernel load order. |
| `platform.recovery_packages` | Optional list (default empty), staged only for recovery. The two normal HAL packages are forbidden here. |

All entries are required; there are no optional loads, discovery, retries with another generation, or sorting by filename. Unknown fields, duplicate entries/keys, missing fields, and incorrect types are errors. A managed ROM requires `gpt` after `espinit` and before real-init handoff; an unmanaged manifest that lists `gpt` is rejected, because there is no projection contract to apply. Other modules must obey the same generation and readiness requirements; dependencies must precede dependents.

## ROM configuration

See [`espinit/rom.example.toml`](espinit/rom.example.toml). The loader validates and consumes this file: it resolves each backend, applies the complete projection through the `gpt` APPLY ioctl, and requires an exact QUERY match before accepting `gpt` readiness. The file selects projected names and their existing whole block-device backends; it does not contain a new physical partition table. Backends are resolved only when the ordered payload reaches the `gpt` entry, after every earlier module and its scripts have run, so a logical volume, mapper device, loop or ESP file those entries publish can be a backend.

`androidboot.espinit.rom` must explicitly select an ASCII ID of 1..59 letters,
digits, `.`, `_`, or `-`, excluding `.` and `..`. The 59-byte bound keeps the
selected `<id>.toml` path component within the platform's 64-byte limit. PID1
accepts the bootconfig or
kernel-command-line spelling; duplicate keys or disagreement between the two
sources are fatal. The selected file's required `id` must match exactly. Multiple
`roms/<id>.toml` files can coexist; PID1 never consults a global source `rom.toml`.
The selected snapshot alone becomes `/metadata/espinit/rom.toml`, and the daemon
and helper check its ID against `ro.boot.espinit.rom` before the HAL can start.

| Field | Type and meaning |
| --- | --- |
| `schema_version` | Integer `1`. |
| `generation` | Same identifier as the manifest. |
| `id` | Required catalogue ID, exactly matching the selected boot ID and configuration filename. |
| `managed` | Boolean. `true` requires successful `gpt` projection before handoff; `false` requires an empty or absent `partitions` array and leaves the partition view unchanged. |
| `partitions` | Nonempty ordered array of tables when `managed = true`. |
| `partitions[].name` | Unique Android-facing projected partition name; ASCII letters/digits plus `_`, `-`, maximum 36 bytes (the `gpt` ABI label size), no path separators. |
| `partitions[].backend` | Backend in one of four documented forms, resolved only when the payload reaches the `gpt` entry: `/dev/block/by-name/<physical-name>` matched exactly against a unique sysfs `PARTNAME`; `/dev/mapper/<name>` matched exactly against a unique `/sys/class/block/dm-*/dm/name`; an existing `/dev/loopN`; or `esp-file:<relative-path>` for a preallocated regular file on the already-mounted read-only ESP, attached read-only through the standard loop-control/loop ioctls with zero offset and no size limit. A writable `esp-file:` backend is fatal, whole-LU devices are not accepted, and there is no offset, size-slicing, or extent/FIEMAP ABI. |
| `partitions[].read_only` | Boolean, explicitly selecting read-only (`true`) or writable (`false`) projected access. A physical partition with the same `PARTNAME` is hidden and forced read-only; unrelated physical partitions retain their native visibility and access mode. |

A projection spans exactly the entire backend block device; no resizing, implicit slot suffix, or offset arithmetic. Projected names must not collide with another projection. A projected name intentionally shadows every physical partition with that exact `PARTNAME`; the loader leaves all other stock partitions visible for normal platform operation. Backends must be distinct block devices, valid for the running device, and resolved without following the newly projected view. An ESP file backend is named as `esp-file:<relative-path>`, relative to the ESP mount root, and is attached by the loader itself with a fresh loop device, read-only, zero offset and no size limit, while the ESP remains read-only; a writable ESP or a writable ESP-file projection is fatal. The loop and backing-file guards are kept open until the projection has been applied, are `O_CLOEXEC`, and the loop is autoclear, so the attachment survives the APPLY close but is not inherited into Android.

## LVM activation

Managed phone payloads place `thin` before `gpt` and ship
`modules/thin/early.sh`, which executes the argument-free `thin-activate`
binary. The activator finds exactly one physical GPT partition whose sysfs
`PARTNAME` is `userdata`, creates one private node from its kernel-reported
major/minor pair, and reads the LVM2 label and committed text metadata
read-only. It requires the PV byte size and UUID to match a single-PV VG named
`rom`, with thin `metadata_1` and `userdata_1` LVs.

The bounded `lvm2-meta` parser verifies the label, metadata-area and text
checksums and rejects conflicting copies, unsupported segments and exceeded
limits. `thin-activate` recursively activates the linear metadata/data LVs,
the `-tpool` layer, and every visible non-skipped LV through the
device-mapper ioctl ABI. Tables come only from the checked on-disk metadata.
An existing name is accepted only when every active target exactly matches;
devices created by a failed invocation are removed in reverse order. The
compiled generation must exactly match the `ESPINIT_GENERATION` supplied by
PID 1. The tool does not invoke a shell or `lvm`, mutate LVM metadata, create
thin IDs, accept arguments, or guess another PV/VG.

## Generation matching and module self-check

The manifest, ROM configuration, PID-1 binary, daemon, core module, and every listed ESP module must carry the **same generation**. Each executable/module carries a build-time generation; `ESPINIT_GENERATION` selects it explicitly, otherwise builds derive the full 40-character lowercase Git HEAD hash. A filename or successful `finit_module` alone is not proof of compatibility. Linux module architecture/vermagic checks still apply. Generation equality is a consistency check, not a signature or authenticity guarantee; trusted boot must protect the payload separately.

Before loading dependent modules, PID 1 queries the espinit-specific UAPI v2 control ioctl and verifies core identity, ABI compatibility, exact generation, and completed initialization. A preloaded core is acceptable only if it passes the same checks; the presence of KernelSU is not success. Each subsequent module must expose matching `generation` and `ready` parameters before the next entry proceeds. For `gpt`, generation is checked before APPLY can publish or hide anything; readiness is checked afterwards in the projection failure stage and diagnostics include the validated requested partition/mode counts.

After core validation PID1 writes its stable boot classification to the core's
PID1-only, write-once `/sys/module/espinit/parameters/platform_boot_mode` parameter
(`1` Android, `2` recovery/fastbootd), then requires matching readback. Unset or
recovery mode selects zero built-in/custom init RC bytes. This is a fixed boot
handshake, not an app-facing control or a property-based fallback.

## Boot ordering and hard-failure receipt

1. Prepare the minimum early mounts/logging and opportunistically retain an already-enumerated payload ESP read-only so vendor-module preload failures can still leave a receipt. Preload the applicable vendor modules with their dependencies/options, then wait up to ten seconds for storage enumeration when the ESP was not available before preload. Enumerate every GPT ESP candidate, probe each read-only, and require exactly one to contain a regular `/espinit/manifest.toml`; other firmware ESPs are allowed. Keep the selected payload ESP mounted read-only and validate its manifest plus explicitly selected per-ROM TOML without changing the partition view. No step here requires projected `/metadata`.
2. Check payload generations, module ordering, backend configuration, and the ESP receipt directory `/espinit/receipts` (runtime `/debug_ramdisk/esp/espinit/receipts`) structurally without opening a write window. For managed boot, unavailable receipt storage is itself a hard failure; do not mount or depend on `/metadata` for this check.
3. Load or validate `espinit.ko`, then load the remaining modules in manifest order, perform each self-check, and run each module's optional `early.sh` or `recovery.sh` through the ESP busybox with a 35-second deadline. Immediately before the `gpt` entry, and only after every earlier module and script has run, resolve each backend — by-name partition, exact `/dev/mapper/<name>`, existing `/dev/loopN`, or a read-only `esp-file:` attached to a fresh loop device on the already-mounted read-only ESP. Load `gpt`, verify its generation, enumerate physical `DEVTYPE=partition` device numbers other than the mounted ESP into `hide[]`, then issue one atomic APPLY. The ESP stays outside `hide[]` so any later hard failure can remount it for its receipt. Exact QUERY (ABI, active view, count) and readiness checks complete before the `gpt` stage script.
4. After kernel-stage scripts and projection, validate selected packages/source inodes and executable generation notes. Privately mount writable metadata; install the daemon, selected ROM and package set; fsync files/directories; publish with rename/exchange; retire the old snapshot under a distinct cleanup name before parent fsync/removal; then unmount. Managed boot uses projected metadata and requires writable projected bdsvars/misc for the normal HAL. Unmanaged boot resolves exactly the native metadata PARTNAME with the same bounded 10-second/100-ms enumeration retry, without projection or the HAL pair. Permanent resolution errors fail immediately; unavailable/unwritable metadata stops handoff.
5. Detach the ESP and owned early mounts, then replace PID1 with fixed `/init`. Core installs HAL rules at `/system/bin/init second_stage`. In Android mode only it injects synchronous `on init` `exec_start` for `espinitd`'s `Stage::Early`: after ueventd coldboot, before late-fs/class early_hal. The daemon validates installed generation/ID/managed mode, extracts its existing interpreter and finishes tiny-espsu. Failure uses `reboot_on_failure`, not a warning. Unmanaged mode skips the helper. Recovery/fastbootd receives neither this service nor custom module RC; its existing ESP recovery scripts/projection remain.

A selected managed ROM has **no stock-ROM fallback**. Any parse, generation, load, self-check, backend, projection, or handoff failure must stop normal Android handoff. Invalid/unreadable configuration must not be interpreted as `managed = false`; only an explicitly valid unmanaged configuration permits an unchanged partition view. Do not silently skip a module or leave a partially projected boot running.

Before entering the platform's fatal-boot stop path, persist ESP `/espinit/receipts/failure.json` (runtime `/debug_ramdisk/esp/espinit/receipts/failure.json`) as a UTF-8 JSON object with these required fields:

- `schema_version`: integer `1`;
- `generation`: selected generation string, or JSON `null` if it could not be validated;
- `stage`: one of `configuration`, `generation`, `storage`, `module-load`, `module-check`, `projection`, `handoff`;
- `component`: failing file/module/projected name, or `null` if not attributable;
- `error`: stable machine-readable error identifier;
- `detail`: bounded diagnostic string, with no credentials or sensitive contents.

Keep the ESP read-only during normal boot. On failure only, use one bounded read-write remount window to replace the receipt: write a temporary file in the same directory, `fsync` the file, atomically rename it to `failure.json`, and `fsync` the directory; then sync the ESP and remount it read-only before the fatal-boot stop. Keep the previous receipt until replacement succeeds; a successful boot does not erase failure evidence. Do not require wall-clock time or a custom receipt database. If the remount, receipt write/sync, or read-only restoration fails, emit the original failure and receipt-storage failure to the early kernel log, still attempt sync and read-only restoration when the writable window was opened, and remain in the fatal-boot stop path without unbounded retries. Logging is not a durable-receipt substitute and never authorizes handoff. The platform stop mechanism must be selected during device integration; it must not restart into Android with the same failed projection unchecked.

## `gpt.ko` limits

`gpt.ko` presents an **in-memory virtual partition view**. Projected names map to selected whole block-device backends, including the standard loop devices the loader attaches for `esp-file:` backends immediately before APPLY; `gpt.ko` itself does not accept regular files or a custom extent/FIEMAP interface. The loader adds only physical partitions whose exact `PARTNAME` collides with a projected name to `hide[]` and requires an exact QUERY match after APPLY. At the normal Android partition surface (partition discovery, device nodes, and by-name aliases), those shadowed physical names are cleared and their endpoints are marked read-only. Unrelated stock physical partitions remain visible with their native access mode. The ESP remains a physical exception solely to preserve the bounded failure-receipt remount; platform permissions must protect it. Projected endpoints use the configuration's explicit read-only or writable policy.

The module never writes disk GPT headers, entries, CRCs, or partition metadata and never changes physical partition boundaries. Writes through a writable projected partition are ordinary writes to its backend contents, not GPT updates. There is **no raw-LU bio firewall**: direct access to a whole UFS logical unit or equivalent raw block device is not filtered by this module. A sufficiently privileged process can bypass the normal partition surface. This is a boot-time naming/projection facility, not a data-loss prevention or hostile-root isolation boundary.

## Platform package contract

Each selected `/espinit/modules/<id>/module.toml` is strict TOML with required
`schema_version = 1`, `generation`, `id` and nonempty `[[files]]`. Each file has
only `source`, `destination`, `mode` and `kind`. Paths are normal relative paths
inside that package, ASCII letters/digits plus `._-/`, with no empty, dot,
dot-dot, absolute, symlink or non-regular component. IDs are 1-64 bytes;
generations follow the existing 1-63-byte manifest rule. Unknown/duplicate
fields, duplicate IDs/destinations, file/directory collisions, missing or empty
files and mismatched generations are fatal. Destinations `module.toml`,
`disable`, `remove` and `update` are reserved. Modes are exactly `"0755"` for
`binary`/`script` and `"0644"` for `data`/`policy`/`initrc`; no setuid bits or
writable executable modes are accepted. Init RC destinations must be directly
under `initrc/` and end in `.rc`.

`binary` entries and `bin/espinitd` must contain exactly one retained ELF64
`.note.espinit` generation note matching the manifest; PID1 checks this without
executing Android binaries. Files and RC fragments install in sorted destination
order, independently of package/file input order. Manifests and the selected ROM
config are copied into the same transaction. Publication uses a sibling
`.espinit-staging` tree and `renameat2` NOREPLACE/EXCHANGE, never an in-place
multi-file update or symlink selector. A leftover staging tree remains fatal,
even if it looks complete. Each published snapshot has a fsynced
`.espinit-complete` commit marker (not a generation/config manifest). After an
exchange, the old tree is renamed `.espinit-retired` **before** parent fsync and
deletion. On restart only this cleanup namespace is removed, after verifying a
complete live snapshot and the retired marker. Cleanup retains the marker until
all other entries are removed; an empty retired directory is also safe to finish.
Unknown nonempty retired trees, symlink roots and unmarked existing live snapshots
stop handoff for offline inspection; no partial staging tree is resumed.

The HAL consumes only `ro.boot.espinit.rom` (from `androidboot.espinit.rom`) and
`ro.boot.slot_suffix`. The ROM ID must name provisioned `Slot-<id>` and
`MergeStatus-<id>` records; there is no default ROM/slot or automatic formatting.
The service remains AIDL V1 `android.hardware.boot.IBootControl/default`, hash
`2400346954240a5de495a1debc81429dd012d7b7`, in `hal_bootctl_default`.
Its record layouts, phased/fsynced bdsvars append and 64-byte misc VAB mirror
are documented in [the port](payloads/boot-hal/README.md).

tiny-espsu accepts **no arguments** and only labels the held HAL source inode
and actual projected bdsvars block inode, then binds the HAL over the fixed
vendor executable. It refuses non-root, wrong namespace, missing properties,
unmatched generation, non-projected bdsvars or symlinked package paths. It has
no shell/su/manager/profile/app API or arbitrary policy/execute/bind interface.
Core's built-in policy creates only `gblbds_hal_exec` and
`gblbds_bdsvars_block_device` without broad attributes, the init/bootctl file
allows, bootctl block permissions and init type transition. Integration adds
`blk_file lock` for preserved flock transactions, `filesystem associate` for the
HAL inode on labeledfs and bdsvars on tmpfs, and the narrowly targeted
`init -> hal_bootctl_default:process2 nosuid_transition` required by a bind source
on nosuid metadata. Packaged `policy.cil` is only a mirror, never the required path.

Recovery still runs its existing projection and recovery scripts. It publishes
a recovery snapshot without the normal HAL/helper/RC, so a previous normal
generation cannot leak into recovery. Only explicitly required recovery
packages can fail recovery package staging. No native writer fallback is
invoked. Device-specific stock QTI/HIDL activation suppression and recovery
AIDL/HIDL parity remain unproven; do not claim recovery OTA support.

Every valid payload, including unmanaged and recovery boots, requires the
exact-generation daemon and explicit metadata filesystem for PID1 staging.
There is no missing-daemon bypass in normal boot. Recovery's absence of injected
RC is selected by the validated PID1/core handshake, not guessed from properties
or metadata availability. Its Android first stage need not mount metadata.
The installed `rom.toml` remains the runtime generation/ID/managed authority;
there is no parallel runtime configuration or generation/mode marker file.

### AArch64 build/package path

Use one `ESPINIT_GENERATION` for PID1, core/thin/gpt, `thin-activate`,
espinitd and both platform binaries. With NDK API 35 and the Rust Android
target installed:

```sh
export ESPINIT_GENERATION=release-1
export ESPINIT_NDK=/path/to/android-ndk-r29
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$ESPINIT_NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android35-clang"
cargo +nightly-2026-08-08 build --locked --release --target aarch64-linux-android \
  -p thin-activate -p espinitd -p espinit-platform \
  --bin thin-activate --bin espinitd --bin tiny-espsu
bash payloads/boot-hal/build-android.sh
```

Copy `thin-activate` to `bin/thin-activate`, `espinitd` to `bin/espinitd`,
and both platform outputs to the source paths declared by
[`espinit/modules/boot-hal/module.toml`](espinit/modules/boot-hal/module.toml) and
[`espinit/modules/tiny-espsu/module.toml`](espinit/modules/tiny-espsu/module.toml);
stamp both package manifests with that exact generation. Ship the checked-in
thin early script, RC, install script and policy mirror alongside them.
`tools/cuttlefish/assemble.py` performs this layout/stamping
from explicit binary inputs and validates their notes. Its ROM placeholder
must still be replaced with real projected backends; it never invents metadata,
bdsvars or misc storage.

Parent verification gates:
`cargo test --locked -p lvm2-meta -p thin-activate -p espinit-platform -p espinit`,
`cargo test --locked --manifest-path payloads/boot-hal/Cargo.toml`,
`cargo test --locked --manifest-path payloads/boot-hal/Cargo.toml -p varstore`,
`python3 -m unittest tools.cuttlefish.test_assemble`, the Android builds and
the exact-kernel module build, plus host compilation/execution of
`kernel/tests/platform_boot_test.c`. A guest/device run must separately prove
normal first-stage metadata mount, enforcing nosuid transitions/label association,
synchronous on-init completion after coldboot and before early_hal, and recovery
without a first-stage metadata dependency. Source/host tests imply no such runtime evidence.

## License and provenance

This fork derives from KernelSU by [tiann](https://github.com/tiann) and its contributors. The retained Git history and original source copyright notices record upstream authorship; espinit renaming does not replace that attribution. Historical upstream documentation, where retained, describes KernelSU and is not the espinit contract.

The core remains licensed under **GNU GPL version 3**; [`LICENSE`](LICENSE) preserves the upstream GPL-3.0 text verbatim. `modules/thin` and `modules/gpt` are separate **GPL-2.0-only** modules aggregated with, not linked into or relicensed as, the GPL-3.0 core. Their subtree licenses and provenance files identify their origins and boundaries. See [`SECURITY.md`](SECURITY.md) for security scope and reporting guidance.

`payloads/boot-hal`, its vendored `varstore`, and `userspace/lvm2-meta`
retain their **Apache-2.0** licenses and upstream provenance. The LVM parser was
ported from the gbl-bds-rs host proof into the public runtime without changing
its format limits or table derivation. `thin-activate`, the platform
installer, and tiny-espsu are GPL-3.0-only; their shared ELF generation-note
source is Apache-2.0 so the HAL does not link GPL userspace code.
