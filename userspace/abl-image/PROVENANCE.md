# Vendored abl-image provenance

`userspace/abl-image` is a pinned copy of `crates/abl-image` from gobbl,
commit `344617cc4f6aa36a4c65dfe04eb389dda0b68223` (branch `storage/efvs`).

The crate is MIT-licensed in both repositories: it is a port of the
MIT-licensed `ablfvextractor` crate, not of this project's GPL-3.0-only
userspace. `src/` is byte-identical to the source crate (`diff -r` is empty);
only the manifest is adapted to espinit's workspace. Both files are copied
because the `abl-key` anchor-key extractor is part of the same package.

| file | sha256 |
| --- | --- |
| src/lib.rs | 318bf72b7a3b71c669b1340e999afa7c144bea46d7daaab901eb1d8d4d72fdb3 |
| src/bin/abl-key.rs | 771708fb87c8ad537054b3176366c33703c598363610c744e7c72b0257a03c3e |

The crate's in-module tests read a GBL test key through the source-relative
path `../../../vendor/libbootloader/gbl/libgbl/testdata/testkey_rsa4096_pub.bin`,
so that one fixture is copied unchanged from gobbl to keep `src/lib.rs`
byte-identical and `cargo test -p abl-image` green:

| fixture | sha256 |
| --- | --- |
| vendor/libbootloader/gbl/libgbl/testdata/testkey_rsa4096_pub.bin | 7728e30f50bfa5cea165f473175a08803f6a8346642b5aa10913e9d9e6defef6 |

It is test data only: nothing in the payload builds or ships it.

From the gobbl worktree, check canonical source identity with:

```sh
diff -r crates/abl-image/src /home/vivy/Projects/efisp-projects/kernelesp/userspace/abl-image/src
sha256sum crates/abl-image/src/lib.rs crates/abl-image/src/bin/abl-key.rs
cmp vendor/libbootloader/gbl/libgbl/testdata/testkey_rsa4096_pub.bin \
    /home/vivy/Projects/efisp-projects/kernelesp/vendor/libbootloader/gbl/libgbl/testdata/testkey_rsa4096_pub.bin
```

## Deviations from the source manifest

`Cargo.toml` differs from gobbl only in workspace adaptation: `version = "0.1.0"`
and `edition.workspace = true` instead of gobbl's inherited workspace fields, and
no `[lints]` section because espinit's workspace declares none. The `lzma-rs`
requirement is already resolved by espinit's lock set, so the vendored crate adds
no new dependency.

## Consumers

`ota-core`'s `abl_has_efisp` extracts the LinuxLoader PE with
`abl_image::extract_linuxloader` and searches it for the `efisp` needle. Surfacer
(gobbl) reads and writes the physical `abl_<x>` partitions itself and only shares
the 5-byte needle constant, not this crate.
