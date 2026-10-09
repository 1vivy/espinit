# KernelSU ESP: userspace patchset, rdinit and native LKMs

## Decision and scope

Use pinned Magisk native userspace plus a small product patchset as the lasting ESP module and lifecycle base. Keep the existing rdinit entry and ordinary, standalone LKMs. The current checkout is the port source, not the new upstream base. Traditional app SU is not a product goal, but eliminating upstream SU is not a goal either: retain upstream machinery unless separation or our actual lifecycle requires a change. Do not build an app-root replacement, root-provider negotiation or SU handoff.

This is an implementation plan. The isolated Magisk mockup tests the size and feasibility of the product patches; it is not a deployed or boot-qualified stack. This revision does not rename repositories, deploy payloads or authorize device writes.

Three repositories retain the agreed names:

| Repository | Owns |
| --- | --- |
| `kernelsu-esp` | Pinned native userspace, ordered patches, rdinit/stage runner, standalone cpio builder, small kernel helper |
| `kernelsu-esp_modules` | Installed ESP modules, platform/storage helpers, Android CLIs and packaging |
| `kernelsu-esp_lkms` | Independent thin, partition-projection, efivarfs and efivar_store LKMs, their ABIs and ACK/DDK builds |

Main-repo storage preparation accepts an opaque source/backing descriptor from a supplied helper. Physical metadata provides shared writable backing; there is no module-storage LV or per-ROM module profile. `gobbl-runtime` owns Android-critical shared metadata handling and boot corrections. LVM, EFI-variable protocols, ROM manifests, HAL/OTA and BCB stay downstream in the modules/platform repositories. Capacity, provisioning and space policy are downstream responsibilities, not requirements of this rework. Preserve legacy provenance when splitting history; update every caller in the cutover, with no compatibility aliases.

## Small upstream patches

Pin Magisk `e8915d9db15f5aae93973ffe65068e34df375a6a` under `vendor/Magisk/`. Keep that pin pristine, with one ordered series under `patches/Magisk/`; materialize patched build sources separately. Apply patches exactly, with no fuzz or offset. Preserve licenses and upstream source naming where it is not a runtime identity.

The product patch surface is limited to:

1. **Separate the product, not rewrite SU.** No Manager/APK deliverable or traditional-consumer SU integration is required. Do not spend patches stripping native SU/authorization code merely to prove it absent, and do not turn retained code into an unauthenticated grant path. Preserve upstream authorization for retained requests; product module/stage operations remain root/init-only. Retain SELinux application with the product's `esp` identity and rules needed by its actual consumers. No blanket permission exception or global denial of other policy loaders.
2. **Rewrite owned paths and namespace.** Ship the daemon as `ksud`; use domain `esp`, file types `esp_file`/`esp_log_file`, and distinct control/socket/service names. Module/update/state/config roots use the one shared metadata-backed view. Executable/helper-script paths resolve to staged `/dev/kernelsu-esp/bin`, never the noexec FAT package bin. Migrate C++/Rust constants and installer paths together. Keep internal CLI applets available under the product's staged paths without automatically claiming the system's ordinary `su` entrypoint.
3. **Port rdinit; add small inner stage points.** Magisk's existing serialized dispatcher starts at post-fs-data and gates on `/data`; do not fake that event before userdata. Add the early entry points and small ordered phase loop to the same product lifecycle, reusing the existing script runner and later dispatch rather than maintaining parallel daemons. Projection uses Magisk's built-in magic mount (`core/module.rs`); there is no metamodule and no second projector.

The product is selected at build time and explicitly entered through rdinit. It does not need a Manager toggle or a second stock/ESP backend. Merely renaming a daemon is insufficient: migrate self-exec paths, sockets, rc commands, script environments, mounts, policy types and installer paths together.

Separation is the contract: own our paths, socket, policy identity and stages, and execute the original `/init` after rdinit. Do not appropriate an installed root's artifacts or ordinary SU entrypoint. There is no provider detection/selection, SU forwarding/handoff, alternate backend or coordinated foreign-root lifecycle. A working lasting product comes first; neither traditional SU support nor proof that all native SU code is disabled is an acceptance gate.

