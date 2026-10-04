# espinit

espinit is an early-boot substrate forked from [KernelSU](https://github.com/tiann/KernelSU), not an Android root product. This repository is a conservative source baseline: it retains KernelSU's kernel-module lifecycle, hook design, Rust PID-1/daemon split, and aarch64, x86_64, and riscv64 build shape where applicable. App-facing root grants, the Manager APK, and Manager-dependent packaging are outside the product.

**Status:** the ESP layout, TOML configuration, generation checks, partition projection, and failure receipts below are a **planned implementation contract**, not working features or a supported installation procedure. Source adaptation alone does not establish boot compatibility or security. The examples are configuration specifications, not files the current baseline necessarily consumes.

## Architecture and identity

- `espinit` is the early PID-1 binary; it prepares the managed boot and eventually transfers control to Android's real init without changing PID.
- `espinit.ko` is the core kernel module. Retaining the inherited lifecycle and existing direct syscall-table hook facilities is intentional reuse of the boot substrate, not KernelSU Manager or root-product compatibility; do not introduce a second hook framework.
- `espinitd` is the Android-side daemon, not a `su` replacement or an application privilege broker. It is installed under the mutable state root and executed by Android init as `/metadata/espinit/espinitd`; the ESP holds only its read-only payload copy.
- `/debug_ramdisk/esp/espinit` is the runtime ESP payload root; `/metadata/espinit` is the persistent Android working root after successful handoff. Early failure receipts use the ESP, not projected metadata. Do not use KernelSU's `/data/adb/ksu` state.
- The SELinux domain/type are `espinit`/`espinit_file` (`u:r:espinit:s0`, `u:object_r:espinit_file:s0`); `espinitd` keeps `espinit_file` on its own binary and `system_file` on Android module files. Public module, ioctl/install magic, anonymous-inode, and socket identities must be distinct from KernelSU: the anonymous inodes are `[espinit]` (driver) and `[espinit_fdwrapper]` (file wrapper), and the info surface reports only the LKM flag, so no manager/late-load/bundled/client variants exist. Private `ksu_` implementation prefixes may remain; they do not establish interoperability.
- `gpt.ko` is a later, separate ESP module, never another name for the core module.

A real KernelSU installation must not be detected as an espinit module, satisfy an espinit self-check, or share espinit state/control endpoints. Distinct identity is necessary, but simultaneous hook ownership still requires integration testing; coexistence is not promised by this baseline.

### Non-goals

No Manager APK, app-root API, `su` compatibility, allowlist/profile product, WebUI module marketplace, or KernelSU ZIP-module installation contract. No new general plugin/configuration framework. No disk repartitioning, GPT repair, raw-device access-control firewall, or claim that an in-memory view confines a privileged process. The core's GPL-3.0 license remains unchanged.

## Delivery stages

1. **Source baseline (current work):** isolate public identity and remove root/Manager product surfaces while preserving reusable kernel/userspace architecture and multi-architecture build logic.
2. **Managed boot (planned):** consume the layout and manifest below, enforce generation matching, load modules in order, check readiness, and persist failure receipts before Android handoff.
3. **ROM projection (planned):** implement the separate `gpt.ko` in-memory view and consume `rom.toml`; only then enable managed-ROM boot on validated devices.

These are dependency stages, not claims that stage 2 or 3 already exists. Device firmware integration and testing are required before deployment.

## Build notes

Every relocatable espinit kernel module must carry a present but **empty** `__versions` section before the module checker runs; `kernel/Makefile` invokes `check_symbol <module.ko> <vmlinux>` from [`kernel/tools/check_symbol.c`](kernel/tools/check_symbol.c). Missing `__versions`, or a section with nonzero size, is a hard failure, while an empty section proceeds to the undefined-symbol validation against `vmlinux`. A build with MODVERSIONS disabled may not emit `__versions` at all, so create the empty section explicitly before running `check_symbol`, for example by adding it from an empty file:

```sh
: > /tmp/__versions.empty
llvm-objcopy --add-section __versions=/tmp/__versions.empty \
    --set-section-flags __versions=noload,readonly espinit.ko
```

`check_symbol` never adds, rewrites, or relaxes the section itself; a module lacking the empty section, or one carrying a populated `__versions`, is rejected rather than repaired.

## Exact ESP and runtime layout

The filesystem mounted as the ESP carries the espinit payload and is normally mounted read-only. Mounting it at `/debug_ramdisk/esp` yields the runtime root below; configuration paths are relative to `/espinit` on that filesystem, not to `/metadata` or the host checkout. The only early-boot write window is the bounded failure-receipt replacement described below.

```text
ESP filesystem /                      # normally read-only
└── espinit/
    ├── manifest.toml
    ├── rom.toml
    ├── bin/
    │   ├── espinit
    │   └── espinitd                  # install source, not the executed path
    ├── modules/                      # kernel modules loaded by PID 1
    │   ├── espinit.ko
    │   └── gpt.ko                    # required only for managed-ROM projection
    └── receipts/
        └── failure.json              # planned: last failed early managed boot
```

The mutable state root `/metadata/espinit/` is separate from the ESP and holds daemon runtime state after successful handoff, once metadata is available. It is not an early-boot receipt dependency. Names already used by the baseline userspace/kernel code are listed here; anything marked planned is not yet implemented.

```text
/metadata/espinit/
├── espinitd                          # installed daemon executed by init.rc
├── bin/
│   ├── espinitd                      # helper link to the installed daemon
│   └── busybox, resetprop            # extracted userspace helpers
├── modules/                          # Android module store (ZIP modules)
├── modules_update/                   # pending module updates
├── metamodule -> modules/<id>        # single active metamodule
├── module_configs/                   # per-module configuration (planned use)
├── .feature_config                   # feature toggles
├── log/
├── initrc/
│   └── modules.rc                    # generated init.rc fragment
└── receipts/
    └── <runtime receipts>            # planned: daemon receipts after handoff
```

The two `modules/` directories are unrelated and never interchangeable: the ESP `modules/` holds the tiny kernel-module payload that PID 1 loads, while `/metadata/espinit/modules/` is the Android ZIP-module store managed by `espinitd` after boot. `initrc/modules.rc` is the generated boot fragment consumed by Android init; every path under `/metadata/espinit/` is a state path, never a boot-critical source for early PID 1.

The boot-image integration must arrange execution of the matching ESP `bin/espinit` as early PID 1 and make the ESP subtree available before manifest processing. It must preserve the real-init handoff target independently of ESP configuration; a manifest may not choose an arbitrary init executable. No `current` symlink, generation fallback directory, or implicit module discovery is part of this contract. `espinitd` starts through Android init only after successful handoff; it cannot repair an unsuccessful early-boot check.

## Manifest (planned)

See [`espinit/manifest.example.toml`](espinit/manifest.example.toml). TOML is used directly; no templating or executable configuration.

| Field | Type and meaning |
| --- | --- |
| `schema_version` | Integer `1`; other versions fail validation. |
| `generation` | Nonempty release identifier, case-sensitive ASCII letters/digits plus `.`, `_`, `-`; maximum 64 bytes. It identifies one complete, coordinated payload, not a kernel version. |
| `rom` | Relative path to the ROM configuration; this layout requires `rom.toml`. |
| `modules` | Nonempty array of tables, processed strictly in document order. |
| `modules[].name` | Unique logical module name, identical to the loaded module name without `.ko`. The first entry must be `espinit`. |
| `modules[].path` | Relative regular-file path of the module file, written in full and rooted at the ESP `/espinit` subtree (e.g. `modules/espinit.ko`); no absolute paths, empty components, `.`/`..`, or symlink traversal. |
| `modules[].params` | String of Linux module parameters, passed as module parameters, never evaluated by a shell. Empty string means no parameters. |

All entries are required; there are no optional loads, discovery, retries with another generation, or sorting by filename. Unknown fields, duplicate entries/keys, missing fields, and incorrect types are errors. A managed ROM requires `gpt` after `espinit` and before real-init handoff. Other modules must obey the same generation and readiness requirements; dependencies must precede dependents.

## ROM configuration (planned)

See [`espinit/rom.example.toml`](espinit/rom.example.toml). This file selects projected names and their existing whole block-device backends; it does not contain a new physical partition table.

| Field | Type and meaning |
| --- | --- |
| `schema_version` | Integer `1`. |
| `generation` | Same identifier as the manifest. |
| `managed` | Boolean. `true` requires successful `gpt` projection before handoff; `false` requires an empty or absent `partitions` array and leaves the partition view unchanged. |
| `partitions` | Nonempty ordered array of tables when `managed = true`. |
| `partitions[].name` | Unique Android-facing projected partition name; ASCII letters/digits plus `_`, `-`, maximum 64 bytes, no path separators. |
| `partitions[].backend` | Absolute block-device path resolving before projection, such as `/dev/block/by-name/<physical-name>` or a prepared `/dev/loopN`; resolve and retain it before hiding physical aliases. Schema v1 accepts only whole block-device backends: no whole-LU device, regular file, offset, or arbitrary executable backend. |
| `partitions[].read_only` | Boolean, explicitly selecting read-only (`true`) or writable (`false`) projected access. Physical residual exposure remains read-only regardless. |

A projection spans exactly the entire backend block device; no resizing, implicit slot suffix, or offset arithmetic. Names must not collide with retained physical names or another projection. Backends must be distinct block devices, valid for the running device, and resolved without following the newly projected view. A preallocated ESP regular file must first be attached using a standard Linux loop device, with no offset or size slicing, before `gpt` APPLY; only the resulting block-device path is supplied as `backend`. The loop attachment must remain alive for the projection's lifetime. Schema v1 adds no file-extent or FIEMAP ABI to `gpt.ko`. Missing/ambiguous backends, repeated backends, invalid names, unknown fields, and unsupported schemas are validation failures. Document order defines publication order, not priority or fallback.

## Generation matching and module self-check (planned)

The manifest, ROM configuration, PID-1 binary, daemon, core module, and every listed ESP module must carry the **same generation**. Each executable/module carries a build-time generation; a filename or successful `finit_module` alone is not proof of compatibility. Linux module architecture/vermagic checks still apply. Generation equality is a consistency check, not a signature or authenticity guarantee; trusted boot must protect the payload separately.

Before loading dependent modules, PID 1 must query the espinit-specific kernel control interface and verify core identity, ABI compatibility, generation, and completed initialization. A preloaded core is acceptable only if it passes the same checks; the presence of KernelSU is not success. Each subsequent module must expose a successful self-check including its identity, generation, and readiness before the next entry proceeds. `gpt` readiness additionally means every requested backend was validated and the complete projected view is active; partial publication is failure. The exact wire encoding/control commands are future implementation work, but these checks and their ordering are mandatory and may not be replaced with a log-string or load-success heuristic.

## Boot ordering and hard-failure receipt (planned)

1. Prepare the minimum early mounts/logging, locate and normally mount the ESP read-only, and validate both TOML files without changing the partition view. No step here requires projected `/metadata`.
2. Check payload generations, module ordering, backend configuration, and the ESP receipt location `/espinit/receipts` (runtime `/debug_ramdisk/esp/espinit/receipts`) and failure-only remount capability. For managed boot, unavailable receipt storage is itself a hard failure; do not mount or depend on `/metadata` for this check.
3. Load or validate `espinit.ko`, then load the remaining modules in manifest order and perform each self-check. Before `gpt` APPLY, attach any preallocated ESP file backends using standard loop devices and resolve their block-device paths. Configure and activate the complete `gpt` projection and establish readiness before handoff for a managed ROM.
4. Only after all required checks succeed, transfer control to real Android init. Later Android startup may make `/metadata/espinit` available and launch the matching `espinitd` for runtime receipts/logs.

A selected managed ROM has **no stock-ROM fallback**. Any parse, generation, load, self-check, backend, projection, or handoff failure must stop normal Android handoff. Invalid/unreadable configuration must not be interpreted as `managed = false`; only an explicitly valid unmanaged configuration permits an unchanged partition view. Do not silently skip a module or leave a partially projected boot running.

Before entering the platform's fatal-boot stop path, persist ESP `/espinit/receipts/failure.json` (runtime `/debug_ramdisk/esp/espinit/receipts/failure.json`) as a UTF-8 JSON object with these required fields:

- `schema_version`: integer `1`;
- `generation`: selected generation string, or JSON `null` if it could not be validated;
- `stage`: one of `configuration`, `generation`, `storage`, `module-load`, `module-check`, `projection`, `handoff`;
- `component`: failing file/module/projected name, or `null` if not attributable;
- `error`: stable machine-readable error identifier;
- `detail`: bounded diagnostic string, with no credentials or sensitive contents.

Keep the ESP read-only during normal boot. On failure only, use one bounded read-write remount window to replace the receipt: write a temporary file in the same directory, `fsync` the file, atomically rename it to `failure.json`, and `fsync` the directory; then sync the ESP and remount it read-only before the fatal-boot stop. Keep the previous receipt until replacement succeeds; a successful boot does not erase failure evidence. Do not require wall-clock time or a custom receipt database. If the remount, receipt write/sync, or read-only restoration fails, emit the original failure and receipt-storage failure to the early kernel log, still attempt sync and read-only restoration when the writable window was opened, and remain in the fatal-boot stop path without unbounded retries. Logging is not a durable-receipt substitute and never authorizes handoff. The platform stop mechanism must be selected during device integration; it must not restart into Android with the same failed projection unchecked.

## `gpt.ko` limits (planned)

`gpt.ko` presents an **in-memory virtual partition view**. Projected names map to selected whole block-device backends, including standard loop devices prepared before APPLY for preallocated ESP files; `gpt.ko` itself does not accept regular files or a custom extent/FIEMAP interface in schema v1. At the normal Android partition surface (partition discovery, device nodes, and by-name aliases), original physical partition names are hidden; any physical partition exposure that cannot be hidden must be read-only. This rule applies to the physical partition endpoints themselves, not merely an alias permission. Projected endpoints obey their explicit `read_only` setting. A platform unable to enforce this surface contract must fail managed boot rather than advertise a compliant view.

The module never writes disk GPT headers, entries, CRCs, or partition metadata and never changes physical partition boundaries. Writes through a writable projected partition are ordinary writes to its backend contents, not GPT updates. There is **no raw-LU bio firewall**: direct access to a whole UFS logical unit or equivalent raw block device is not filtered by this module. A sufficiently privileged process can bypass the normal partition surface. This is a boot-time naming/projection facility, not a data-loss prevention or hostile-root isolation boundary.

## License and provenance

This fork derives from KernelSU by [tiann](https://github.com/tiann) and its contributors. The retained Git history and original source copyright notices record upstream authorship; espinit renaming does not replace that attribution. Historical upstream documentation, where retained, describes KernelSU and is not the espinit contract.

The core remains licensed under **GNU GPL version 3**; [`LICENSE`](LICENSE) preserves the upstream GPL-3.0 text verbatim. Future `thin.ko` is planned as a separate **GPL-2.0-only** module aggregated with the GPL-3.0 core, not linked into or relicensed as that core. Per-subtree license notices are required to identify each component's license and provenance; the top-level GPL-3.0 text must not be presented as relicensing the separate module. This is a planned packaging boundary, not an implemented module or a compatibility guarantee. See [`SECURITY.md`](SECURITY.md) for security scope and reporting guidance.
