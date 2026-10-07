# Vendored esu-config provenance

`userspace/esu-config` is a pinned copy of `crates/esu-config` from gbl-bds-rs.
Its baseline is commit `e028394530f798766bd81bcc0fc14b72afe26cac` (branch
`sesori/execute-esp-kernelsu-plan`, "esu-config, rom-catalogue: shared
installed schema and BLS/ROM catalogue"). The AVB metadata graft cutover updates
`src/schema.rs` and `tests/schema.rs` from the canonical working tree; the
content hashes below identify this updated source without claiming a new commit.

The schema code was authored in this repository as
`userspace/esuinit/src/config.rs`; the owner approved publishing it as
Apache-2.0 in gbl-bds-rs and vendoring it back as this shared crate. The `src/`
and `tests/` trees are byte-identical to the source crate (`diff -r` is empty);
only the manifest is adapted to espinit's workspace.

| file | sha256 |
| --- | --- |
| src/installed.rs | d32dae2716a81d51eb5428272e88f626f0505b6585d590292f2ac1cc894d8a12 |
| src/lib.rs | f81e4f8375c89dcbc8b18c71e554e9fc777901b51af312005a866cf669453711 |
| src/schema.rs | 6fde39f36039c20a2a6c4502571f3266055af8bfac51195494eed0d5b3c07a9a |
| tests/installed.rs | 832780037e7ad2660739c3a15feffd32742417ca528c8b3bd93b76ee3f78fb58 |
| tests/schema.rs | c9fb833e71948de03a216dd8cd35db211384749e9ffdd1489c0cb361237aa028 |
| tests/fixtures/mod.rs | 16a9282a22f644dfaa6f5bc5710025baaae77cfc6c8989e3847659e66fe9edad |

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

## Deviations from the source manifest

`Cargo.toml` differs from gobbl only in workspace adaptation:
`edition.workspace = true` (espinit's edition) and `license = "Apache-2.0"`
instead of the inherited workspace fields, and no `[lints]` section because
espinit's workspace declares none. The serde and toml requirements are already
resolved by espinit's lock set, so the vendored crate adds no new dependency.

## What stays in esuinit

The Linux side is not vendored: backend resolution (`validate_backends`, the
resolved `OnceLock` state, `has_writable_esp_file`, `partition_modes`), the
ESP-file lifecycle refusal, the efivarfs identity, and the compile-time
assertions that this crate's limits equal `gpt_uapi`'s remain in
`userspace/esuinit`.
