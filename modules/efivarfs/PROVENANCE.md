# Upstream efivarfs frontend provenance

The filesystem sources are pristine ACK android16-6.12, KMI generation 6,
commit `925a103d123c30a84577f29e1573376aacbde94b`, from
`https://android.googlesource.com/kernel/common` (`fs/efivarfs/`).
Original SPDX notices, authorship and `MODULE_IMPORT_NS(EFIVAR)` are retained.

| Upstream path | SHA-256 |
| --- | --- |
| fs/efivarfs/super.c | 671773f9b0cd9b5ee05bfd7a55608039d67075decc998327cf22078fe38424d0 |
| fs/efivarfs/inode.c | 7ba69dd1ad51af11525a89977e1748855861e9153782f95d2b8926600f190159 |
| fs/efivarfs/file.c | 5fbdf4d179358134c999480f7522b227813c22cef3b822781e4238d3922b7b96 |
| fs/efivarfs/vars.c | 50bf38c3267bc7da8961780acba7ca7516d605d18b4f7a89b253dc007f1baf1f |
| fs/efivarfs/internal.h | ff7818298d360324f9e104f903a1b3073047049dea3a23f89f0c95d3907e38d6 |

Exactly one named patch, `patches/0001-esu-project-guid.patch`, adds the
esu project GUID to the removable-variable allowlist. Kbuild applies it to
ignored/generated `vars-patched.c`; `vars.c` remains byte-identical upstream.
There is no combined backend, registration wrapper, C format engine or host
shim. The filesystem's own upstream module entry point remains unchanged.

Build with `make -C modules/efivarfs KMI_SRC=... KMI_OUT=... JOBS=4`.
The shared `scripts/kmi_modules.py` admission writes a schema-2 `.compat.json`
receipt binding the module and KMI reference hashes. Load this frontend through
esu's production relocation loader, then the separately built Rust
`efivar_store.ko` with `dev=major:minor`, before mounting efivarfs.
See `modules/efivar_store/PROVENANCE.md` for its pinned source and LLVM pairing.
