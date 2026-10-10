# KernelSU ESP

A separated Magisk-derived native userspace, an rdinit loader, and a small kernel
helper. The implementation plan and acceptance requirements are in
[KERNELSU_REFORK_PLAN.md](KERNELSU_REFORK_PLAN.md). Source builds and policy-free
VM checks do not establish an enforcing Android boot or credential safety.

## Ownership and identity

- This repository owns pinned Magisk, its ordered product patches, `esuinit`,
  the shared runtime, the standalone host cpio builder, and `kernelsu-esp.ko`.
- `kernelsu-esp_modules` owns platform modules, `gobbl-runtime`, Android CLIs,
  ROM/storage/OTA/HAL handling, and product packaging.
- `kernelsu-esp_lkms` owns independent thin, partition-projection, efivarfs and
  efivar_store LKMs. They have no dependency on a root-provider credential ABI.

The Android native daemon is `ksud`; its domain is `esp`, with `esp_file` and
`esp_log_file` types. Product sockets, services, paths and installer state are
separate from other root installations. Traditional app SU is neither the goal
nor something to strip indiscriminately: retained upstream authorization stays
intact, without claiming the system's ordinary `su` entrypoint. There is no
Manager deliverable, provider negotiation, SU forwarding or alternate backend.

The kernel helper's internal name is `kernelsu_esp`. ABI 4 supplies root-only
policy application, coherent live-policy export and the one-pass init-rc
supplement, plus retained module symbol machinery. It does not grant credentials or expose app allowlists,
profiles or SU compatibility. Second-stage exec observation initializes the
bounded base policy before `esp`-labelled init commands run. The independent
EFI backend brackets device I/O with its backing file's opener credentials.

## Boot and storage contract

`rdinit=/esuinit` enters the platform loader; `/ksuinit` is the generic archive
entry. Critical LKMs are loaded before ESP discovery. The original `/init` is
executed without replacement, preserving the remaining arguments and environment.

```text
/dev/esp                         one raw RW ESP, nosuid,nodev,noexec
/dev/esp/kernelsu-esp/            product config, bin, modules and exact KMI sets
/dev/esp/esu/                    downstream schema-2 ROM configuration
/dev/kernelsu-esp/bin/            executable tmpfs tools
/dev/kernelsu-esp/store/          shared physical-metadata product subtree
/dev/kernelsu-esp/modules/<id>/   writable effective module views
/dev/kernelsu-esp/state/<id>/     durable shared module state
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
process-group bound. Scripts receive `KSU_BACKING_ROOT` for the selected physical
backing, independent of Android's projected `/metadata`. Services remain disabled
until their owning stage succeeds. Early Android projection uses Magisk's built-in
magic mount, without running post-fs-data scripts early.

Owned overlays and backing/staging mounts are released before handoff. Only a
known ESP-backed loop and actual EBUSY allow raw-ESP detach; vendor mounts and
executable tmpfs are not detached. Blocking second-stage bootstrap reconstructs
the same selected devices and generations before consumers start. The temporary
`/debug_ramdisk/kernelsu-esp` staging path is not a second permanent ESP mount.

Reconstruction runs stock tools as explicitly labelled `init` services, with
blocking `exec_start` and `reboot_on_failure` for each prerequisite. Selected
block nodes live under `/dev/block` with mode 0600. The helper's initial policy
allows the required copying, relabelling, backing mounts and native policy load;
it does not make the product domain permissive.

The native policy tool consumes a coherent helper-exported snapshot, preserving
Android policy configuration bits that the kernel's ordinary sysfs export omits.
The snapshot is private and removed after use. Failed policy writes propagate a
nonzero status rather than allowing reconstruction to continue. Reconstruction
enters `esp` before relabelling persisted uppers or creating final overlays.
Those uppers retain Android export labels across boots; stock init does not
need broader access to those labels, and overlays retain product credentials.

Owned filesystem permissions cover OverlayFS directory reparenting and whiteout
metadata operations, not character-device I/O. Context-mounted efivarfs requires
explicit superblock relabel/mount permissions as well as the sysfs mountpoint.
The native daemon must leave its blocking init client's cgroup; the policy
authorizes its existing cgroup/v2 detachment operations. Startup errors reach the
kernel log before Android logd and the product FIFO reader are available.

## Native userspace and exact artifacts

Magisk is pinned at `e8915d9db15f5aae93973ffe65068e34df375a6a` under
`vendor/Magisk`. `patches/Magisk` applies exactly into a separate build tree;
materialization rejects fuzz, offsets and source drift. Retain upstream licenses.

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

The native build uses Magisk's matching ONDK Rust/linker. The independently
built static rdinit uses the Android NDK. `out/native/<abi>/bin` supplies native
package inputs; BusyBox is an explicit independent input. Artifact publication
creates an immutable complete `<branch>-<generation>/<arch>` set containing
`ksuinit`, `kernelsu-esp.ko` and `kernelsu-esp.ko.compat.json`; it does not replace
an existing set. The verifier checks real ELF architecture, vermagic, symbol
CRCs, private imports and byte-matched receipts. Objtool diagnostics fail admission.

## Standalone host cpio builder

The host `esp-tools` package also names its executable `ksud`; it is not the
Android daemon binary. It consumes already-built inputs and downloads nothing.

```sh
cargo build --release -p esp-tools
target/release/ksud --build-cpio --legacy-lz4 \
  --kmi android16-6.12-6 --arch aarch64 --artifact-dir out/kmi \
  --bootstrap-dir /path/to/bootstrap --entry esuinit \
  --out /path/to/takeover.cpio.lz4