## Kernel helper, not kernel root

Reduce the legacy core to the mechanisms the userspace framework still needs:

- Existing one-pass init-rc supplement and root-only SELinux policy application/cache refresh.
- No credential provider: backing-store I/O credentials belong to the `efivar_store` LKM (below).
- Required native-module symbol machinery and existing syscall-wrapper ownership/lifetime handling.

Build it as **`kernelsu-esp.ko`**, internal name **`kernelsu_esp`**. Migrate artifact, loader and cpio names together. Retain a distinct control FD/install identity; reusing another root's version probe is not helper discovery. Do not add `/dev/ksu` or another driver transport.

Remove app SU, Manager/profile/allowlist machinery, SU compatibility and daemon/app credential-grant logic. Retain the existing ownership-checked `execve`/`execveat` observers **only** for second-stage init policy readiness, plus read/fstat hooks for rc delivery. They call saved originals and do not replace paths, alter arguments or grant caller credentials; remove `setresuid` root-grant interception. Pin published callbacks for the boot; never force-unload or restore over a foreign owner.

Preserve the rc supplement's root permission, pointer/length/reserved validation, 64 KiB bound, one supply, `EALREADY` on repeat supply and `EBUSY` after consumption. The helper is not a root backend: boot mode, admission, safe-mode inputs and stage completion live in the userspace boot context, not kernel-root reports.

Keep the existing second-stage exec observation from the recorded coexistence repair (`60a16cf7`): it applies bounded base `esp` declarations after Android loads policy, before an `esp`-labelled rc command runs. Do not remove that observer while leaving its policy initialization behind. Module rules are applied once by the later bootstrap, before their services start; normal app/zygote execs do not drive our lifecycle.

**Backing-store I/O credentials live in `efivar_store`, not the helper.** Its `kernel_read`/`kernel_write`/`vfs_fsync`/`fput` otherwise run as the caller (for example a HAL) and hit an `fd use` denial on a file opened by PID 1. Bracket exactly that I/O with `override_creds(store_file->f_cred)`/`revert_creds`: the opener's credential, an exported API, no private symbols, no provider ABI, no `esp` credential, no `symbol_get`/`symbol_put`. Remove `efivar_store_io_enter/leave` and the legacy `io_cred.c`. Open the store during rdinit, before policy. Policy must then let the opener's domain access the store block device; module rules supply it. UID 0 alone does not solve SELinux backing-file checks.

Retain the existing userspace ELF prebinding loader for private vmlinux imports. Use in-module runtime resolution for private calls that need it, without importing another root runtime. Vendor modules use normal `finit_module`, Android module-list ordering, dependencies, softdeps and options. Symbol resolution does not repair incompatible layouts, CFI, CRCs or signatures.

## Installed layout and writable views

```text
ESP/
  EFI/                                    firmware assets, unchanged
  kernelsu-esp/
    config.toml                           source/backing configuration
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

physical metadata: existing Unix-capable filesystem, shared across boots/ROMs
  password_slots/                         existing shared Android slot-map state
  kernelsu-esp/
    overlays/<id>/<package-generation>/{upper,work}
    modules/<id>/                         writable local installs
    modules_update/<id>/                   local install staging
    state/<id>/                           durable shared module state
    preferences/                          disable/remove independent of generation
    ...                                   daemon database/log/config/script roots

final runtime:
  /dev/esp                                one RW raw ESP mount
  /dev/kernelsu-esp/bin/                   executable tmpfs tools
  /dev/kernelsu-esp/source                 selected boot descriptor
  /dev/kernelsu-esp/store/                 bind of metadata's kernelsu-esp directory
  /dev/kernelsu-esp/modules/<id>/          RW package overlays or local binds
  /dev/kernelsu-esp/state/<id>/            shared durable-state binds
```

Mount the raw ESP RW with `nosuid,nodev,noexec`; do not restore it to RO after writes. Copy early executable files to tmpfs. FAT cannot supply Unix permissions, symlinks or per-file SELinux labels.

