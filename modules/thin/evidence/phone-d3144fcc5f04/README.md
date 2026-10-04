# Historical phone proof evidence: `thinpool-private-d3144fcc5f04.ko.zst`

**This is pre-import evidence, not a build of this subtree.** It is kept so the
phone proof of the vendored dm-thin fork stays checkable after the import. The
module is compressed so a recursive kernel `make clean` cannot delete it.

## What is here

| File | SHA-256 | Content |
| --- | --- | --- |
| `thinpool-private-d3144fcc5f04.ko.zst` | archive: `2191811acf9e0c2ef3e0f45a3c5cb0d77f0516ee1c1c7334dac5e60d3914c200`; decompressed module: `d3144fcc5f049d09b952219e19d8ae837617fc27b60f8cfd0e28388aaca4b4d3` | frozen loadable module as it was built and loaded on the phone; recover with `zstd -d` |
| `thinpool-private-d3144fcc5f04.imports` | `7de9dec1013338783bf0ca71a4b33dc1e19d51732ef5c01b0162a305a002bdeb` | 151 undefined imports |
| `thinpool-private-d3144fcc5f04.modversions` | `145476b9e64a6a6b1c11a7acdb2eee69e69bfa88e8a0d3e4691a99ee1f12b5a0` | 152 rows including `module_layout` |

Build identity:

- ACK branch `android16-6.12`, commit `925a103d123c30a84577f29e1573376aacbde94b`
  (kernel `6.12.58`), matching phone release
  `6.12.58-android16-6-g925a103d123c-ab15898589-4k`.
- vermagic `6.12.58-4k-g925a103d123c SMP preempt mod_unload modversions aarch64`.
- Sources: the pristine ACK files with the frozen thin-fork changes applied, i.e.
  exactly the `.c`/`.h` set and `private-rename.h` that this subtree vendors
  (see `../../PROVENANCE.md` section 2), plus the proven entry point *before* the
  generation/ready changes of section 4.
- Configuration: the phone's `/proc/config.gz` with Rust disabled,
  `CONFIG_MODVERSIONS=y`, `CONFIG_GENDWARFKSYMS=y`, `CONFIG_TRIM_UNUSED_KSYMS=y`,
  built with `KBUILD_GENDWARFKSYMS_STABLE=1` and Android clang `clang-r536225`.
  Build `.config` SHA-256 `49f9314c7b2e3e3ea7953cddce7ed9572e7263d3068f9a4abaec4ecd745b6a7c`;
  exact-build `Image` SHA-256 `1236b441a68643ed37836812c7e0ea61aa951145043f8c5f638af4436a83b6b8`.

## Recorded results

QEMU gate against the exact-build `Image`
(proof record `records/20260929T034940Z-phone-build/record.md`):

- `PROOF kernel=6.12.58-4k-g925a103d123c` and the artifact SHA-256 above.
- `PROOF insmod_rc=0`, then `thin-pool v1.23.0` and `thin v1.23.0` between the
  target markers. Clean guest poweroff, only the normal out-of-tree-module
  taint, no module warning or oops.

Phone run on device `3C15AT003ZB00000`
(proof record `records/20260929T035316Z-phone-ram3/record.md`):

- read-only corpus cross-check: 124 confirmed imports, 124 matches, 0
  mismatches, 27 imports unconfirmed by the corpus and left to the kernel
  MODVERSIONS load gate.
- MODVERSIONS gate: `insmod` returned 0; `dmsetup targets` lists `thin-pool`
  and `thin`.
- RAM-only tables `tp_meta`, `tp_data`, `tp_pool` and `tp_userdata` were created
  over the approved `rawdump` range; vold measured the thin mapper, generated a
  wrapped storage key and stored it in the tmpfs keydirectory.
- vold then returned service error 25 creating its `dm-default-key` device
  (`vold: Could not create default-key device userdata`), so **dm-default-key
  stacked over the thin device: NO**, and filesystem format/mount, file
  write/read, trim and physical-map proof were **not reached**. This is a
  default-key composition rejection after all thin table construction served,
  not a MODVERSIONS failure.
- Teardown restored the stock fstab bind and removed all proof mappings, the
  module and the keydirectory (`post-state.txt`: `vold=running`, `module=absent`,
  `mappers=absent`, `keydir=absent`).

Related arm64 proof of the same sources: signed `insmod` and both targets on ACK
`f1bdb13583da85a47fcf1632a78ef52d6e6da651` (`records/20260929T020200Z-stage2b`,
`records/20260929T030000Z-stage6`), the 151-import forbidden-symbol audit
(`records/20260929T030000Z-stage6`: forbidden-import check **PASS**) and the
dm-thin behavior matrix (`records/20260929T030100Z-stage7`,
`records/20260929T034000Z-stage7b`).

## What this evidence does not prove

- It is **not** the current ABI: the artifact loads as module name
  `thinpool-private` and exposes no `generation` and no `ready` parameter. The
  espinit self-check (`userspace/ksuinit/src/selfcheck.rs`) cannot pass against
  it, and it does not prove the generation validation/injection added on
  import.
- It is not a build of the current subtree: the current sources add
  `thin-main.c` (generation/ready), `Makefile`, `build.sh` and the renamed
  `thin.ko` output. Only the vendored `.c`/`.h` files and `private-rename.h` are
  byte-identical.
- It is not a `thin.ko`: it was not renamed and is not loaded under the manifest
  name `thin`.
- It is not a Cuttlefish-first-stage or a boot-completion proof, and the earlier
  attempts in the same record (dynamic `dmsetup` loader, mapper SELinux label)
  failed for runner reasons, not for module reasons.
- The separate exact Cuttlefish artifact built during the historical proof was
  not imported here. Its documented identity is ACK `3ec022196c4e`, published
  build `15076761`, `module_layout` CRC `0xda0486d9`.

The records named above identify the historical proof stages. The source,
fork patch, import manifests, frozen phone artifact and result summary needed to
audit this repository are retained here; no local proof-workspace path is part
of the public contract.
