# Block-backed efivarfs provenance

The filesystem sources come from ACK android16-6.12, KMI generation 6:
`/home/vivy/Projects/efisp-projects/.work/thinpool-proof/.work/phone-common`,
commit `925a103d123c30a84577f29e1573376aacbde94b`.
Original SPDX notices, authorship and `MODULE_IMPORT_NS(EFIVAR)` are retained.

| Upstream path | Original SHA-256 |
| --- | --- |
| fs/efivarfs/super.c | 671773f9b0cd9b5ee05bfd7a55608039d67075decc998327cf22078fe38424d0 |
| fs/efivarfs/inode.c | 7ba69dd1ad51af11525a89977e1748855861e9153782f95d2b8926600f190159 |
| fs/efivarfs/file.c | 5fbdf4d179358134c999480f7522b227813c22cef3b822781e4238d3922b7b96 |
| fs/efivarfs/vars.c | 50bf38c3267bc7da8961780acba7ca7516d605d18b4f7a89b253dc007f1baf1f |
| fs/efivarfs/internal.h | ff7818298d360324f9e104f903a1b3073047049dea3a23f89f0c95d3907e38d6 |

## Deviations from verbatim

Exactly one copied-source change: `vars.c` adds
`{ ESU_PROJECT_GUID, "*", NULL }` immediately after the crash GUID entry.
`esu-guid.h` defines the GUID; Kbuild force-includes it rather than changing
any copied include directives. All other copied files are byte-identical.

The modified `vars.c` SHA-256 is
`cdf9cc5229b4cd9fab27f31b36a9be751a91141d1aec38d05bcd77f0945d96be`.
The other copied-file hashes are unchanged from the table above.

The upstream `super.c` has static init/exit functions and its own
`module_init`/`module_exit`. `super-wrapper.c` includes it unchanged, overriding
only those registration macros after including `linux/module.h`. This exposes
`esu_fs_init`/`esu_fs_exit` to the combined module. `bdsvars.c` owns the single
module entry point: register filesystem, initialize/register backend; on failure
unregister filesystem. Exit unregisters the backend, frees buffers/closes the
block file, then unregisters the filesystem. The filesystem owner prevents
normal module unload while mounted.

The plan's prose phase order contradicts its named source of authority,
`gobbl/crates/varstore/src/persist.rs::apply`. The implementation follows
the actual Rust algorithm (approved by the integration owner):

1. Predecessor(s): state AND `0xfe` (IN_DELETED_TRANSITION), then flush.
2. Replacement's full 60-byte header with state `0xff`, then flush.
3. Header state `0x7f` (HEADER_VALID), then flush.
4. UTF-16LE name, data and erased alignment padding, then flush. The GUID is
   already in the authenticated header at byte 44, not in the payload.
5. Replacement state `0x3f` (ADDED/live), then flush.
6. Predecessor(s): state AND `0xfd` (transition `0x3e` becomes `0x3c`), then flush.

Delete applies AND `0xfd` directly to all live predecessors in one phase
(`0x3f` becomes `0x3d`), without an intermediate transition. A missing key or
identical set makes no writes. Deleted/header-only records reserve their slot;
only the fully erased suffix is free. A transition is recovered as the last
transition only when no ADDED version exists, matching Rust's parser rather
than losing an interrupted update's old value. Authenticated-write attributes
on an existing key are rejected. New attributes must be exactly 7; an empty
value deletes (attribute 0 is additionally accepted for EFI deletion).

Every phase writes only changed ranges, fsyncs once, and compares its touched
span in 4096-byte chunks. The final commit additionally compares the entire
FV image, as Rust does. Any write/fsync/read/compare failure returns
EFI_DEVICE_ERROR after reload/revalidation; unsuccessful reload disables ops
until module reload, rather than operating on an uncertain image. The backend
accepts only authenticated-layout stores, as required by this module contract.
It validates FV identity/signature/checksum/block map, store identity/state and
bounds, record sizes/states, UTF-16 names, and the erased suffix.

## Build and host contract

The outer Makefile uses `PHONE_MODULE := efivarfs` and the shared
`../../scripts/phone-module.mk` gate, `scripts/kmi_modules.py`.
Supply `KMI_SRC` and `KMI_OUT`. No payload generation pin is added. Kbuild
uses `-Werror` and links the four filesystem objects and block backend.

Run `python3 test/test_backend.py` (or `make host-test` in the installed repo).
The test compiles the actual `bdsvars.c` through stub kernel include files with
`-std=gnu11 -Wall -Wextra -Werror`. Shims implement file-backed block I/O,
fsync, allocation, mutex and captured efivars registration; there is no second
format implementation. Tests copy the recovery evidence image, never write
the original, and compare CLI enumeration plus complete post-set/update/delete
images, assert each write/flush phase, reject unsupported attributes without
writes, fill the store without reclaim, verify malformed geometry/required dev,
and inject a readback mismatch with same-process recovery of the old value.
The Rust CLI is built with `cargo build --locked -p bdsvars` in the read-only
main checkout using this module directory's ignored `target-bdsvars` directory.
`BDSVARS_REPO` defaults to `/home/vivy/Projects/efisp-projects/gobbl`;
`BDSVARS_FIXTURE` and `CC` may also override host locations. Tests skip cleanly
if the CLI source checkout or the recovery fixture is absent.

During the original scratch bring-up, `gki-out/Module.symvers` was not yet
available. That scratch artifact (not installed as the delivery artifact) was
compiled against `phone-exact-out` with Android clang r536225 (19.0.1), using:

```sh
env PATH=/home/vivy/Projects/efisp-projects/.work/thinpool-proof/.work/cf-clang/clang-r536225/bin:/home/vivy/.cargo/bin:/usr/bin:/bin make -C /home/vivy/Projects/efisp-projects/.work/thinpool-proof/.work/phone-common O=/home/vivy/Projects/efisp-projects/.work/thinpool-proof/.work/phone-exact-out M=/home/vivy/Projects/efisp-projects/.work/efivarfs-module ARCH=arm64 LLVM=1 KBUILD_GENDWARFKSYMS_STABLE=1 KBUILD_MODPOST_WARN=1 -j4 modules
```

Compilation is warning-as-error clean. MODPOST reports the intentional trimmed
imports (warning mode is required for loader relocation); these are not compiler
warnings. `test/imports.txt` records the `llvm-nm -u efivarfs.ko` inventory.
The following imports are absent from that output's Module.symvers, but every
one is present in its System.map and must resolve through the loader's kallsyms
path:

```
always_delete_dentry
d_alloc
efi_status_to_err
efivar_get_next_variable
efivar_get_variable
efivar_is_available
efivar_lock
efivar_ops_nh
efivar_query_variable_info
efivar_set_variable_locked
efivar_supports_writes
efivar_unlock
efivars_register
efivars_unregister
get_next_ino
get_tree_single
guid_parse
ucs2_as_utf8
ucs2_strnlen
ucs2_strsize
ucs2_utf8size
uuid_is_valid
```

No device was accessed. Host evidence does not prove recovery insertion,
SELinux access or phone durability; those remain the integration owner's gates.
Stock `super.c` gates statfs on EFI runtime-service support, so its statfs
capacity reporting may remain zero on a non-EFI Android kernel even though
the backend's query operation reports the actual store geometry. That source
behavior is intentionally left verbatim, not silently patched here.
