# KernelSU ESP: userspace patchset, rdinit and native LKMs

## Decision and scope

Implement one ESP module framework, not another app-root implementation. Use pinned Magisk native userspace plus a small product patchset; port the existing rdinit, module and storage contracts. Keep ordinary Linux modules for functionality that actually belongs in the kernel. The current checkout is the port source, not the new upstream base.

This is an implementation plan. The isolated Magisk mockup tests the size and feasibility of the product patches; it is not a deployed or boot-qualified stack. This revision does not rename repositories, deploy payloads or authorize device writes.

Three repositories retain the agreed names:

| Repository | Owns |
| --- | --- |
| `kernelsu-esp` | Pinned native userspace, ordered patches, rdinit/stage runner, standalone cpio builder, small kernel helper |
| `kernelsu-esp_modules` | Installed ESP modules, platform/storage helpers, Android CLIs and packaging |
| `kernelsu-esp_lkms` | Independent thin, partition-projection, efivarfs and efivar_store LKMs, their ABIs and ACK/DDK builds |

Main-repo storage preparation accepts an opaque source/profile descriptor from a supplied helper. LVM, EFI-variable protocols, ROM manifests, HAL/OTA and BCB stay in the modules/platform repositories. Preserve legacy provenance when splitting history; update every caller in the cutover, with no compatibility aliases.

## Small upstream patches

Pin Magisk `e8915d9db15f5aae93973ffe65068e34df375a6a` under `vendor/Magisk/`. Keep that pin pristine, with one ordered series under `patches/Magisk/`; materialize patched build sources separately. Apply patches exactly, with no fuzz or offset. Preserve licenses and upstream source naming where it is not a runtime identity.

The product patch surface is limited to:

1. **Disable Manager and app-root policy.** Do not build/embed/install an APK, crown a Manager, grant app SU, or run its authorization/profile machinery. Keep root/init-only module and stage operations, with a root-only socket and peer checks. Retain SELinux application; remove upstream permissive/unconstrained grants, app socket access and global denial of other policy loaders. Use scoped `esp`/`init` rules, not a blanket policy exception.
2. **Rewrite owned paths and namespace.** Ship the daemon as `ksud`; use domain `esp`, file types `esp_file`/`esp_log_file`, and distinct control/socket/service names. All module/update/state/config paths resolve to the selected profile view. Executable/helper-script paths resolve to staged `/dev/kernelsu-esp/bin`, never the noexec FAT package bin. Migrate C++/Rust constants and installer paths together.
3. **Port rdinit; add small inner stage points.** Magisk's existing serialized dispatcher starts at post-fs-data and gates on `/data`; do not fake that event before userdata. Port existing early dispatch separately, with a small ordered phase loop and the existing script runner, not a new lifecycle framework. Route module handling/projection through the one admitted view and selected metamodule, not both Magisk magic mount and the metamodule.

The product is selected at build time and explicitly entered through rdinit. It does not need a Manager toggle or a second stock/ESP backend. Merely renaming a daemon is insufficient: migrate self-exec paths, sockets, rc commands, script environments, mounts, policy types and installer paths together.

Keep the installed root's `/init`, `/kernelsu.ko`, `/data/adb/ksud`, Magisk paths, sockets and policy untouched. Our handoff executes the original `/init`, even when it is another root's wrapper. Do not run a second upstream root startup or claim its later stages. Root/init permission checks apply to our control operations; no app-root listener is shipped.

## Kernel helper, not kernel root

Reduce the legacy core to the mechanisms the userspace framework still needs:

- Existing one-pass init-rc supplement and root-only SELinux policy application/cache refresh.
- Scoped backing-file I/O credentials; no permanent credential change to a caller.
- Required native-module symbol machinery and existing syscall-wrapper ownership/lifetime handling.

Build it as **`kernelsu-esp.ko`**, internal name **`kernelsu_esp`**. Migrate artifact, loader and cpio names together. Retain a distinct control FD/install identity; reusing another root's version probe is not helper discovery. Do not add `/dev/ksu` or another driver transport.