```

The bootstrap supplies `kernelsu-esp.toml`, critical LKMs with compatibility
receipts, and independent early tools. The platform packager keeps its backing
helper in cpio and stages other tools from raw ESP, not from overlays that those
tools must construct. The archive contains newc records and optionally legacy
LZ4; it never supplies `/init`, `ksu_config` or a patched boot image. A branch-only
KMI selector must resolve to exactly one generation for the requested architecture.

`kernelsu-esp-build.json` records architecture, exact KMI, entry and member
hashes/sizes/permissions. It excludes itself and has no package `build_id` field.
The modules packager publishes the separate product `build-id` and schema-2
`set.json` archive receipt. Gobbl/lab owns final boot-image assembly.

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

The gate is proven, not predicted (2026-10-09). Against the published
`ghcr.io/ylarod/ddk-min:android14-6.1-20260828`
(`sha256:1dd6ac340b627a90a4031d6d0df6b129d8cc949b139fb56032c898f023d3d5d3`,
clang `r487747c`) with the workspace's complete `android14-6.1` generation-11
outputs, and against `android17-6.18-20260828`
(`sha256:fb8b66200bf402cf65a751060fc1aa29524b9fa8840feef16c7b196a4f3f0e6e`,
`r584948c`) with its generation-5 outputs, both matrix legs passed the admission
step, `kmi_modules.py build`, the `release/<branch>-<generation>/<arch>` install
and `kmi_modules.py verify` with exit 0 and `"status": "accepted"`; the admitted
modules import 54/55 and 56/71 versioned symbols respectively, and a leg leaves
only kbuild products under `kernel/` (`*.o`, `*.cmd`, `modules.order`,
`Module.symvers`, `kernelsu-esp.ko` with its schema-2 receipt and the build log)
plus `release/`. No bound source, kernel output or toolchain was written.
Unproven here: the DDK images are `modules_prepare`-only, so loading the module
and every device lane remain separate requirements; only `android14-6.1` and
`android17-6.18` were run, although `ghcr.io/ylarod/ddk-min` publishes the dated
tag for all eight branches.

Both routes admit exact branch/generation/compiler tuples, so every branch needs
its own prepared root and its own `clang-<CLANG_VERSION>` tree: `android12-5.10`
(generation 9, `r416183b`), `android13-5.10` (4, `r450784e`), `android13-5.15`
(8, `r450784e`), `android14-5.15` (11, `r487747c`), `android14-6.1` (11,
`r487747c`), `android15-6.6` (8, `r510928`), `android16-6.12` (6, `r536225`) and
`android17-6.18` (5, `r584948c`), each on `aarch64` and `x86_64`. Neither route
cross-checks that a supplied output was configured from the supplied source;
`kernelsu-esp.ko.compat.json` records the inputs it was admitted against, and the
DDK gate re-verifies through `scripts/kmi_modules.py verify`, which compares
`<output>/source/build.config.constants` when the output tree carries that link.

## Checks and integration

```sh
cargo ndk -t arm64-v8a check -p esp-runtime -p esuinit
cargo ndk -t arm64-v8a clippy -p esp-runtime -p esuinit
cargo fmt --all
cargo test --workspace --locked --offline
python3 -m unittest scripts.test_kmi_modules
```

Exercise produced archives and real module operations, not just schemas or
compiler exits. The final integrated helper matrix passes both architectures
for all seven legacy/modern branches listed above, with unchanged provisioned
kernel inputs; the current android16-6.12 generation-6 pair also builds and
admits. The generation-6 x86_64 row uses the recorded CF pair whose
`Module.symvers` carries the stock CF `module_layout` CRC while its own
`vmlinux.symvers` holds a different one, so that row is admitted against the
supplied output and is not kernel-provenance-qualified. Enforcing Android
lifecycle, OTA and physical credential-consumer gates remain distinct
requirements; see the saved plan's observed execution state. No push,
deployment, remote rename or physical-device write follows implicitly from a
successful build.

KernelSU history and copyright notices are retained. [LICENSE](LICENSE) remains
GPL-3.0; pinned Magisk and other subtrees retain their own license notices. See
[SECURITY.md](SECURITY.md) for the product boundary and disclosure route.
