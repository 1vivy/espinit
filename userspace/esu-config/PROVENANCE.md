# Vendored esu-config provenance

`userspace/esu-config` is a pinned copy of `crates/esu-config` from gobbl,
commit `344617cc4f6aa36a4c65dfe04eb389dda0b68223` (branch `storage/efvs`,
"feat: make Surfacer own EFVS initialization and variable access").

The schema code was authored in this repository as
`userspace/esuinit/src/config.rs`; the owner approved publishing it as
Apache-2.0 in gobbl and vendoring it back as this shared crate. The `src/`
and `tests/` trees are byte-identical to the source crate (`diff -r` is empty);
only the manifest is adapted to espinit's workspace.

| file | sha256 |
| --- | --- |
| src/installed.rs | f226efa953a2158a52958e256b0e2dc12ca02f1ca9bdcb14f111d991d5d9cc79 |
| src/lib.rs | f81e4f8375c89dcbc8b18c71e554e9fc777901b51af312005a866cf669453711 |
| src/schema.rs | e6bb638a75d65f2a6511e5796b1d1d70c24633edf28e1227a3681da53b5e48e5 |
| tests/installed.rs | d19a2583b3e81ae723d821b448cf29a2af029d27fa5ee5b4aa9571f596999769 |
| tests/schema.rs | 6a52c0bdcb5c24f0479267287d2f12449e5e933e1bd13bf2f5fc047148a302bb |
| tests/fixtures/mod.rs | 48a1038cef913e29d64960cfbb84b0e107fb9f8e3e25cc34411cfaf9bba2a2c8 |

From the gobbl worktree, check canonical source identity with:

```sh
diff -r crates/esu-config/src /home/vivy/Projects/efisp-projects/kernelesp/userspace/esu-config/src
diff -r crates/esu-config/tests /home/vivy/Projects/efisp-projects/kernelesp/userspace/esu-config/tests
sha256sum crates/esu-config/src/schema.rs crates/esu-config/tests/schema.rs
```

`PartitionEntry.metadata` selects a safe ESP-root-relative `.vbmd` seeded when
a backing is initialized. ESP-file contents are grafted during provisioning;
runtime preserves current contents. Mapper metadata is admitted only for that
ROM's configured firmware view; an empty view can be seeded, while existing COW
contents win. Direct physical/loop backends and unslotted/raw vbmeta entries are
rejected. Other slotted images are admitted subject to runtime footer geometry
and metadata validation by the shared `avb-graft` crate.

This pin adds optional bounded ROM `number` for Surfacer's first-boot Slot
initialization; an existing Slot record remains authoritative. It also makes
canonical gobbl retain the three-module bootstrap introduced by kernelesp's
`3c6cadc9`: `kernelesp` and `efivarfs` with empty parameters, then
`efivar_store` with `dev=by-name:bdsvars`. The DT PARTUUID is the backend's
default for loaders that pass no explicit device. No payload declarations or
EFVS `SOURCE_REVISION` are changed by this schema synchronization.

## Deviations from the source manifest

`Cargo.toml` differs from gobbl only in workspace adaptation:
`version = "2.0.0"`, `edition.workspace = true` (espinit's edition), and
`license = "Apache-2.0"` instead of gobbl's version and inherited workspace
fields, and no `[lints]` section because espinit's workspace declares none.
The serde and toml requirements are already
resolved by espinit's lock set, so the vendored crate adds no new dependency.

## What stays in esuinit

The Linux side is not vendored: backend resolution (`validate_backends`, the
resolved `OnceLock` state, `has_writable_esp_file`, `partition_modes`), the
ESP-file lifecycle refusal, the efivarfs identity, and the compile-time
assertions that this crate's limits equal `gpt_uapi`'s remain in
`userspace/esuinit`.
