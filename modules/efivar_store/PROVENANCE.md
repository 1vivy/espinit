# Rust EFVS backend provenance and build

Source: https://github.com/1vivy/efivar-store; exact commit is recorded in
`SOURCE_REVISION` (published `main`, `3a42f653cfc5ae9b13e8247fadd31f6b650f3e30`).
No source is copied into kernelesp. `scripts/kmi_modules.py build` checks out
that revision in ignored `.source`, refuses tracked working-tree changes,
builds its `linux/efivar_store.rs`, admits the final artifact and writes the
standard schema-2 module/KMI-hash receipt beside `efivar_store.ko`.

```sh
export PATH="$HOME/.cargo/bin:$PATH"
export KMI_SRC=/path/to/android16-6.12/source
export KMI_OUT=/path/to/matching/gki-out
# Local integration before publication of the pin:
export EFIVAR_STORE_REPO=/path/to/efivar-store
make -C modules/efivar_store JOBS=4
python3 scripts/kmi_modules.py verify --kmi-out "$KMI_OUT" \
  --module modules/efivar_store/efivar_store.ko
```

The build defaults to upstream rustc 1.82.0 at its rustup toolchain path and
LLVM tools in `/usr/bin/`. `EFVS_RUSTC` and `EFVS_LLVM` override these paths.
The exercised pairing is rustc 1.82.0 (LLVM 19.1.1 bitcode) and LLVM 23.1.1
linking/codegen, using matching ACK android16-6.12 `925a103d123c` Rust metadata.
Android LLVM 19.0.1 linked this upstream Rust bitcode into invalid IR (`ptr
undef`), even though linking and KMI admission passed; QEMU failed at init.
LLVM 23.1.1 passed the production esu loader round-trip. Do not assume ABI
admission proves LLVM bitcode pairing. Exact Android rustc1.82.0.p1 metadata
pairing and physical phone runtime remain unproven.

The module is not restricted to exported GKI symbols. It is loaded by esu's
relocating loader using live kallsyms for trimmed imports, not ordinary insmod.
Load upstream efivarfs first, then efivar_store with `dev=major:minor`, then mount
read-write efivarfs. Nonblocking writes are unsupported. PolicyNone refuses
authenticated enrollment; firmware alone formats blank storage and compacts.
The VM harness and complete non-KMI inventory are in the pinned source's
`docs/linux.md`; final source-build logs are `/var/tmp/efvs-geometry-final/`.

## Final exercised gates

The pinned-source build and schema-2 receipt verification passed against
`/home/vivy/Projects/efisp-projects/.work/thinpool-proof/.work/gki-out`.
Final backend admission: 38 versioned imports, 11 kallsyms imports; frontend:
59 versioned imports, 20 kallsyms imports. No CRC mismatches.
Backend modinfo reports GPL, no dependencies, `dev` and `partuuid`, vermagic
`6.12.58-4k-g925a103d123c SMP preempt mod_unload modversions aarch64`.

Both final packaged modules passed the actual esu-loader VM harness:
`/var/tmp/efvs-packaged-geometry/{write,read,compact,torn,blank}.log`.
The blank image remained byte-for-byte zero. Touched-crate fmt/clippy passed;
esuinit/esud/esu-config tests: 150 passed, including frontend/backend missing
payload/receipt rejection; `python3 -m unittest scripts.test_kmi_modules`:
6 passed. No phone commands, push, merge or PR were performed.
