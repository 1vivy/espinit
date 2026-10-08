# Vendored esu-config provenance

`userspace/esu-config` is a pinned copy of `crates/esu-config` from gobbl,
commit `f91f411` (branch `main`, "feat: ROM OTA staging — image roles, stage
sources, copy-once arena, ROM 1 posture").

The schema code was authored in this repository as
`userspace/esuinit/src/config.rs`; the owner approved publishing it as
Apache-2.0 in gobbl and vendoring it back as this shared crate. The `src/`
and `tests/` trees are byte-identical to the source crate (`diff -r` is empty);
only the manifest is adapted to espinit's workspace.

| file | sha256 |
| --- | --- |
| src/installed.rs | a6ee019e016ff28370be5bacdb9d12575fb0a260f67c58b5cc99aae295aea2fb |
| src/lib.rs | 7786d883251e8ede27e0ba9155baa03019b88606251958e050fdf18ad9631929 |
| src/schema.rs | f47c72062f3c0f1c98ef8a421d58d61c4e560c5b6e30808741181027ad11c93b |
| tests/installed.rs | 2b4017b221af7ceed63ba80cc6d4a0d643adc94cf8369baf0eb6b164daa380c3 |
| tests/schema.rs | 5063e209620112c00f84fa20fb80a527ea5c729ed032b9afaf2d220fdac874d8 |
| tests/fixtures/mod.rs | d3c9f7840e4634aef5c106f6bf0b05955f90556c32bc315707cc6c145539f6b1 |

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

This pin replaces the fixed seven-base, fourteen-path ESP kernel set with
`rom-image:<base>` roles: ROM ≥ 2 declares any subset of `IMAGE_BASES` (GBL's
ten replacement names), each `<base>_a`/`<base>_b` partition naming
`rom-image:<base>` with `read_only = false`, served by one base-named ESP file
`base_image_path(id, base)` = `rom/<id>/<base>.img`. `kernel_images()` takes no
slot. New codes: `KernelSetEmpty`, `KernelSetReadOnly`, `BackendRomImageBase`;
`KernelSetDuplicatePath` and `KERNEL_SET_BASES` are gone. Earlier pins added the
bounded ROM `number` and the three-module bootstrap (`kernelesp`, `efivarfs`,
`efivar_store` with `dev=by-name:bdsvars`).

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