Use the existing physical metadata volume as the writable backing for the whole module framework, not a new LV or filesystem image. The supplied bootstrap helper exposes its owned `kernelsu-esp/` subtree at `/dev/kernelsu-esp/store` before daemon startup. Native secure/update/database/log/common-script roots use that subtree. Adopt the actual filesystem and its existing Android ownership; the current shared-store implementation mounts F2FS, so do not assume ext4 or format during boot.

Use **one module-tree OverlayFS per ESP package**, with upper/work on the same suitable metadata filesystem and distinct pairs per module/package generation. Local installs are writable metadata directories, not additional lowers. A module ID cannot simultaneously belong to ESP and local storage. The entire volume is the backing resource, not literally one `upperdir=/metadata`: existing keys, password slots and other Android files remain outside the owned upper/work trees, without bulk relabelling or relocation.

There is one shared module set and one shared set of overrides, local installs, preferences and durable state. No profile selector, per-ROM module view or shared-versus-private mode. ROM identity may still be needed by downstream Android credential/boot handling; it does not select module storage. No LV activation, allocation, fallback or storage-capacity policy belongs to this contract.

An updated lower does not merge with copied-up files, whiteouts or opaque directories. Stage updates, activate only before mounting, and select a **fresh upper/work for a changed package generation**. Unchanged modules keep their uppers. Preserve the old lower/upper pair for rollback; do not carry OverlayFS internals into the new pair. Durable state is outside generations; generation-local config requires explicit module migration or user recovery from the retained pair.

Record package activation so interrupted old-to-previous/new-to-installed renames can complete or roll back. Flush data/metadata before advertising a generation. FAT rename/fsync does not make the multi-directory change transactional. Never edit an active lower.

Use conservative OverlayFS options (`index=off,metacopy=off,xino=off,redirect_dir=nofollow` where supported). Prepare permissions/customization for new effective generations; after policy is available, copy up and label exports before consumers use them. Persist preparation completion only after all required preparation succeeds. Validate OverlayFS upper support on the actual metadata filesystem.

Use the daemon's built-in **magic mount** for Android `system/` projection; it is also upstream's idiom for injecting its own tools. Reuse that implementation, not a metamodule or a new projector. Split the projection portion from `core/module.rs::handle_modules`: that entry point also performs upgrades and runs `post-fs-data` scripts, which must not run early. Invoke projection from blocking second-stage bootstrap once target mounts, policy, labels and the effective module view exist, no later than `post-fs` and before projected HALs/services start. Remove its `/data` assumptions for this path and its projected-file read-only remount only for files backed by the writable effective tree; leave unrelated Android mounts read-only. Point module/config roots at the shared view and scope built-in tool injection to our product rather than ordinary system SU. Bootstrap creates the backing view before the projector reads it. Do not project again or repeat early work at post-fs-data; retain the proper later script event. Writes through projected files reach the module upper; durable new files belong in the module tree, not a temporary mount skeleton. This does not make untouched Android partitions writable or automatically reconcile their OTA updates.

## Ordering and stage transition

Installed directories plus matching `module.prop` IDs are the inventory. `modules_order` ranks discovered modules; append unlisted installed modules in stable ID order. Do not scan lab content implicitly. Preserve identifier/path hygiene, safe copies/symlinks, duplicate ownership errors and rc bounds; do not add a second approval inventory or repeated mount/context attestation.

Use the same admitted set for kernel objects, policy, rc, scripts and projection. Flags `critical`, `recovery_ok`, `disable`, `remove`, `skip_mount` accept `1`/`true`; absent/other values are false. Shared preferences remain outside lower generations. Retain recovery/charger/safe-mode behavior.

Inner stage points are a small ordered dispatch inside the relevant lifecycle stage: **phase outermost, `modules_order` innermost**. They provide barriers, not a dependency solver. Modules perform their own prerequisite checks. Physical-metadata bootstrap runs before module-view discovery; its required tools are independently staged bootstrap inputs, never accessible only through the overlays they must construct. Keep `rd/`, `rd_copy` and `early_bin` staging/collision semantics.

