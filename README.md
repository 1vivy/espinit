# Egysk

Egysk is the current core product: a separated Magisk-derived native userspace,
an rdinit loader, and a small kernel helper for multi-OS Android. This README
owns the remaining integration checklist. Core and platform source/producer
cutover, repository naming and complete release packaging are implemented.
Whole-device validation is not complete. Source builds alone do not establish
Android consumer behavior; existing real-device userspace evidence and source/RE
remain valid inputs.

## Ownership and identity

- This repository owns pinned Magisk, its ordered product patches, `egyskinit`,
  `egysk-runtime`, the standalone `egysk-build-cpio` host builder, and `egysk.ko`.
- `egysk-modules` owns platform modules, `gobbl-runtime`, Android CLIs,
  ROM/storage/OTA/HAL handling, and product packaging.
- `egysk-lkms` owns independent thin, partition-projection, efivarfs and
  efivar_store LKMs. They have no dependency on a root-provider credential ABI.

The Android native daemon is `egyskd`; its domain is `egysk`, with `egysk_file`
and `egysk_log_file` types. Product sockets, services, paths and installer state are
separate from other root installations. Traditional app SU is neither the goal
nor something to strip indiscriminately: retained upstream authorization stays
intact, without claiming the system's ordinary `su` entrypoint. There is no
Manager deliverable, provider negotiation, SU forwarding or alternate backend.

The kernel helper's internal name is `egysk`. Its uapi ABI 4 supplies
root-only policy application, coherent live-policy export and the one-pass init-rc
supplement, plus retained module symbol machinery. It does not grant credentials or expose app allowlists,
profiles or SU compatibility. Second-stage exec observation initializes the
bounded base policy before `egysk`-labelled init commands run. The independent
EFI backend brackets device I/O with its backing file's opener credentials.

The core checkout is `../egysk`, with public origin
`https://github.com/1vivy/egysk.git`. The private split origins are
`1vivy/egysk-modules` and `1vivy/egysk-lkms`; their local checkouts have matching
names and recovery remotes point to this core checkout. The split histories
were recovered from main revisions `e985dfac4fea73751f5ef3865cdcc4122c8a4eb3`
(`retired/kernelsu-esp-modules`) and `75af27f5b2d79c2f11721069f8d1f739c4ac9f36`
(`retired/kernelsu-esp-lkms`). Upstream KernelSU history, licenses and the pinned
Magisk submodule are preserved. Hosted names/privacy and the local move are
verified. Both private histories and the public core/gobbl integration commits
have been pushed with owner authorization, including their existing ancestry.
Fresh mirrors of all four published main branches matched local commits,
reachable object sets and complete trees; restored object integrity checks passed.

## Boot and storage contract

The generic archive entry is `/egyskinit` at the archive root; its consumer
selects it with `rdinit=/egyskinit`. It executes the original `/init` without
replacement, preserving PID, remaining arguments and environment. Critical LKMs
load before ESP discovery. Gobbl-generated launchers select
`rdinit=/egyskinit`; the independent platform archive names `esu.cpio` and
`esu.stage.cpio` remain unchanged.

```text
/dev/esp                         one raw RW ESP, nosuid,nodev,noexec
/dev/esp/egysk/                  product config, bin, modules and exact KMI sets
/dev/esp/esu/                    independent downstream schema-2 ROM configuration
/dev/egysk/bin/                  executable tmpfs tools
/dev/egysk/store/                shared physical-metadata product subtree
/dev/egysk/modules/<id>/         writable effective module views
/dev/egysk/state/<id>/           durable shared module state
/sys/firmware/efi/efivars/        efivarfs, when supplied by the platform
```

The backing descriptor is opaque to the core: block sources select exactly one
absolute source or sysfs PARTNAME; filesystem sources select an absolute path.
The supplied helper exposes the existing Unix-capable backing filesystem.
There is no module-storage LV, filesystem formatting, per-ROM module profile,
capacity policy or ephemeral fallback. Keys and Android password-slot state
remain outside owned module upper/work trees.

Each ESP module gets an immutable Unix lower snapshot plus an OverlayFS upper/work
pair per package generation on metadata. VFAT dentries cannot be OverlayFS lowers;
the runtime copies each new generation once, verifies its identity, and publishes
the snapshot only after syncing it. Interrupted copies are never exposed. Changed
packages select fresh triples; prior edits remain recoverable rather than being
merged into the new generation. Local installs, preferences and durable state share
the metadata store. Activation journals recover interrupted package renames before
views are mounted. Module ZIP operations use these product roots.

