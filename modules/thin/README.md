# `modules/thin` — esu `thin.ko`

A separate **GPL-2.0-only** out-of-tree kernel module that provides the
device-mapper `thin-pool` and `thin` targets the multi-ROM thin-storage layout
needs. It is vendored from the Linux kernel dm-thin family and forked to use
only exported KMI surface; `PROVENANCE.md` records the exact origin, file set,
patch and proof evidence.

`thin.ko` is aggregated with, never linked into, the GPL-3.0 esu core. The
repository's top-level `LICENSE` is GPL-3.0 and does **not** cover this subtree:
see `LICENSE` here (the verbatim GPL-2.0 text) and the `GPL-2.0-only` SPDX
headers of every vendored file.

## Output and identity

| Item | Value |
| --- | --- |
| Module file | `thin.ko`, built in this directory |
| Module name | `thin` (from `obj-m := thin.o`); the ESP manifest `modules[].name` must be `thin` |
| Description | `esu thin provisioning targets (thin-pool, thin)` |
| Author | `esu` |
| License | `GPL v2` in `thin-main.c`; the vendored files keep their upstream `MODULE_LICENSE("GPL")` boilerplate, all GPL-2-compatible (see `PROVENANCE.md`) |
| Device-mapper targets | `thin-pool` (v1.23.0) and `thin` (v1.23.0), names unchanged from the proven build |

The proven private symbol names (`thinpool_private_*`, applied by
`private-rename.h`) are retained, so the module neither defines nor overrides
kernel device-mapper symbols, and the proven two-target registration is
unchanged.

The ESP manifest loads `thin` as an ordinary later module — `name = "thin"`,
`path = "modules/thin.ko"` — listed after the `esu` core and before `gpt`,
since entries ordered before `gpt` run the scripts that may activate the logical
volumes `gpt` then projects. The manifest contract is described in the
repository `README.md`.

## Module parameters

The esu PID-1 loader self-check (`userspace/esuinit/src/selfcheck.rs`) reads
`/sys/module/thin/parameters/` after loading:

| Parameter | Access | Value |
| --- | --- | --- |
| `generation` | read-only (0444) | the build generation compiled into the module; must equal the payload generation |
| `ready` | read-only (0444) | `Y` only after every subsystem and both targets initialized; `N` while any initializer fails and again as soon as unload teardown starts |

`generation` is injected by the Makefile and checked at compile time
(`thin-main.c` fails to build without it). `ready` is set exactly once, after
the last initializer returns, and cleared first in the exit path; the proven
init order and every failure-unwind branch are unchanged.

## Build

Prerequisites:

- the exact target source, complete build output (`vmlinux`, `Module.symvers`,
  generated headers), and an independently captured full phone config.
  `modules_prepare` alone is insufficient. The phone uses `CONFIG_MODVERSIONS=y`,
  `CONFIG_GENDWARFKSYMS=y` and `CONFIG_TRIM_UNUSED_KSYMS=y`.
- a clang/LLVM toolchain matching that kernel (`LLVM=1` is always used).

```sh
KERNEL_SRC=/path/to/kernel KERNEL_OUT=/path/to/out \
    KERNEL_CONFIG=/path/to/captured-phone.config modules/thin/build.sh
```

or directly:

```sh
make -C modules/thin KERNEL_SRC=/path/to/kernel KERNEL_OUT=/path/to/out \
    KERNEL_CONFIG=/path/to/captured-phone.config [JOBS=1..13]
```

- All three modules use the [shared phone contract](../../README.md#build-notes).
  `KERNEL_SRC`, `KERNEL_OUT` and `KERNEL_CONFIG` are required; mismatched full
  configuration or stale generated configuration fails before compilation.
- `JOBS` defaults to 13 and is capped at 13; phone builds use `ARCH=arm64`.
- `KBUILD_GENDWARFKSYMS_STABLE=1` is always passed and old module objects are
  cleaned first. Disabling MODVERSIONS or inserting empty versions is forbidden.
- Unresolved symbols are never tolerated for thin: modpost must pass without
  `KBUILD_MODPOST_WARN`. Real import CRCs including `module_layout`, export CRCs,
  exact vermagic and target BTF settings are verified after build.
- Keep the generated `thin.ko.compat.json` beside `thin.ko` when copying it to a
  payload. The assembler checks the receipt and module again before packaging.

### Build generation

The generation identifies one coordinated ESP payload and must match the PID-1
stage, the esu core module, the esud daemon and every other ESP module.
It is taken from `ESU_GENERATION` when set, otherwise from the full 40-byte
lowercase Git HEAD hash of this repository, and must be 1-63 ASCII
letters/digits/`._-`. The Makefile validates it before compiling and rejects a
missing or malformed value with the build failing, never with a truncated or
empty generation. Keep this logic in sync with `kernel/Kbuild`,
`userspace/esud/build.rs` and `userspace/esuinit/build.rs`.

## Layout

| Path | Contents |
| --- | --- |
| `thin-main.c` | module entry point: init order, unwind, `generation`/`ready` parameters, metadata |
| `src/` | vendored dm-thin family sources (`dm-thin`, `dm-thin-metadata`, `dm-bufio`, `dm-io`, `dm-kcopyd`, `dm-bio-prison-v1/v2`) |
| `src/persistent-data/` | vendored persistent-data helpers (btree, bitset, array, block/space maps, transaction manager) |
| `private-rename.h` | private `thinpool_private_*` renames for every non-exported symbol |
| `Makefile` | kbuild module description, include paths, generation validation/injection, bounded out-of-tree build wrapper |
| `build.sh` | scripted entry point for the same build with required `KERNEL_SRC`/`KERNEL_OUT` |
| `patches/0001-fork-dm-thin-for-exported-KMI-surface.patch` | the fork that adapts the vendored sources to the exported KMI |
| `evidence/phone-d3144fcc5f04/` | frozen pre-import phone artifact, its import/modversion manifests and record summary |
| `PROVENANCE.md` | origin, exact file set with hashes, modifications, proof evidence, licensing |
| `LICENSE` | verbatim GPL-2.0 text |

## Verifying a built module

```sh
modinfo thin.ko                      # description, author, license, vermagic
modprobe --dump-modversions thin.ko  # import manifest; 151 imports when built for the phone kernel
insmod thin.ko
cat /sys/module/thin/parameters/generation
cat /sys/module/thin/parameters/ready   # Y only after a complete init
dmsetup targets                         # thin-pool and thin
```

The recorded proof for the vendored sources covers: arm64 build, valid modpost
and signed `insmod` against ACK `f1bdb13583da85a47fcf1632a78ef52d6e6da651`; the
full dm-thin behavior matrix in QEMU (snapshots, discard, partial-block zeroing,
`dm-default-key` ciphertext equality, ENOSPC modes, forced-reset metadata
recovery); and the phone-matched artifact below. `evidence/` states exactly what
the frozen artifact does and does not prove.
