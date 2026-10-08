# ota-core

**Status (2026-10-08)** — shared OTA library for the staging transaction. The
library half of the ROM OTA work is complete and host-tested; the `ota-stage`
PID-1 helper and the boot HAL's transaction module are the next consumers and
are not part of this crate.

`ota-core` is the one place the OTA transaction's shared vocabulary is derived,
so the four programs that take part in it cannot drift apart:

| consumer | uses |
| --- | --- |
| boot HAL (`esu/modules/boot-hal/layer`) | staging LV and switch device names, `exact_sectors`, `copy_range`, `verify_equal`, `kmi_from_boot`, `arb::scan`, `abl_has_efisp`, `select_module_set`, `build_overlay` |
| `ota-stage` (PID 1 helper) | `ESU_STAGE` decoding through `esu-platform`'s record, the switch device names, `dm::DeviceMapper::create`/`reload` |
| esud | `esd` node paths, `POOL_TDATA`, `build_overlay` (the takeover archive), `legacy_lz4` |
| esuinit | the switch device names esuinit resolves for the staged letter, and the payload paths |

Nothing in the crate touches a device, a mount or an efivarfs variable: every
function is pure (names, the overlay, the ARB and KMI scans) or takes the file
handles and resolved paths the caller already opened.

## Layout

| module | contract |
| --- | --- |
| `names` | VG `rom`, LV `rom<n>-stage-<base>`, its dm name `rom-rom<n>--stage--<base>`, per-base switch `rom<n>-ota-<base>`, the `/dev/block/esd/` node tree (`pv/a`, `by-name/`, `lv/`, `mapper/`, `lock`, `run`, `etc`), payload paths `rom/<id>/esu.stage.cpio` and `rom/<id>/esu.cpio` |
| `copy` | `exact_sectors`, `copy_range` (1 MiB chunks, in place, one `sync_all`), `verify_equal` |
| `kmi` | `Kmi { branch, generation }` and the `(\d+\.\d+)\.\d+-(android\d+)-(\d+)` banner read out of a boot image's kernel block |
| `arb` | the `xbl_config` OEM anti-rollback triple, ported from the MIT `arbscan` |
| `abl` | the `efisp` needle in the extracted ABL LinuxLoader, ASCII or UTF-16LE |
| `modules` | `esu/kmi/<branch>-<generation>/set.json` selection with per-module SHA256 |
| `switch` | the two tables `rom<n>-ota-<base>` serves: `linear` over the staging LV, `error_target` while nothing is staged |
| `overlay` | the legacy-LZ4 newc takeover archive: `esuinit`, `esu-build-id`, `lib/<name>.ko` |

The `Stage-<id>` transaction record itself is **not** here: it lives in
`esu-platform` (`esu_platform::stage`, `efivars::stage`, `efivars::write_stage`,
`efivars::selected_slot`) next to the other efivarfs records, because both the
Linux side and the UEFI side read those bytes.

## Signatures chosen where the plan left them open

- `exact_sectors(image: &Path)` takes the **resolved** base-image path, not
  `(esp_mount, id, base)`. The wave-2 slice that adds
  `esu_config::base_image_path(id, base)` joins it to the ESP root; this crate
  never has to know the mount or the ROM directory layout. A missing, empty or
  non-4096-multiple image is an error, because every later length (LV creation,
  prefill, promote, switch table) is derived from this one answer.
- `build_overlay(esuinit, modules, build_id) -> Result<Vec<u8>>`: the plan wrote
  `-> Vec<u8>`, but the newc writer and the LZ4 encoder can both fail, and a
  silently empty archive would be worse than an error the caller must handle.
  `modules` maps a full member path (`lib/<name>.ko`) to bytes, which is what
  `ModuleSet::payload_members()` returns. The `lib` directory entry precedes its
  files so the kernel's extractor never has to create a parent directory.
- `select_module_set(payload_root, kmi) -> Result<ModuleSet>`: the plan wrote
  `-> PathBuf`, but the seal step needs the verified module list as well as the
  directory, so the set carries both. `ModuleSet::payload_members()` hashes every
  file again: selection and use are separate steps, and a set that changed in
  between must fail the seal instead of being written into a staged payload.
- `arb::scan(&[u8]) -> Option<Arb>` and `abl_has_efisp(&[u8]) -> bool` are pure
  functions over bytes. `abl_has_efisp` reports `false` when the LinuxLoader
  cannot be extracted: the ROM 1 carry-over copy is the safe outcome, and one
  unnecessary partition write is cheaper than a slot whose firmware cannot reach
  `efisp`.
- `dm::DeviceMapper::reload(name, targets, read_only)` loads the new table, then
  suspends and resumes, so the swap happens at the resume. `create` accepts an
  already identical table, so the boot-time helper is idempotent on a re-run.

## Verification

```sh
cargo test -p ota-core -p dm -p esu-platform -p abl-image
cargo clippy -p ota-core -p dm -p esu-platform -p abl-image --all-targets -- -D warnings
ANDROID_NDK_HOME=<ndk-r29> cargo ndk -t arm64-v8a check -p ota-core
```

The overlay round-trip decodes the legacy-LZ4 stream and parses the newc members
by hand, asserting the exact member set and modes; the KMI test builds a
synthetic boot image carrying a `Linux version 6.12.23-android16-6-…` banner; the
ARB test builds synthetic ELF64 images and checks every reject.