The same admitted module set supplies scripts, rc, policy, kmods and projection.
Inner phases are outermost, module order innermost, with a 35-second foreground
process-group bound. Scripts receive `EGYSK_BACKING_ROOT` for the selected physical
backing, independent of Android's projected `/metadata`. Services remain disabled
until their owning stage succeeds. Early Android projection uses Magisk's built-in
magic mount, without running post-fs-data scripts early.

Installed directories with matching `module.prop` IDs are inventory;
`modules_order` ranks them, appending unlisted IDs in stable order. Flags
`critical`, `recovery_ok`, `disable`, `remove`, `skip_mount` accept `1`/`true`;
other values are false. Optional failures remove admission before rc assembly;
critical failures stop managed boot. Keep recovery/charger/safe-mode and
`rd/`, `rd_copy`, `early_bin` collision semantics. Scripts receive `EGYSK`,
`EGYSK_MODE`, `EGYSK_STAGE`, `EGYSK_PHASE`, `EGYSK_MODULE`, `MODDIR`,
`EGYSK_MODULE_STATE`, `EGYSK_BACKING_ROOT` and staged PATH.

The helper rc supplement remains root-only, pointer/length/reserved-validated,
bounded to 64 KiB and supplied once (`EALREADY` on repeat, `EBUSY` after
consumption). Its immutable bytes precede the original init.rc bytes in the same
parse, so product barriers register before stock actions for each event. Base
policy precedes product init commands; admitted module rules apply once before
their services. Dispatch logical `early-init`, `init`, `early-fs`, `post-fs`,
`post-fs-data`, `post-mount`, `service`, `boot-completed` in order. Reconstruction
and logical `early-init` run at the **front of Android's `init` event**, followed
by logical `init`, not before stock `early-init`: the latter must start ueventd,
set up bootstrap APEXes/linker configuration, and finish before init's built-in
coldboot wait. System/vendor mounts and SELinux policy come from first stage.
Projection completes no later than post-fs and before projected HALs start;
post-fs-data and post-mount scripts run at the real post-fs-data event, after
late-fs has supplied `/data`, but before stock post-fs-data commands.
Projected module-backed files stay writable, unrelated Android mounts stay
read-only. Never mutate an
active lower or reuse upper/work with live overlay references; unchanged
generations keep their uppers, and durable state stays outside generations.

Owned overlays and backing/staging mounts are released before handoff. Only a
known ESP-backed loop and actual EBUSY allow raw-ESP detach; vendor mounts and
executable tmpfs are not detached. Blocking second-stage bootstrap reconstructs
the same selected devices and generations before consumers start. Reconstruction
does not introduce a second permanent ESP mount.

Reconstruction runs stock tools as explicitly labelled `init` services, with
blocking `exec_start`, `reboot_on_failure`, and a 120-second timeout for each
prerequisite. Reconstruction alone publishes `egysk.bootstrap.ready=1` after
mount, policy, label and descriptor completion. Its action waits for that value
before product dispatch. Every logical stage uses a disabled named oneshot,
`exec_start`, a 300-second outer timeout and `reboot_on_failure`, followed by
`wait_for_prop egysk.stage.<stage> 1`. Only the serialized native completion
boundary publishes stage success, after native callbacks/projection and module
work; it acknowledges duplicate dispatch only after publication succeeds.
Failed execution requests, unsuccessful exits and timeouts reboot; even a
service lookup failure cannot advance stock same-event actions past an absent
success property. A failed reboot leaves the consumer barrier closed.
Selected block nodes live under `/dev/block` with mode 0600. The helper's initial
policy allows copying, relabelling, backing mounts and native policy load;
it does not make the product domain permissive.