Remove app SU, Manager/profile/allowlist machinery, SU compatibility and daemon/app credential-grant logic. Retain the existing ownership-checked `execve`/`execveat` observers **only** for second-stage init policy readiness, plus read/fstat hooks for rc delivery. They call saved originals and do not replace paths, alter arguments or grant caller credentials; remove `setresuid` root-grant interception. Pin published callbacks for the boot; never force-unload or restore over a foreign owner.

Preserve the rc supplement's root permission, pointer/length/reserved validation, 64 KiB bound, one supply, `EALREADY` on repeat supply and `EBUSY` after consumption. The helper is not a root backend: boot mode, admission, safe-mode inputs and stage completion live in the userspace boot context, not kernel-root reports.

Keep the existing second-stage exec observation from the recorded coexistence repair (`60a16cf7`): it applies bounded base `esp` declarations and refreshes helper credentials after Android loads policy, before an `esp`-labelled rc command runs. Do not remove that observer while leaving its policy initialization behind. Module rules are applied once by the later bootstrap, before their services start; normal app/zygote execs do not drive our lifecycle.

Decouple the I/O provider from `ksu_cred`. Export `esp_io_cred_enter/leave`; update efivar_store in the same cutover. At helper load, retain a trusted rdinit bootstrap credential so **pre-policy EFI/profile discovery works**. At the retained second-stage policy point, publish the owned credential with its `esp` identity before Android consumers start; reference lifetime must cover concurrent entry during refresh. `enter` returns the previous credential or `ERR_PTR(errno)` when genuinely unready; callers skip I/O on error and call `leave` only after success. Pair `override_creds`/`revert_creds` in one execution context and balance provider `symbol_get`/`symbol_put`. An absent optional provider leaves caller credentials, not fake successful privilege. UID 0 alone does not solve SELinux backing-file/FD-use checks.

Retain the existing userspace ELF prebinding loader for private vmlinux imports. Use in-module runtime resolution for private calls that need it, without importing another root runtime. Vendor modules use normal `finit_module`, Android module-list ordering, dependencies, softdeps and options. Symbol resolution does not repair incompatible layouts, CFI, CRCs or signatures.

## Installed layout and writable views

```text
ESP/
  EFI/                                    firmware assets, unchanged
  kernelsu-esp/
    config.toml                           source/storage configuration
    bin/{ksud,busybox}                     bootstrap inputs, not executed from FAT
    modules/<id>/                         installed shared package
      module.prop
      system/{bin,etc,...}
      rd/{bin,etc,...}
      rdinit.sh
      early-init.sh / init.sh / early-fs.sh / post-fs.sh
      post-fs-data.sh / post-mount.sh / service.sh / boot-completed.sh
      initrc/*.rc
      sepolicy.rule
      kmod/<branch>-<generation>/*.ko
    modules_order                         ordering, not inventory
    modules_update/<id>/                   staged package replacement
    modules_previous/<id>/<generation>/    rollback lower
    kmi/<branch>-<generation>/<arch>/       core/ramdisk artifacts and receipts
    receipts/
    lab/                                  explicitly enabled only
  esu/{manifest.toml,roms/<id>.toml}        platform schema v2, no module inventory
  rom/<id>/{esu.cpio,esu.stage.cpio,...}    platform boot/OTA assets

profile LV: ext4, mounted at /dev/kernelsu-esp/store
  profiles/<rom-or-shared>/
    overlays/<id>/<package-generation>/{upper,work}
    modules/<id>/                         writable local installs
    modules_update/<id>/                   local install staging
    state/<id>/                           durable state
    preferences/                          disable/remove independent of generation
    magic_mount/{config.toml,custom,...}

final runtime:
  /dev/esp                                one RW raw ESP mount
  /dev/kernelsu-esp/bin/                   executable tmpfs tools
  /dev/kernelsu-esp/source                 selected boot descriptor
  /dev/kernelsu-esp/store/                 profile filesystem
  /dev/kernelsu-esp/profile/               bind of store/profiles/<selected>
  /dev/kernelsu-esp/modules/<id>/          RW shared overlays or local binds
  /dev/kernelsu-esp/state/<id>/            selected durable-state binds
  /dev/kernelsu-esp/metamodule             link on Unix-capable storage
```

