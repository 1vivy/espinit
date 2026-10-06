# Vendored esu-config provenance

`userspace/esu-config` is a pinned copy of `crates/esu-config` from gbl-bds-rs
commit `e028394530f798766bd81bcc0fc14b72afe26cac` (branch
`sesori/execute-esp-kernelsu-plan`, "esu-config, rom-catalogue: shared
installed schema and BLS/ROM catalogue"). Every hash below was checked against
`git show e028394:crates/esu-config/<file>`.

The schema code was authored in this repository as
`userspace/esuinit/src/config.rs`; the owner approved publishing it as
Apache-2.0 in gbl-bds-rs and vendoring it back as this shared crate. The `src/`
and `tests/` trees are byte-identical to the source crate (`diff -r` is empty);
only the manifest is adapted to espinit's workspace.

| file | sha256 |
| --- | --- |
| src/installed.rs | d32dae2716a81d51eb5428272e88f626f0505b6585d590292f2ac1cc894d8a12 |
| src/lib.rs | f81e4f8375c89dcbc8b18c71e554e9fc777901b51af312005a866cf669453711 |
| src/schema.rs | bd2800f795cbfcf89142801e7d352005b0a775f657186da12dd303276248aaf4 |
| tests/installed.rs | 832780037e7ad2660739c3a15feffd32742417ca528c8b3bd93b76ee3f78fb58 |
| tests/schema.rs | 617eaf4d2c5867865dc42fb2f632040ed48834297cca22a5672126b1ac036da9 |
| tests/fixtures/mod.rs | 16a9282a22f644dfaa6f5bc5710025baaae77cfc6c8989e3847659e66fe9edad |

## Deviations from the source manifest

`Cargo.toml` differs from gbl-bds-rs only in workspace adaptation:
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