The ordering basis is pinned AOSP system/core
[`68be0c2c0006a0740d0b1809abe4717308f90d15`](https://android.googlesource.com/platform/system/core/+/68be0c2c0006a0740d0b1809abe4717308f90d15/):
[`init.cpp`](https://android.googlesource.com/platform/system/core/+/68be0c2c0006a0740d0b1809abe4717308f90d15/init/init.cpp)
queues early-init, coldboot wait, then init and suspends command execution for
exec services/property waits;
[`action_manager.cpp`](https://android.googlesource.com/platform/system/core/+/68be0c2c0006a0740d0b1809abe4717308f90d15/init/action_manager.cpp)
matches actions in registration order;
[`service.cpp`](https://android.googlesource.com/platform/system/core/+/68be0c2c0006a0740d0b1809abe4717308f90d15/init/service.cpp)
implements ExecStart/Start failure reboot guards, failed-exit reboot and timeout
killing. The stock
[`init.rc`](https://android.googlesource.com/platform/system/core/+/68be0c2c0006a0740d0b1809abe4717308f90d15/rootdir/init.rc)
starts vold at early-fs and queues post-fs-data after late-fs. Consumer preparation
must finish synchronously in critical module scripts: early-init work precedes
vold, and post-fs-data checks precede consumers started by stock post-fs-data or
later actions. Module RC property actions run asynchronously after publication;
`ctl.start` acknowledges a start request, not service readiness. Neither is a
substitute for script completion. Scripts must not wait for later init actions,
stock post-fs-data-created directories, installkey or CE unlock. The product
contract requires `/data` mounted by late-fs and credential consumers not started
in stock early-init or before their prerequisite event; core does not infer ROM
identity or maintain service-name allowlists.
The pinned
[`gatekeeperd.rc`](https://android.googlesource.com/platform/system/core/+/68be0c2c0006a0740d0b1809abe4717308f90d15/gatekeeperd/gatekeeperd.rc)
uses class `late_start`, started by the stock nonencrypted/framework restart
actions after post-fs-data. Product packages preserve that service schedule.

The native policy tool consumes a coherent helper-exported snapshot, preserving
Android policy configuration bits that the kernel's ordinary sysfs export omits.
The snapshot is private and removed after use. Failed policy writes propagate a
nonzero status rather than allowing reconstruction to continue. Reconstruction
enters `egysk` before relabelling persisted uppers or creating final overlays.
Those uppers retain Android export labels across boots; stock init does not
need broader access to those labels, and overlays retain product credentials.

Owned filesystem permissions cover OverlayFS directory reparenting and whiteout
metadata operations, not character-device I/O. Context-mounted efivarfs requires
explicit superblock relabel/mount permissions as well as the sysfs mountpoint.
The native daemon must leave its blocking init client's cgroup; the policy
authorizes its existing cgroup/v2 detachment operations. Startup errors reach the
kernel log before Android logd and the product FIFO reader are available.

### Explicit offline product-state transition

One engine handles each physical metadata or ESP product subtree:

```sh
egyskinit --transition-product-state --offline PHYSICAL_BACKING_ROOT
```

Stop all consumers in **all namespaces** first, and expose the physical backing
in an offline maintenance namespace. `--offline` asserts that precondition; the
engine cannot prove other namespaces idle and never quiesces them automatically.
Run separately on the selected metadata backing and ESP backing as needed.
The root-only engine rejects PID1, either current/legacy runtime root, current
mount references (including bind roots and OverlayFS options), non-absolute or
symlinked backing paths, non-directory/symlink product roots, missing state and
old/new collisions. It locks the backing directory, renames `kernelsu-esp` to
`egysk` atomically with `RENAME_NOREPLACE`, and fsyncs the parent. Unsupported
filesystems fail closed; there is no copy, merge, reset or managed-boot fallback.
Successful repeat invocation returns `AlreadyTransitioned`.

The in-place rename preserves inodes, xattrs, journals, module edits and state,
including persisted `.esp-generation` and `esp-tmp` markers. It never traverses
credential siblings such as `password_slots`, recursively relabels metadata,
or transitions credential identity. The stopped-consumer `host`/peer credential
transition is implemented separately in the shared installation registry and
provisioning commands; it is not another product-root migration engine.

## Platform contracts retained through integration

- `gobbl-runtime` owns independent physical-metadata bootstrap and Android
  property/shared-state corrections, before each affected consumer. Its early
  bootstrap is a cpio input, not dependent on its own effective module view.
  Module lifecycle operations must never traverse Android credential state.
- Every Android installation is a peer, including the former ROM1/`host`.
  Boot slot selects an installation; immutable installer identity selects its
  monotonic namespace, never a module profile. The shared `installation-state`
  crate allocates fresh namespaces 2..2146, with local user IDs 0..999999 and
  checked hardware UID `namespace * 1,000,000 + local_user_id`; this is not a
  cryptographic SID. Explicit offline adoption preserves existing hardware
  namespace zero as immutable record data while assigning positive Weaver
  ownership. It does not rewrite handles, tokens, KeyMint bindings or local SIDs.
  Shared `/metadata/password_slots/slot_map` uses Java Properties `gsiN` owners
  and `ro.gsid.image_running=N`; namespace one is excluded because Android's
  boolean property consumers would interpret it as ordinary DSU mode.
  Journaled adoption/replacement publishes the selected ESP config with the
  registry; pending transitions block runtime admission. Retirement does not
  claim hardware erasure or recycle a namespace.
- Runtime preparation disables vold's global metadata-key deletion and binds
  the shared slot map before vold. The pinned Gatekeeper patch removes global
  cold-boot `deleteAllUsers`, retaining per-user cleanup. Early preparation
  verifies the projected daemon's protocol and publishes its read-only namespace
  handoff; post-fs-data rechecks these before Gatekeeper's late-start service.
  A `.coldboot` marker alone is not sufficient proof.
- Independent thin retains the owned `dm-thin-pool` / `dm_thin_pool` fork,
  target versions and suspend/resume/gate behavior. One pinned `lvm2` recipe owns
  static `lvm`, `dmsetup`, FAT-safe `dmstats` and configuration; LVM owns its own
  mappings, upstream `dmsetup` owns product mapping reloads. Preserve DM
  names/UUIDs/tables and existing-storage activation; neither is a prerequisite
  of the shared module store.
- The Partformer configfs ABI4 interface (unrelated to the helper's uapi ABI 4
  above) supersedes the proposed ABI3 integration: generic
  `View(name, backing, sectors, access, offset)`,
  `Endpoint(backing, hide, writable)` and
  `Plan(mapping, views, endpoints, seal, lvs)` belong to partformer; ROM selection
  belongs to the gobbl-multi-os adapter. ppconf only checks/operates the externally
  provisioned configfs ABI, never mounts configfs or loads the LKM.
  Production ABI4 uploads 67/70/76/88 ordered 512-byte sector records for
  512/1024/2048/4096-byte logical blocks, then uses
  `pp-meta`/`pp-range`/`pp-hole` DM targets, `endpoints`, commit and reset after
  normal DM removal. GPT construction uses one bounded metadata buffer;
  upstream DM owns data I/O and queue/crypto propagation. Partition publication,
  writable/RO ranges, physical PARTNAME hiding and seal behavior remain separate
  from protection against a hostile root ROM.
- EFI remains at `/sys/firmware/efi/efivars`, type `efivarfs`, with native EFI
  ownership preserved. `efivar_store` brackets backing-file I/O with
  `override_creds(store_file->f_cred)` / `revert_creds`; opener-domain block
  access still needs policy. Select by unique `partuuid` or Surfacer
  `/chosen/efivar-store,partuuid`, mutually exclusive with explicit `dev`.
  Never infer identity from the `bdsvars` label. Distinct identifiers are type
  GUID `4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709`, vendor namespace
  `7a5e4b1c-0d3f-4e62-9b8a-1c2d3e4f5a6b`, configuration-table GUID
  `930e89ed-540e-4af0-9b41-c2c559939d50` and the selected partition's unique GUID.
- Keep downstream schema v2 ROM/firmware configuration separate from module
  inventory. Preserve `esu-bootctl` wire/state behavior, its separate
  `esu_bootctl` policy and early copy; retire `/dev/block/esd` only with all
  callers migrated. Shared `dm`, `ota-core`, `esu-vars` Git dependencies retain
  full revision pins and EFI wire formats, including the empty-efivarfs fix.
  BCB/watchdogs, allocation, provisioning and capacity policy stay downstream.

## Native userspace and exact artifacts

Magisk is pinned at `e8915d9db15f5aae93973ffe65068e34df375a6a` under
`vendor/Magisk`. `patches/Magisk/series` applies three additive patches in order:
product selection/identity, lifecycle/projection adapters, and policy/dependency
errors. `scripts/magisk.py` materializes a separate `out/<tree>` build tree,
rejecting fuzz, offsets and source drift. `product/identity.json` generates
native Rust/C++ constants; the owned dispatch adapter retains the upstream
serialized stage lock. Native links the authoritative
`userspace/egysk-runtime` by relative crate path, not copied mockup userspace.
Prepared-view built-in magic mount, late callbacks/common scripts and authorized
native SU remain; excluded stock side effects fail at their entry boundaries.
Retain upstream licenses.

```sh
git submodule update --init --recursive
python3 scripts/magisk.py --output out/Magisk build \
  --ndk /path/to/ondk --abi arm64-v8a

# Use a fully prepared matching kernel output and its compiler on PATH.
python3 scripts/build_artifacts.py \
  --kmi-src /path/to/kernel --kmi-out /path/to/kernel-out \
  --branch android16-6.12 --generation 6 --arch aarch64 \
  --ndk /path/to/android-ndk-r29 --output out/kmi --jobs 13
```

The native build uses Magisk's matching ONDK Rust/linker and the resolved
lockfile in the production patch series. Both native Cargo entry points use
`--locked`; sequential arm64/x86_64 builds preserve exact materialized source.
The independently built static rdinit uses the Android NDK.
`out/native/<abi>/bin` supplies native
package inputs; BusyBox is an explicit independent input. Artifact publication
creates an immutable complete `<branch>-<generation>/<arch>` set containing
`egyskinit`, `egysk.ko` and `egysk.ko.compat.json`; it does not replace
an existing set. The verifier checks real ELF architecture, vermagic, symbol
CRCs, private imports and byte-matched receipts. Objtool diagnostics fail admission.

## Standalone host cpio builder

The host `egysk-tools` package names its executable `egysk-build-cpio`; it is
distinct from the Android `egyskd` daemon. It consumes already-built inputs and
downloads nothing.

```sh
cargo build --release --locked -p egysk-tools
target/release/egysk-build-cpio --build-cpio --legacy-lz4 \
  --kmi android16-6.12-6 --arch aarch64 --artifact-dir out/kmi \
  --bootstrap-dir /path/to/bootstrap --entry egyskinit \
  --out /path/to/takeover.cpio.lz4
```

The bootstrap supplies `egysk.toml`, critical LKMs with compatibility
receipts, and independent early tools. ESP supplies matching `egysk/egysk.toml`.
The platform packager keeps its backing
helper in cpio and stages other tools from raw ESP, not from overlays that those
tools must construct. The archive contains newc records and optionally legacy
LZ4; it never supplies `/init`, `ksu_config` or a patched boot image. A branch-only
KMI selector must resolve to exactly one generation for the requested architecture.

`egysk-build.json` records architecture, exact KMI, entry and member
hashes/sizes/permissions. It excludes itself and has no package `build_id` field.
The modules packager publishes the separate product `build-id` and schema-2
`set.json` archive receipt. Gobbl/lab owns final boot-image assembly.

The shared `takeover-contract` crate owns archive admission for `rom-install`
and `ota-core`, leaving publication and OTA selection with those consumers.
Its read-only host CLI also checks produced archives at the lab boundary:

```sh
# From ../gobbl; the optional final argument byte-matches the exact helper set.
cargo run --release --locked -p takeover-contract --bin takeover-validate -- \
  /path/to/takeover.cpio.lz4 android16-6.12-6 aarch64 \
  ../egysk/out/kmi/android16-6.12-6/aarch64
```

Acceptance prints JSON with `"status":"accepted"`; wrong identity, mismatched
artifacts or corrupt framing fail rather than selecting another package.

## Exact KMI and DDK workflows

`.github/workflows/build-lkm.yml` dispatches manually on the prepared self-hosted
runner and derives everything from one root: `<kmi_root>/<branch>/source` supplies
the exact branch and generation (read from `build.config.constants`,
`build.config.common` or `bazel/constants.scl`),
`<llvm_root>/clang-<CLANG_VERSION>/bin` must hold the matching compiler, and
`<kmi_root>/<branch>/<arch>` must be a complete output (`Module.symvers`,
`System.map`, `include/generated/utsrelease.h`, `.config`). It then runs
`scripts/build_artifacts.py` and uploads the immutable `<branch>-<generation>/<arch>`
set from a private `RUNNER_TEMP` directory.

`.github/workflows/ddk-lkm.yml` is the reusable DDK gate (no in-repo caller yet).
`ubuntu-latest` starts a `ghcr.io/ylarod/ddk-min:<branch>-<ddk_release>` container,
which ships its own kernel source and a `modules_prepare`-only kdir and sees only
the runner workspace. The caller-supplied `kmi_src`, `aarch64_out`, `x86_64_out`
and `llvm_bin` must therefore be absolute, colon-free host paths, because they are
bound into the container at the identical paths. `llvm_bin`'s parent tree travels
with it, because the AOSP clang drivers carry `RUNPATH $ORIGIN/../lib` and a
`bin`-only bind leaves `libc++.so.1` unloadable. The image also exports
`ARCH=arm64` and `CROSS_COMPILE=aarch64-linux-gnu-`; kbuild turns that prefix into
`--target=aarch64-linux-gnu` for both matrix legs, which rejects the x86_64
`-mcmodel=kernel`, so the build step clears `CROSS_COMPILE` and lets
`scripts/Makefile.clang` derive the target from `ARCH`. A source without an
identity file, a missing `clang`, an empty or incomplete architecture output, a
non-`android<version>-<major>.<minor>` branch and a non-dated `ddk_release` all
fail admission before any build.

Earlier DDK admission (2026-10-09) exercised
`ghcr.io/ylarod/ddk-min:android14-6.1-20260828` with generation 11 outputs and
`ghcr.io/ylarod/ddk-min:android17-6.18-20260828` with generation 5 outputs, both
architecture legs. These historical producer results are not device loading
proof or coverage of all eight published branches. The images supply
`modules_prepare`-only trees; complete supplied outputs remain required. Current
renamed-helper generation-6 arm64 admission is summarized below.

Both routes admit exact branch/generation/compiler tuples, so every branch needs
its own prepared root and its own `clang-<CLANG_VERSION>` tree: `android12-5.10`
(generation 9, `r416183b`), `android13-5.10` (4, `r450784e`), `android13-5.15`
(8, `r450784e`), `android14-5.15` (11, `r487747c`), `android14-6.1` (11,
`r487747c`), `android15-6.6` (8, `r510928`), `android16-6.12` (6, `r536225`) and
`android17-6.18` (5, `r584948c`), each on `aarch64` and `x86_64`. Neither route
cross-checks that a supplied output was configured from the supplied source;
`egysk.ko.compat.json` records the inputs it was admitted against, and the
DDK gate re-verifies through `scripts/kmi_modules.py verify`, which compares
`<output>/source/build.config.constants` when the output tree carries that link.

## Checks and integration

```sh
cargo ndk -t arm64-v8a check --locked -p egysk-runtime -p egyskinit
cargo ndk -t arm64-v8a clippy --locked -p egysk-runtime -p egyskinit -- -D warnings
cargo fmt --all
cargo test --workspace --locked --offline
python3 -m unittest scripts.test_kmi_modules
```

Exercise produced archives and real module operations, not just schemas or
compiler exits. The generation-6 x86_64 row is admitted against the supplied CF
output whose `Module.symvers` carries the stock CF `module_layout` CRC while its
own `vmlinux.symvers` holds a different one, so that row is not
kernel-provenance-qualified. Enforcing Android lifecycle, OTA and physical
credential-consumer behavior remain distinct requirements from source builds and
admission; do not read them as satisfied by this document.

Parent-run integration checks cover core and platform host contracts, Android
arm64/x86_64 builds and Clippy, both AArch64 UEFI lanes, exact native replay
before and after dual-ABI builds, and helper/independent-LKM KMI admission.
The actual Android Partformer CLI produced CRC-valid 512-byte and 4Kn GPT
metadata. Actual offline maintenance preserved hardware namespace zero,
slot-map ownership/mode/xattrs and unrelated payloads while publishing and
retiring installation identities. These temporary-root checks do not establish
phone credential behavior. Pinned Gatekeeper builds and complete packages passed
for arm64 and x86_64; both takeover archives passed exact-artifact admission.
Real stock-image assembly and the lab's installer source admission also passed.
Whole-device integration remains a separate check.

## Remaining Egysk integration checklist

Recovered split sources, native replay, helper reduction, shared-store runtime
and standalone builder are implementation inputs, not greenfield tasks.
Earlier enforcing Cuttlefish OTA results describe their recorded revisions,
not proof of the newly integrated platform. Selected mockups supply integration
deltas, not deployed replacements.
This is the authoritative remaining-work list; the storage split draft retains
ownership rationale, not a competing implementation plan.

- [x] **Repository publication:** owner-authorized source pushes are complete;
  all four published histories passed restore-and-compare.
  - [x] Rename public core to `1vivy/egysk`, create both private repositories,
    move the local core checkout, and reconcile origins/recovery remotes.
    Upstream submodule identity/gitlink and imported generic-bootctl history
    remain intact; shared-crate dependencies carry full verified revision pins.
  - [x] Recover authoritative split main revisions at `../egysk-modules` and
    `../egysk-lkms`, preserving complete histories (revisions listed above).
- [x] **Complete rename surface:** migrate downstream platform init/`rdinit`
  callers, module/tool packaging, config, environment, properties, services and
  SELinux consumers as one coordinated platform cutover. Update CI
  checkout paths, self-hosted runner labels, artifact upload/download patterns,
  release assets, installer URLs, disclosure/issue links and live docs.
  Preserve independent esu/gobbl/platform names, EFI GUIDs and frozen wire
  formats unless a separately required contract change calls for migration.
  Historical records, copyright and genuine upstream identifiers are not a
  global search-and-replace target. Existing on-disk state needs an explicit
  transition; renaming a directory is not authority to discard module state
  or credentials. Leave no product compatibility aliases after cutover.
  - [x] Core daemon/builder/loader/helper identities, runtime/ESP roots,
    bootstrap config/receipt, owned env/properties/services and policy names
    use Egysk; standalone generic `/init` handoff remains intact.
  - [x] Implement and wire explicit offline metadata/ESP product-subtree and
    journaled credential-identity transitions; no automatic boot-time adoption.

- [x] **Native source integration and Egysk identity:** both native producers and
  downstream packaging passed; whole-device behavior remains separately pending.
  - [x] Port the selected
    [Magisk additive-boundary design](../mockups/2026-10-09/magisk-selection/README.md)
    into one production three-patch series/materializer; link authoritative
    runtime sources within the enforced `out/<tree>` layout.
  - [x] Retain upstream stage serialization, prepared-view built-in magic mount,
    actual late callbacks/common scripts and authorized native SU; refuse
    excluded stock side effects at entry boundaries. Exact replay and both
    native architecture producers are green, not whole-device proof.
- [x] **Partformer ABI4 source cutover:** the
  [selected design](../mockups/2026-10-09/partformer/README.md) is implemented as
  Rust Partformer/ppconf with Android arm64/x86_64 producers. Independent ABI4
  LKMs build and pass admission in both lanes. Firmware views, gobbl-multi-os
  and OTA/HAL callers use the shared check/prepare/publish/reload/teardown
  lifecycle, retaining rollback, busy-removal failure and stable mapped-device
  identity across reload. The actual CLI covers 512-byte and 4Kn metadata.
  Live partition discovery, projected I/O and hardware-wrapped crypto remain
  part of the final device integration check, not consequences of a build.
- [x] **Peer credential source lifecycle:** integrate the
  [credential coordinator and pinned gatekeeperd patch](../mockups/2026-10-09/gobbl-credentials/README.md)
  with existing installation/update/wipe selection, `gobbl-runtime`, packaging,
  init dependencies and policy. System-only updates preserve identity; wipe/new
  install assigns a new one with explicit replacement, never namespace reuse.
  Follow Android GSI/DSU credential allocation, ownership and deletion lifecycle;
  module removal must not delete an installation's credentials. Gobbl itself
  owns no slot. UID/slot semantics and local fake-user bounds are implemented;
  PIN unlock and biometric/vendor interactions remain final device checks.
  **Capacity decision (2026-10-10):** assume 16 Weaver slots for five Android
  installations, roughly three slots per installation. This is a planning
  assumption, not a measured device capacity or a per-ROM quota. Capacity
  discovery, quotas, reservation policy and generalized reclamation are out of
  scope; organic exhaustion remains Android/backend behavior, not a gobbl
  capacity-management responsibility. Do not mask the backend's failure.
- [x] **Cross-repository wiring:** reconcile changed native hooks, configfs ABI4
  and peer identities through schema v2, Surfacer, bootgen/provisioning,
  module/tool packaging, cpio/OTA assembly and gobbl-lab fixtures. Keep one
  module inventory and shared view, independent bootstrap inputs, exact KMI
  receipts and separate package build-id. Remove obsolete inherited-FD and
  duplicate admission paths where callers still use them. Preserve LVM
  activation/configuration semantics, including the pinned release's
  `thin_check_executable` behavior.
- [x] **Release and bootstrap closure:** integrate the recovered platform
  packager source and consume renamed core/modules/LKM release inputs with
  their existing exact revision/KMI/artifact identities. The core's
  three-file helper set is not the platform OTA set: trace production of
  `set.json` and `takeover.cpio.lz4` through initial installation and OTA
  selection (`rom-install`, `ota-core`, platform packager and lab installer).
  Package independent critical LKMs, compatibility receipts and early tools
  separately from the generic helper. Reconcile bootstrap configuration in
  cpio and ESP, including EFI module names and unique-PARTUUID selection;
  do not revive legacy `dev=by-name:bdsvars` examples. Keep ROM schema,
  bootstrap config, cpio receipt, OTA-set receipt and KMI receipt contracts
  distinct; a shared numeric schema version does not make them one schema.
- [x] **Bounded ownership refactors during integration:**
  - [x] Generate native constants from owned `product/identity.json`; consolidate
    runtime path/policy/tool/mount constants without depending on a materialized
    Magisk tree for standalone runtime/init builds.
  - [x] Use `egysk_runtime::Stage` for names/order and enum matches. Native
    outer completion covers projection/common callbacks and runtime dispatch
    before ACK under the upstream lock; runtime duplicates alone are not relied on.
  - [x] Share bootstrap tool/mount data; retain parsed/transformed module RC and
    service schedules from admission through assembly, without reopening after
    scripts. PID1 syscalls, stock-init service text and reconstruction remain
    distinct execution contexts, not a generic workflow engine.
  - [x] Mirror C UAPI ioctl/layout/policy constants in runtime `helper::abi`,
    with compile-time layout checks and the C ABI gate. Keep ABI 4 transport
    bytes and upstream policy numbers unchanged; no binding framework.
  - [x] Consolidate archive validation in shared `takeover-contract`, consumed
    by `rom-install`, `ota-core` and the read-only `takeover-validate` lab CLI;
    retain consumer publication/selection policy and licensing boundaries.
  - [x] Share the `no_std` GBS1/GBM1/GBT1 codec while preserving bytes, bounds
    and state semantics; keep firmware storage and Android efivarfs separate.
  - [x] Migrate platform callers to the shared interfaces and exact gobbl
    revision locks; final artifact production is tracked separately above.
- [x] **Lifecycle and documentation consumers:** connect installation identity
  to install/update/wipe and existing ROM-retirement work, including the
  stopped-consumer `host` transition. Trace staged-letter boot, staged cpio,
  `Stage-<id>` and live DM reload through Surfacer and OTA/HAL callers. Keep
  generic module uninstall separate from Android credential deletion; use
  Android's lifecycle for the latter. Update ABI3 diagrams/limits, stale
  vendoring/producer paths and obsolete build instructions to the integrated
  source. Do not silently expand this cutover into all unimplemented operations
  in the older provisioning plan.
- [ ] **Close only remaining behavioral gaps:** reuse source/RE, existing suites,
  enforcing CF OTA results and existing real-device userspace observations.
  Check original-init handoff and reconstruction of the same source/generations,
  one RW ESP, metadata ownership, policy/labels before early HALs, exactly-once
  stages and optional/critical failure handling for the integrated deltas.
  Retain package replacement/rollback, shared state across installations,
  projected writes and EFI HAL-domain I/O behavior. The existing nine-row OTA
  baseline is a regression reference, not nine unfinished implementation tasks.
  Record unresolved credential consumers and module operations honestly;
  no mandatory new harness, phone loop or hostile-ROM security proof is added.
  Physical integration/deployment remains a separate owner-selected action.

No successful build implicitly authorizes push, deployment, remote rename or
physical-device writes.

KernelSU history and copyright notices are retained. [LICENSE](LICENSE) remains
GPL-3.0; pinned Magisk and other subtrees retain their own license notices. See
[SECURITY.md](SECURITY.md) for the product boundary and disclosure route.
