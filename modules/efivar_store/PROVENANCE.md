# EFVS backend provenance and build

Source: https://github.com/1vivy/efivar-store; the exact commit is recorded in
`SOURCE_REVISION`. No source is copied into kernelesp. `scripts/kmi_modules.py build`
checks out that revision in ignored `.source`, refuses tracked working-tree changes,
builds its `linux/` directory with ACK kbuild, admits the final artifact and writes the
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

## Design

The module is a C kbuild module (`linux/efivar_store.c`) that links the audited
allocation-free Rust EFVS engine as a freestanding `no_std` object through a small C
ABI. It imports no Rust-mangled symbol and needs neither `CONFIG_RUST` nor the kernel's
Rust crates. This is deliberate: the first backend, written against the kernel `rust`
crates, was refused on the phone because the running GKI exports different `core`/`kernel`
crate hashes and no `core::fmt::Formatter` methods (`20261008T055846Z-phone-efvs-lkm-test`).
The Rust object is built offline with `nightly-2026-08-08`, `-Z build-std=core,compiler_builtins`
for `aarch64-unknown-none-softfloat` and `panic=immediate-abort`; the C object and link use
`LLVM=/usr/bin/` (clang 23, `EFVS_LLVM` overrides). The C object is compiled by kbuild
against the ACK headers; the earlier finding that Android LLVM 19.0.1 mislinked upstream
Rust bitcode does not apply because the Rust object carries no bitcode.

The module is loaded by esu's relocating loader using live kallsyms for trimmed
imports, not ordinary insmod. Load upstream efivarfs first, then efivar_store with
`dev=major:minor`, then mount read-write efivarfs. Nonblocking writes are unsupported.
PolicyNone refuses authenticated enrollment; firmware alone formats blank storage and
compacts. The VM harness and symbol inventory are in the pinned source's `docs/linux.md`.

## Device evidence

Loaded by hand on the real phone kernel in passthrough recovery, then exercised through
efivarfs: firmware-written `Slot-rom1`/`MergeStatus-rom1`/`BootedRom` read back
byte-identical, a probe variable was set, updated and deleted with the physical store
parsing cleanly after each step, and the next firmware boot replayed and compacted the
Linux-written records (`20261008T063318Z-phone-efvs-lkm-test`,
`20261008T063449Z-phone-efvs-efivarfs-test`, `20261008T063532Z-phone-efvs-store-read`).