Boot sequence:

1. Enter `/esuinit` through `rdinit` (generic entry `/ksuinit`). Preserve original `/init`; set up `/proc`, `/sys`, `/dev`, classify boot mode and load vendor/critical ramdisk LKMs before ESP discovery. Critical helpers are cpio inputs, not hidden behind inaccessible ESP modules.
2. Resolve the selected ESP, report ambiguity, mount RW and stage tools on executable tmpfs. Run the supplied physical-metadata bootstrap, finish pending package activation and construct effective modules. Retain exact source/backing identity, generations and order in the boot descriptor; no ROM-selected module profile.
3. Load admitted module kmods and run ordered `rdinit.sh`/inner phases with bounded blocking execution (35 seconds, terminate and reap). Run from module directories with `KSU`, `KSU_MODE`, `KSU_STAGE`, `KSU_MODULE`, `MODDIR`, `KSU_MODULE_STATE` and the staged PATH. No shell evaluation of kernel parameters or lasting userspace daemons from rdinit. Optional failure removes the module before rc assembly; critical failure stops managed handoff.
4. Supply bootstrap rc followed by admitted module rc once, preserving `norc`; base policy initialization stays on the retained second-stage observer. Carry the selected descriptor through the actual rc/handoff channel, not a soon-to-disappear `/dev` file. Close script/cwd/FD references, fully unmount owned overlays, then release owned mounts in reverse order. Do not detach vendor mounts. Execute original `/init`, preserving PID, remaining argv and environment.
5. Android's root transition recreates `/dev` and `/sys`. Blocking bootstrap uses stock Android tools in the init context to recreate the **same selected** ESP, metadata-backed view and executable tmpfs; copy ksud, policy tool, Busybox and installer helper script from package inputs. Coordinate with Android's metadata mount through the supplied helper; do not mount the same filesystem independently under competing ownership. Only then launch product tools. Do not execute or relabel FAT, use detached tmpfs or start a daemon before its socket/log directories and base policy exist. `/debug_ramdisk/kernelsu-esp` is temporary staging, not a second permanent ESP mount.
6. Apply admitted module rules once through retained policy application, preserving foreign rules; restore efivarfs and complete labels before exports/services are usable. Bootstrap precedes module actions; product services stay disabled until owning-stage preparation succeeds. Optional failure cannot start a predeclared service. Fatal bootstrap errors invoke managed fatal handling, not merely return nonzero and let Android continue unmanaged.
7. Dispatch `early-init`, `init`, `early-fs`, `post-fs`, `post-fs-data`, `post-mount`, `service`, `boot-completed`. Module projection runs at the earliest stage where target mounts, policy, labels and the effective view exist, and no later than `post-fs`. `gobbl-runtime` completes each shared-metadata mount, property and credential-state correction before its Android consumer starts. Do not reapply declarations or repeat projection at post-fs-data. Projection precedes early HAL start where a HAL binary is projected.

`exec /init` itself does not destroy mounts; Android's root transition/cleanup does. Loaded LKMs, DM objects and referenced loop backends persist, but their pathnames do not. Fully unmount an old overlay before reusing its upper/work; `MNT_DETACH` with live references is insufficient. Rebuild the final view before consumers open it. Exactly one final raw ESP mount remains.

Failures report the actual operation and may write/flush a receipt to available RW storage. Generic code uses the existing fatal/reboot handling, with no BCB dependency and no silent RO, ephemeral-backing or foreign-root fallback.

## Independent LKMs and platform modules

Each LKM lives under `kernelsu-esp_lkms/kernel/<name>/` with standalone `Kbuild`, `Makefile`, sources, ABI docs and applicable `uapi/`. Release layout is decoupled from source. Reuse ACK/DDK aarch64/x86_64 lanes and `include/kernelsu_esp_lkm.h`: `KERNELSU_ESP_LKM_METADATA`, `KERNELSU_ESP_LKM_READY_PARAM`, `.modinfo` keys `kernelsu_esp_name/version/abi/critical`. Metadata is descriptive, not authorization.

