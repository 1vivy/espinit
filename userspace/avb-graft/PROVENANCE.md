# AVB graft canonical-source provenance

`crates/avb-graft/src/lib.rs` in gobbl is the canonical allocation-free
layout implementation. Public esu vendors this exact file as
`userspace/avb-graft/src/lib.rs`; GBL embeds the same source through the
libbootloader patch series. Adapters own I/O and cryptographic admission.
Changes to this file must be copied to both consumers in the same cutover.

The wire format is grounded in AOSP `external/avb/libavb`:

- `avb_footer.h`: fixed end-of-partition 64-byte AVBf footer, version at 4/8,
  original payload size at 12, metadata offset/size at 20/28, reserved at 36.
- `avb_vbmeta_image.h`: fixed 256-byte AVB0 header, auth/aux block sizes at
  12/20, block-relative hash/signature/key/key-metadata/descriptor ranges at
  32/48/64/80/96, release string at 128, reserved at 176. Blocks align to 64.
- `avb_descriptor.h`: two big-endian u64 fields (tag, following-byte count),
  a 16-byte descriptor header and 8-byte aligned following data.
- `avb_version.h`: format compatibility through libavb 1.4.
- `avb_vbmeta_image.c`: release string must end in NUL (header byte 175).

This is structural validation, not signature, key-policy or payload validation.
Unknown descriptor semantics and cryptographic admission remain libavb's job.
Original metadata is intentionally not trusted or required to parse: valid
footer geometry can point to an empty region. Replacement placement retains the
original metadata offset, replaces only the exact validated metadata bytes and
end footer, and preserves all other bytes and the total image size.

Current source SHA-256:
`7efab28ac81752f6f46e523d2daaee39c307b5e01bfa1cedfb0e219f307d049b`.
Contract tests SHA-256:
`f23d12b57d901535da305f4b510773cded33d25729f49eb8cf1b2ed2df42ec3d`.
These content hashes, rather than a pre-cutover commit hash, identify the source.

From the gobbl worktree, check public esu provenance with:

```sh
sha256sum crates/avb-graft/src/lib.rs crates/avb-graft/tests/layout.rs
cmp crates/avb-graft/src/lib.rs /home/vivy/Projects/efisp-projects/espinit/userspace/avb-graft/src/lib.rs
cmp crates/avb-graft/tests/layout.rs /home/vivy/Projects/efisp-projects/espinit/userspace/avb-graft/tests/layout.rs
cmp crates/avb-graft/src/lib.rs /home/vivy/Projects/efisp-projects/gobbl-aosp/bootable/libbootloader/gbl/avb-graft/src/lib.rs
```

After applying the GBL patch series, compare its embedded module directly with
this canonical `src/lib.rs`, not a separately maintained parser. Formatting the
canonical source requires refreshing copies and these hashes together. Only
Cargo manifests may differ for workspace integration.