Mount the raw ESP RW with `nosuid,nodev,noexec`; do not restore it to RO after writes. Copy early executable files to tmpfs. FAT cannot supply Unix permissions, symlinks or per-file SELinux labels.

Bind the selected profile before daemon startup. Native secure/update/database/log/common-script roots use `/dev/kernelsu-esp/profile`, not the raw LV root; module and state binds come from that same profile. This is one selected view, not another ESP mount or runtime backend switch.

Use **one module-tree OverlayFS per shared package**, with upper/work on the same ext4 filesystem and distinct pairs per profile/module/package generation. The provider activates existing configured storage; boot never formats or allocates a fallback LV. Local installs are writable LV directories, not additional lowers. A module ID cannot simultaneously belong to shared and local storage.

Packages are shared across ROMs; writable overrides, local installs, preferences and state are per ROM by default. Use the existing EFI ROM identity, with an opaque profile key supplied to generic code. Slots of one ROM share its profile. An explicitly selected `shared` profile provides deliberate all-ROM state.

An updated lower does not merge with copied-up files, whiteouts or opaque directories. Stage updates, activate only before mounting, and select a **fresh upper/work for a changed package generation**. Unchanged modules keep their uppers. Preserve the old lower/upper pair for rollback; do not carry OverlayFS internals into the new pair. Durable state is outside generations; generation-local config requires explicit module migration or user recovery from the retained pair.

Record package activation so interrupted old-to-previous/new-to-installed renames can complete or roll back. Flush data/metadata before advertising a generation. FAT rename/fsync does not make the multi-directory change transactional. Never edit an active lower.

Use conservative OverlayFS options (`index=off,metacopy=off,xino=off,redirect_dir=nofollow` where supported). Prepare permissions/customization for new effective generations; after policy is available, copy up and label exports before consumers use them. Persist preparation completion only after all required preparation succeeds. Metadata copy-up can consume file-data space.

Use the existing selected metamodule for Android `system/` projection. Pin meta-magic_mount-rs `bfa551f61b010709e0ff5567eaae58ce1a8cfd2d` and patch its module/config/notification paths, marker handling and projected-file RO remount. Writes through projected files reach the module upper. Durable new files belong in the module tree, not a temporary mount skeleton. This does not make untouched Android partitions writable or automatically reconcile their OTA updates.

## Ordering and stage transition

Installed directories plus matching `module.prop` IDs are the inventory. `modules_order` ranks discovered modules; append unlisted installed modules in stable ID order. Do not exclude an unlisted metamodule or scan lab content implicitly. Preserve identifier/path hygiene, safe copies/symlinks, duplicate ownership errors and rc bounds; do not add a second approval inventory or repeated mount/context attestation.

Use the same admitted set for kernel objects, policy, rc, scripts and projection. Flags `critical`, `recovery_ok`, `disable`, `remove`, `skip_mount` accept `1`/`true`; absent/other values are false. Profile preferences remain outside lower generations. Retain one active metamodule and recovery/charger/safe-mode behavior.

Inner stage points are a small ordered dispatch inside the relevant lifecycle stage: **phase outermost, `modules_order` innermost**. They provide barriers, not a dependency solver. Modules perform their own prerequisite checks. Storage-provider bootstrap runs before module-view discovery so activating the LV never depends on tools inside that LV's overlays. Keep `rd/`, `rd_copy` and `early_bin` staging/collision semantics.

Boot sequence:

