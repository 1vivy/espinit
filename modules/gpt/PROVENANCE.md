# Provenance and licensing

`gpt.c`, `gpt_uapi.h`, `Makefile` and the module documentation are newly authored
for esu's single in-memory GPT projection contract. This is not a fork of a
GPT rewriting module or a physical block-I/O firewall. All files in this subtree
are GPL-2.0-only; `LICENSE` supplies the full GPL v2 text and fixes the license to
version 2 only.

The generation derivation and literal-safe ASCII token validation in `Makefile`
reuse esu's existing `kernel/Kbuild` convention; the identity is compiled
into static storage and cannot be changed by a load-time parameter.

Public Linux v6.12 interfaces consulted during implementation:

- [include/linux/blkdev.h](https://github.com/torvalds/linux/blob/v6.12/include/linux/blkdev.h)
  — file-based bdev opens, dynamically allocated bio-based disks, initial disk
  registration/partition scan, partition metadata, atomic per-bdev flags,
  `blk_set_stacking_limits`, `queue_limits_stack_bdev` and exported `submit_bio`.
- [include/linux/blk_types.h](https://github.com/torvalds/linux/blob/v6.12/include/linux/blk_types.h)
  — partition/whole-disk relationship, sector offsets and bio representation.
- [include/linux/blk-crypto-profile.h](https://github.com/torvalds/linux/blob/v6.12/include/linux/blk-crypto-profile.h),
  [include/linux/blk-crypto.h](https://github.com/torvalds/linux/blob/v6.12/include/linux/blk-crypto.h)
  and [block/blk-crypto-profile.c](https://github.com/torvalds/linux/blob/v6.12/block/blk-crypto-profile.c)
  — slotless stacking profile, lower capability intersection and key eviction.
- [block/bio-integrity.c](https://github.com/torvalds/linux/blob/v6.12/block/bio-integrity.c)
  — integrity payload allocation semantics for cloned bios.
- [drivers/block/brd.c](https://github.com/torvalds/linux/blob/v6.12/drivers/block/brd.c)
  — conventional bio-based disk operations and page mapping for RAM metadata.

No binary, extracted firmware code, downstream patch or third-party GPT module
was imported. The license document is the standard FSF GPL v2 text; it is a
license-text copy, not imported implementation code.

Repository validation has established exact Cuttlefish- and phone-kernel builds,
phone import resolution against the exact `vmlinux`, and the disposable
Cuttlefish runtime behavior listed in README. It has not established physical
UFS hiding/restoration or wrapped-key inline-crypto behavior on the phone; those
remain device-validation requirements.