- **Thin:** import the owned fork whole as `dm-thin-pool`, internal `dm_thin_pool`; preserve private renames, target versions, suspend/resume/gate and bufio changes. Do not convert this owned fork to an upstream patchset.
- **Part projection:** configfs ABI 3 at `/config/part-projection/{abi,format,state,last_error,commit,projections/,hide/}`. Projections have `dev`/`ro`; hidden backends use major:minor; commit takes `1` or `seal`. Preserve construction/apply/seal/destroy and capability checks. Idle/staged/active/failed transitions release acquired handles on failed commit, permit correction, and forbid active mutation. Preserve 128 projection/256 hide/36-byte-label limits, duplicates, invalid devices, RO and sealed-view errors. Remove old `/dev/gptctl` UAPI after migrating all clients.
- **EFI:** keep efivarfs upstream-close with the existing project-GUID patch; pin reconciled efivar_store for both GKI-LKM and DKMS. Mount **`/sys/firmware/efi/efivars`**; filesystem type is `efivarfs`. Acquire/create EFI sysfs objects with ownership-aware cleanup; never remove a native EFI tree. Directory presence is not a successful mount.

Use efivar_store's existing `partuuid=<unique-GPT-GUID>` or Surfacer's `/chosen/efivar-store,partuuid`; explicit `dev=<major>:<minor>` is mutually exclusive. Do not discover by `bdsvars` label or substitute a fixed unique GUID. Type GUID `4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709`, EFI vendor namespace `7a5e4b1c-0d3f-4e62-9b8a-1c2d3e4f5a6b`, configuration-table GUID `930e89ed-540e-4af0-9b41-c2c559939d50` and selected PARTUUID are different identifiers.

Generalize `kmi_modules.py` to explicit module directory/name, branch, exact generation and architecture. Preserve ET_REL/architecture, internal names, vermagic flags, exported CRCs, private relocation imports and receipt/artifact matching. `--kmi` selects build artifacts, not a new assessment framework; ambiguous branch-only selection is an error. Load the exact shipped `.ko` and propagate real readiness/ABI errors.

The modules repository ships:

- **`gobbl-runtime`: Android-critical shared metadata and boot corrections.** Port the existing `userspace/esud/src/rom_isolation.rs` behavior here, not into a generic `gobbl-metadata` storage utility: expose the physical metadata backing, bind the shared `password_slots`/AOSP `slot_map` at the Android-visible location, and preserve required permissions, labels, property ordering and gatekeeper first-boot handling. Preserve ROM 1's `host` slot-map ownership and numbered GSI identities for subsequent ROMs; this Android credential identity is distinct from the removed module profiles. Preserve the existing prevention of shared-key deletion and the narrow TEE/RPMB access rules. Supply the minimal backing bootstrap independently of its own effective module view, then run the module's Android corrections at their owning phases before vold, credential services or other affected consumers. Missing required shared state stops managed boot; do not silently replace/reset keys or invent a new key format. The physical store also backs module uppers, but module cleanup must never traverse Android credential state.
- Native LVM bootstrap/static bionic tools (`rd/`, later `system/bin/`) remain downstream only where platform/ROM storage needs them; they are not a prerequisite of the metadata-backed module view. Preserve `system/etc/lvm/lvm.conf`, scan/udev/lock settings, existing-storage `vgchange -ay`, DM names/UUIDs/tables and legacy replica-PV fixture equivalence before retiring thin-activate. Verify `thin_check_executable` semantics for the pinned LVM release.
- `ppconf` at `system/bin/ppconf` plus ramdisk copy: version/status/ls, add name major:minor [--ro], hide major:minor, commit [--seal], abort. Mount configfs when required; success requires active state. CLI sources/releases belong here, not inside an LKM directory.
- `dmtricks`, `gobbl-multi-os`, `gobbl-ota`, `gobbl-boot-hal`: storage activation, EFI ROM selection, OTA staging, firmware views and projection in explicit inner-stage order. Keep shared DM tables/UUIDs and live switch-device reload semantics. Preserve `esu-bootctl` wire/state behavior, separate `esu_bootctl` policy and early executable copy. Remove `/dev/block/esd` only after all callers migrate.
- Shared Rust crates (`dm`, `ota-core`, `esu-vars`) as gobbl-owned Git dependencies pinned by full revision. Preserve EFI wire formats; use pinned efivar-rs plus the empty-standard-efivarfs fix. Platform/lab watchdog and any BCB operations remain explicit module-owned payloads, not generic boot dependencies.