1. Enter `/esuinit` through `rdinit` (generic entry `/ksuinit`). Preserve original `/init`; set up `/proc`, `/sys`, `/dev`, classify boot mode and load vendor/critical ramdisk LKMs before ESP discovery. Critical helpers are cpio inputs, not hidden behind inaccessible ESP modules.
2. Resolve the selected ESP, report ambiguity, mount RW and stage tools on executable tmpfs. Run the supplied storage helper, select profile, finish pending package activation and construct effective modules. Retain exact source/profile/generations/order in the boot descriptor.
3. Load admitted module kmods and run ordered `rdinit.sh`/inner phases with bounded blocking execution (35 seconds, terminate and reap). Run from module directories with `KSU`, `KSU_MODE`, `KSU_STAGE`, `KSU_MODULE`, `MODDIR`, `KSU_MODULE_STATE` and the staged PATH. No shell evaluation of kernel parameters or lasting userspace daemons from rdinit. Optional failure removes the module before rc assembly; critical failure stops managed handoff.
4. Supply bootstrap rc followed by admitted module rc once, preserving `norc`; base policy initialization stays on the retained second-stage observer. Carry the selected descriptor through the actual rc/handoff channel, not a soon-to-disappear `/dev` file. Close script/cwd/FD references, fully unmount owned overlays, then release owned mounts in reverse order. Do not detach vendor mounts. Execute original `/init`, preserving PID, remaining argv and environment.
5. Android's root transition recreates `/dev` and `/sys`. Blocking bootstrap uses stock Android tools in the init context to recreate the **same selected** ESP, profile/view and executable tmpfs; copy ksud, policy tool, Busybox and installer helper script from package inputs. Only then launch product tools. Do not execute or relabel FAT, use detached tmpfs or start a daemon before its socket/log directories and base policy exist. `/debug_ramdisk/kernelsu-esp` is temporary staging, not a second permanent ESP mount.
6. Apply admitted module rules once through retained policy application, preserving foreign rules; refresh I/O credentials if needed, restore efivarfs and complete labels before exports/services are usable. Bootstrap precedes module actions; product services stay disabled until owning-stage preparation succeeds. Optional failure cannot start a predeclared service. Fatal bootstrap errors invoke managed fatal handling, not merely return nonzero and let Android continue unmanaged.
7. Dispatch `early-init`, `init`, `early-fs`, `post-fs`, `post-fs-data`, `post-mount`, `service`, `boot-completed`. Do not reapply declarations at post-fs-data. Preserve platform HAL startup before post-fs-data; metamount does not delay early HAL readiness.

`exec /init` itself does not destroy mounts; Android's root transition/cleanup does. Loaded LKMs, DM objects and referenced loop backends persist, but their pathnames do not. Fully unmount an old overlay before reusing its upper/work; `MNT_DETACH` with live references is insufficient. Rebuild the final view before consumers open it. Exactly one final raw ESP mount remains.

Failures report the actual operation and may write/flush a receipt to available RW storage. Generic code uses the existing fatal/reboot handling, with no BCB dependency and no silent RO, ephemeral-profile or foreign-root fallback.

## Independent LKMs and platform modules

Each LKM lives under `kernelsu-esp_lkms/kernel/<name>/` with standalone `Kbuild`, `Makefile`, sources, ABI docs and applicable `uapi/`. Release layout is decoupled from source. Reuse ACK/DDK aarch64/x86_64 lanes and `include/kernelsu_esp_lkm.h`: `KERNELSU_ESP_LKM_METADATA`, `KERNELSU_ESP_LKM_READY_PARAM`, `.modinfo` keys `kernelsu_esp_name/version/abi/critical`. Metadata is descriptive, not authorization.

- **Thin:** import the owned fork whole as `dm-thin-pool`, internal `dm_thin_pool`; preserve private renames, target versions, suspend/resume/gate and bufio changes. Do not convert this owned fork to an upstream patchset.
- **Part projection:** configfs ABI 3 at `/config/part-projection/{abi,format,state,last_error,commit,projections/,hide/}`. Projections have `dev`/`ro`; hidden backends use major:minor; commit takes `1` or `seal`. Preserve construction/apply/seal/destroy and capability checks. Idle/staged/active/failed transitions release acquired handles on failed commit, permit correction, and forbid active mutation. Preserve 128 projection/256 hide/36-byte-label limits, duplicates, invalid devices, RO and sealed-view errors. Remove old `/dev/gptctl` UAPI after migrating all clients.
- **EFI:** keep efivarfs upstream-close with the existing project-GUID patch; pin reconciled efivar_store for both GKI-LKM and DKMS. Mount **`/sys/firmware/efi/efivars`**; filesystem type is `efivarfs`. Acquire/create EFI sysfs objects with ownership-aware cleanup; never remove a native EFI tree. Directory presence is not a successful mount.

