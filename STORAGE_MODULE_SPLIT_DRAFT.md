# Storage-tool ownership rationale

**Status (2026-10-10)** — Retained rationale for the selected
[Partformer ABI4 mockup](../mockups/2026-10-09/partformer/README.md).
The [README integration checklist](README.md#remaining-egysk-integration-checklist)
owns remaining work. This document is not a second plan or deployment authority;
the selected design is not a claim that its runtime behavior has been exercised.

## Standard

Use upstream userspace tools for LVM and device-mapper operations, patching them
only where necessary instead of maintaining custom LVM/DM implementations.
Apply the same standard to projection: reuse existing tools where they meet the
contract, and keep ppconf a minimal frontend for the custom configfs ABI rather
than a parallel storage implementation.

Publishing, hiding and sealing remain wanted features with their intended
behavior preserved. Improving their implementation is welcome; removing or
weakening them is not the proposed simplification.

## Userspace package/module boundaries

| Package/module | Owns | Does not own |
| --- | --- | --- |
| `lvm2/` | Upstream `lvm`, `dmsetup`, `dmstats`, required configuration and necessary product patches | Kernel modules or their loading; a custom LVM/DM reimplementation |
| `ppconf/` | Thin userspace frontend to part-projection configfs, ABI checks and diagnostics | `part-projection.ko` installation/loading, LVM activation, ROM selection or storage orchestration |
| `partformer/` | Generic partition-view composition and relevant current `fw-views` orchestration; calls LVM/DM tools and ppconf | ROM selection, duplicate tool binaries, private ioctl implementations or ownership of prerequisite LKMs |

These userspace packages belong in the modules repository. Independent LKM
sources remain in the LKMs repository. Kernel artifacts are separately built,
packaged and loaded by boot/platform provisioning; a separate DKMS package may
provide them on systems using DKMS. Installing a userspace tool package does not
imply installing or loading its kernel counterpart.

Current source is `../egysk-modules` at recovered full main
`e985dfac4fea73751f5ef3865cdcc4122c8a4eb3` from
`retired/kernelsu-esp-modules`, and `../egysk-lkms` at recovered main
`75af27f5b2d79c2f11721069f8d1f739c4ac9f36` from
`retired/kernelsu-esp-lkms`, with both complete histories preserved. They are
maintained source inputs, not leftover build outputs or missing repositories.
Their old platform/module code still requires integration; source recovery is
not the Partformer ABI4 production port, publication or device proof. Hosted
split-repository creation/private push and the core `1vivy/egysk` rename are
authorized but not yet performed; the local core remains `../kernelesp`.

The core now supplies `egyskinit`, `egyskd`, `egysk-build-cpio` and `egysk.ko`,
with `/dev/egysk`, ESP `egysk/egysk.toml`, cpio `/egysk.toml` and
`egysk-build.json`. Independent `/dev/esp/esu`, per-ROM `esu.cpio`,
`esu.stage.cpio`, `esu-bootctl` and frozen wire bytes/GUIDs remain separate.
The README documents the one offline product-subtree engine,
`egyskinit --transition-product-state --offline PHYSICAL_BACKING_ROOT`; it
preserves persisted `.esp-generation`/`esp-tmp` markers and does not traverse
credential siblings or transition installation credential identity. Platform
installer wiring remains in the README checklist, not a second plan here.

`partformer` requires the userspace tools and appropriate kernel features, but
these are separate requirements. Use existing module/phase ordering and explicit
prerequisite checks; do not introduce a dependency solver. Missing tools or
incompatible/unavailable kernel ABIs fail closed.

## Operation ownership

- LVM-owned PV/VG/LV metadata, allocation and activation: upstream `lvm` commands.
- Product-owned DM mappings, tables, target messages and live switching:
  upstream `dmsetup`. Do not manipulate LVM's internal mappings behind its back.
- Custom partition publication, hiding and sealing: ppconf and the projection
  kernel interface.
- ROM selection and ROM-derived naming/policy: the gobbl adapter and OTA/HAL
  callers. Generic composition, sequencing and recovery: `partformer`, not ppconf.

Use explicit subprocess arguments and machine-readable reports where available.
Preserve stable mapping identity and open FDs during OTA switching; do not
replace reload with remove/recreate. Upstream DM already supports reload,
suspend/resume and target messages. A patch requires a concrete missing behavior,
not an assumption that reload itself is absent. A C FFI layer is an alternative
only if a concrete requirement justifies it, not the default direction.

## Selected kernel implementation boundary

The current projection LKM owns both custom partition semantics and block-I/O
machinery overlapping DM: range forwarding, flush fan-out, queue-limit stacking,
inline-encryption propagation and I/O lifetime management.

The selected [Partformer ABI4 mockup](../mockups/2026-10-09/partformer/README.md)
uses DM composition (`pp-meta`/`pp-range`/`pp-hole`) rather than retaining the
outer private block-I/O stack. DM owns splitting, lifetime, queue stacking and
crypto propagation; custom targets retain only the required metadata/range/hole
semantics. The integration checklist owns the production port and unresolved
runtime behavior. Reuse source/RE and existing userspace evidence for the
questions they answer; do not turn this rationale into a second set of gates.

Preserve real partition publication and Android discovery semantics, backing
access modes, encryption behavior, hiding and sealing. Hiding currently removes
physical PARTNAME identities, not raw-device access. Existing seal limitations
must be described honestly: partition RO flags can be cleared by root, and
pre-existing writable opens are not a storage firewall. Preserve the intended
features while evaluating improvements, rather than presenting those limitations
as a reason to drop them.

Configfs remains a reasonable interface for the custom controls. ppconf should
stay thin, and informational commands should not mount or otherwise mutate state.