## Implementation order

Each task ends in a focused commit after its affected checks. The mockup informs patch boundaries; do not mistake it for completion of the later integration tasks.

1. Freeze only agreed legacy work; record/tag actual provenance. Coordinate the three repository splits and names without rewriting historical paths. Import the recorded generic-bootctl revision with history; remote archiving/renames remain owner-controlled.
2. Pin upstream and replay the minimal native userspace product patches. No Manager/APK deliverable; do not add an SU-removal project. Establish owned daemon/path/socket/policy identities, preserved authorization and root/init-only stage control before adding ESP orchestration.
3. Reduce/rename the kernel helper; port the existing rc/policy handoff. Move backing-file credential handling wholly into `efivar_store` as specified above, removing the obsolete provider ABI and callers. Remove legacy kernel-root hooks and reports in the same cutover.
4. Port rdinit loading, source/backing bootstrap, small inner-stage dispatch and original-init handoff. Extend the serialized userspace dispatcher for pre-/data stages; replace foreign-install/setup/preinit assumptions with the selected descriptor and admitted rules. Wire rc, base-policy observation and second-stage module policy/labels/services in the order above. Split early built-in magic mount from later post-fs-data scripts; product controls must not create or migrate another installation's files.
5. Implement one shared metadata-backed effective module view, shared preferences/state, generation replacement/rollback and upstream module ZIP install/update/disable/remove/uninstall against those roots. No per-ROM module profiles, module-storage LV or copy into `/data/adb/modules`. Package disable/uninstall applies to the shared set. Keep Android credential state outside module lifecycle operations.
6. Split/build the independent LKMs and migrate projection/EFI consumers to the agreed ABIs. Implement module templates and CLI/tool packaging, including `gobbl-runtime`'s independent metadata bootstrap and Android shared-state corrections, then platform stage ordering and early HAL startup.
7. Add standalone `ksud --build-cpio --legacy-lz4 --kmi android16-6.12 --arch aarch64 --artifact-dir ./release --bootstrap-dir ./bootstrap --out ./takeover.cpio.lz4`. Emit newc (legacy LZ4 when requested), selected `/ksuinit` or explicit platform `/esuinit`, `/kernelsu-esp.ko`, product-only config, critical LKMs/tools and exact receipts. No `/init`, stock `/ksu_config`, patched boot image, implicit download or generic ROM/LVM/BCB dependency. Platform packaging supplies bootstrap inputs; gobbl/lab owns final image assembly. This command is not yet implemented.
8. Reconcile schema v2, Surfacer, bootgen/provisioning, OTA/cpio assembly and gobbl-lab fixtures together. Keep ROM identity/config/firmware views; remove duplicated module inventory/order/admission and inherited-FD paths. Update paths to ESP `/dev/esp/kernelsu-esp`, platform `/dev/esp/esu`, runtime `/dev/kernelsu-esp`.
9. Run the acceptance gates below, then perform the owner-selected integration/deployment. No push, remote rename or replacement of deployed payloads is implied by finishing this plan.

## Acceptance gates

These are future implementation checks, not results of this document edit.