Use efivar_store's existing `partuuid=<unique-GPT-GUID>` or Surfacer's `/chosen/efivar-store,partuuid`; explicit `dev=<major>:<minor>` is mutually exclusive. Do not discover by `bdsvars` label or substitute a fixed unique GUID. Type GUID `4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709`, EFI vendor namespace `7a5e4b1c-0d3f-4e62-9b8a-1c2d3e4f5a6b`, configuration-table GUID `930e89ed-540e-4af0-9b41-c2c559939d50` and selected PARTUUID are different identifiers.

Generalize `kmi_modules.py` to explicit module directory/name, branch, exact generation and architecture. Preserve ET_REL/architecture, internal names, vermagic flags, exported CRCs, private relocation imports and receipt/artifact matching. `--kmi` selects build artifacts, not a new assessment framework; ambiguous branch-only selection is an error. Load the exact shipped `.ko` and propagate real readiness/ABI errors.

The modules repository ships:

- Native LVM bootstrap/static bionic tools (`rd/`, later `system/bin/`) and `system/etc/lvm/lvm.conf`, preserving scan/udev/lock settings. Use `vgchange -ay` on existing configured storage; compare DM names/UUIDs/tables with the legacy replica-PV fixture before retiring thin-activate. Verify `thin_check_executable` semantics for the pinned LVM release.
- `ppconf` at `system/bin/ppconf` plus ramdisk copy: version/status/ls, add name major:minor [--ro], hide major:minor, commit [--seal], abort. Mount configfs when required; success requires active state. CLI sources/releases belong here, not inside an LKM directory.
- `dmtricks`, `gobbl-multi-os`, `gobbl-ota`, `gobbl-boot-hal`: storage activation, EFI ROM selection, OTA staging, firmware views and projection in explicit inner-stage order. Keep shared DM tables/UUIDs and live switch-device reload semantics. Preserve `esu-bootctl` wire/state behavior, separate `esu_bootctl` policy and early executable copy. Remove `/dev/block/esd` only after all callers migrate.
- Shared Rust crates (`dm`, `ota-core`, `esu-vars`) as gobbl-owned Git dependencies pinned by full revision. Preserve EFI wire formats; use pinned efivar-rs plus the empty-standard-efivarfs fix. Platform/lab watchdog and any BCB operations remain explicit module-owned payloads, not generic boot dependencies.

## Implementation order

Each task ends in a focused commit after its affected checks. The mockup informs patch boundaries; do not mistake it for completion of the later integration tasks.

1. Freeze only agreed legacy work; record/tag actual provenance. Coordinate the three repository splits and names without rewriting historical paths. Import the recorded generic-bootctl revision with history; remote archiving/renames remain owner-controlled.
2. Pin upstream and replay the minimal native userspace product patches. Build no Manager/APK. Establish daemon/path/socket/policy isolation and root-only control before adding ESP orchestration.
3. Reduce/rename the kernel helper; port the existing rc/policy handoff and independently owned I/O provider. Remove obsolete root hooks and reports; migrate all UAPI/provider callers in one cutover.
4. Port rdinit loading, source/provider bootstrap, small inner-stage dispatch and original-init handoff. Extend the serialized userspace dispatcher for pre-/data stages; replace foreign-install/setup/preinit assumptions with the selected descriptor and admitted rules. Wire rc, base-policy observation and second-stage module policy/labels/services in the order above. Product script/daemon controls must not create or migrate another installation's files.
5. Implement shared/local effective views, per-profile preferences/state, generation replacement/rollback and upstream module ZIP install/update/disable/remove/uninstall against those roots. Shared uninstall is a profile preference; deleting an all-ROM package is explicit. No copy into `/data/adb/modules`.
6. Split/build the independent LKMs and migrate projection/EFI consumers to the agreed ABIs. Implement module templates and CLI/tool packaging, then platform stage ordering and early HAL startup.
7. Add standalone `ksud --build-cpio --legacy-lz4 --kmi android16-6.12 --arch aarch64 --artifact-dir ./release --bootstrap-dir ./bootstrap --out ./takeover.cpio.lz4`. Emit newc (legacy LZ4 when requested), selected `/ksuinit` or explicit platform `/esuinit`, `/kernelsu-esp.ko`, product-only config, critical LKMs/tools and exact receipts. No `/init`, stock `/ksu_config`, patched boot image, implicit download or generic ROM/LVM/BCB dependency. Platform packaging supplies bootstrap inputs; gobbl/lab owns final image assembly. This command is not yet implemented.
8. Reconcile schema v2, Surfacer, bootgen/provisioning, OTA/cpio assembly and gobbl-lab fixtures together. Keep ROM identity/config/firmware views; remove duplicated module inventory/order/admission and inherited-FD paths. Update paths to ESP `/dev/esp/kernelsu-esp`, platform `/dev/esp/esu`, runtime `/dev/kernelsu-esp`.
9. Run the acceptance gates below, then perform the owner-selected integration/deployment. No push, remote rename or replacement of deployed payloads is implied by finishing this plan.

## Acceptance gates

These are future implementation checks, not results of this document edit.

- Replay patches against the pristine pin; build native userspace with matching Rust/NDK linker versions and the supported ACK/DDK matrix. For changed project Android Rust crates: `cargo ndk -t arm64-v8a check`, then `cargo ndk -t arm64-v8a clippy`, then `cargo fmt`; fix warnings/errors. Run affected existing host suites once at integration.
- Run the actual standalone builder, decode/extract LZ4/newc and compare payload bytes/modes/parameters, then boot that archive. Prove original `/init` and installed root's payloads/paths/sockets/policy remain intact; only our daemon dispatches our stages. Our control rejects non-root requests and exposes no app SU grant. Do not infer coexistence from namespace strings.
- Exercise each inner-phase barrier, module order, optional/critical failures, recovery/charger/safe mode, one-pass rc errors, early policy application, missing helper, unavailable LV and ambiguous source. Critical managed-bootstrap failure stops boot rather than reaching consumers.
- Load each shipped `.ko` and drive readiness/ABI operations, including ppconf successful/sealed commit, failure/correction, handle lifetime, duplicate/limit/RO errors; EFI mount/read/write before policy during rdinit and after `esp` credential refresh, provider present/absent, native EFI ownership and GKI/DKMS builds. Confirm native LVM mappings against the legacy fixture.
- Observe first-stage cleanup and second-stage reconstruction in Android: same source/profile/generations, no live upper/work overlap, one final RW ESP, executable staged tools, efivarfs and HAL ready before consumers/post-fs-data. Confirm module copy-up labels and projected writes on an enforcing boot.
- Exercise module install/update/disable/uninstall; two ROMs' overrides/config/preferences remain separate, plus deliberate shared-profile behavior. Test changed lower with copied-up content, whiteout and opaque directory: fresh generation exposes new package, old edits remain recoverable, other uppers/durable state remain intact. Verify projected module content still masks Android OTA changes until updated/disabled.
- Interrupt package activation between directory operations and recover to matching lower/upper pairs. No mutation behind active overlays, discarded edits or automatic lower merge.
- Preserve all nine OTA rows: VG-reserve-ROM2-install, rdinit-idle-boot, missing-KMI-denial, cancel-staging, self-OTA-staging-sealed, target-staged-boot, kill-promote-resume, ESP-hash-LV-removal, second-idle-boot. Then ROM 1 enforcing phone-loop and staged ROM >= 2 phone gate. Record only observed results.

## Source anchors

- [Pinned Magisk source](https://github.com/topjohnwu/Magisk/tree/e8915d9db15f5aae93973ffe65068e34df375a6a); isolated mockup patch/replay/build evidence is reported separately, not committed as production implementation here.
- Legacy `userspace/esuinit/src/{handoff,loader,scripts}.rs`, `userspace/esud/src/{module,init_event}.rs`, `kernel/{infra/io_cred.c,selinux/selinux.c,runtime/esud_integration.c,hook/syscall_hook.c}`: port sources for handoff, loading, dispatch, RC/policy and credential lifetime.
- `scripts/kmi_modules.py`; sibling `efivar-store/linux/efivar_store.c` and `gobbl/docs/storage/variables.md`: exact artifact, selector/GUID and EFI contracts.
- [OverlayFS contract](https://docs.kernel.org/filesystems/overlayfs.html); [pinned metamodule](https://github.com/Tools-cx-app/meta-magic_mount-rs/tree/bfa551f61b010709e0ff5567eaae58ce1a8cfd2d): update/copy-up semantics and project-owned path/RO-remount changes.