- Replay patches against the pristine pin; build native userspace with matching Rust/NDK linker versions and the supported ACK/DDK matrix. For changed project Android Rust crates: `cargo ndk -t arm64-v8a check`, then `cargo ndk -t arm64-v8a clippy`, then `cargo fmt`; fix warnings/errors. Run affected existing host suites once at integration.
- Run the actual standalone builder, decode/extract LZ4/newc and compare payload bytes/modes/parameters, then boot that archive. Prove original `/init` handoff, product-owned artifacts/paths/socket/policy and exactly-once product stage dispatch. Stage controls reject non-root requests; retained upstream requests retain authorization. No SU-provider negotiation or handoff, traditional-consumer SU compatibility gate or requirement to prove native SU code absent.
- Exercise each inner-phase barrier, module order, optional/critical failures, recovery/charger/safe mode, one-pass rc errors, early policy application, missing helper, unavailable physical metadata and ambiguous source. Critical managed-bootstrap failure stops boot rather than reaching consumers. Prove the module view boots without module-storage LV activation.
- Load each shipped `.ko` and drive readiness/ABI operations, including ppconf successful/sealed commit, failure/correction, handle lifetime, duplicate/limit/RO errors; EFI mount/read/write during rdinit and again from a HAL domain under enforcing policy after Android loads it (no `fd use` or block-device denial), with native EFI ownership and GKI/DKMS builds. Independently preserve native LVM mappings against the legacy downstream fixture.
- Observe first-stage cleanup and second-stage reconstruction in Android: same source/backing/generations, no live upper/work overlap or competing metadata mount ownership, one final RW ESP, executable staged tools, efivarfs and HAL ready before consumers/post-fs-data. Confirm upper support on the real metadata filesystem, module copy-up labels and projected writes under enforcing policy. Prove early magic mount does not run post-fs-data scripts, which run once at their proper event.
- Exercise module install/update/disable/uninstall across ROM boots using the same shared modules, overrides/config/preferences and durable state. Test changed lower with copied-up content, whiteout and opaque directory: fresh generation exposes new package, old edits remain recoverable, other uppers/durable state remain intact. Verify projected module content still masks Android OTA changes until updated/disabled.
- Interrupt package activation between directory operations and recover to matching lower/upper pairs. No mutation behind active overlays, discarded edits or automatic lower merge.
- Exercise `gobbl-runtime` on ROM 1 and a subsequent ROM: shared password-slot mount and labels, `host`/numbered-GSI slot-map identities, shared-key deletion prevention, gatekeeper first-boot ordering and PIN unlock under enforcing policy. Module install/update/uninstall must leave shared credential state intact. No boot-success claim from mounts alone; observe the credential consumers.
- Preserve all nine OTA rows: VG-reserve-ROM2-install, rdinit-idle-boot, missing-KMI-denial, cancel-staging, self-OTA-staging-sealed, target-staged-boot, kill-promote-resume, ESP-hash-LV-removal, second-idle-boot. Then ROM 1 enforcing phone-loop and staged ROM >= 2 phone gate. Record only observed results.

## Source anchors

- [Pinned Magisk source](https://github.com/topjohnwu/Magisk/tree/e8915d9db15f5aae93973ffe65068e34df375a6a); isolated mockup patch/replay/build evidence is reported separately, not committed as production implementation here.
- Legacy `userspace/esuinit/src/{handoff,loader,scripts}.rs`, `userspace/esud/src/{module,init_event,rom_isolation}.rs`, `kernel/{infra/io_cred.c,selinux/selinux.c,runtime/esud_integration.c,hook/syscall_hook.c}`: port sources for handoff, loading, dispatch, RC/policy, removal of the credential-provider ABI and `gobbl-runtime` shared-state behavior.
- `scripts/kmi_modules.py`; sibling `efivar-store/linux/efivar_store.c` and `gobbl/docs/storage/variables.md`: exact artifact, selector/GUID and EFI contracts.
- [OverlayFS contract](https://docs.kernel.org/filesystems/overlayfs.html): update/copy-up semantics. [Pinned Magisk module implementation](https://github.com/topjohnwu/Magisk/blob/e8915d9db15f5aae93973ffe65068e34df375a6a/native/src/core/module.rs): built-in magic mount, internal tool injection, read-only bind remount and the post-fs-data work that must be separated from early projection.
