# Draft: userspace storage tools and partition-view composition

Status: discussion draft, 2026-10-09. Not an implementation instruction or a
replacement for KERNELSU_REFORK_PLAN.md. Saving this draft does not authorize
changes to the ongoing implementation, kernel modules or deployed payloads.

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
| `partformer/` | Composition of the selected ROM partition view, including the relevant current `fw-views` orchestration; calls LVM/DM tools and ppconf | Duplicate tool binaries, private ioctl implementations or ownership of prerequisite LKMs |

These userspace packages belong in the modules repository. Independent LKM
sources remain in the LKMs repository. Kernel artifacts are separately built,
packaged and loaded by boot/platform provisioning; a separate DKMS package may
provide them on systems using DKMS. Installing a userspace tool package does not
imply installing or loading its kernel counterpart.

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
- Product naming, selected-ROM policy, sequencing and recovery: `partformer`
  and the appropriate OTA/HAL callers, not the generic tool packages.

Use explicit subprocess arguments and machine-readable reports where available.
Preserve stable mapping identity and open FDs during OTA switching; do not
replace reload with remove/recreate. Upstream DM already supports reload,
suspend/resume and target messages. A patch requires a concrete missing behavior,
not an assumption that reload itself is absent. A C FFI layer is an alternative
only if a concrete requirement justifies it, not the default direction.

## Kernel implementation question still open

The current projection LKM owns both custom partition semantics and block-I/O
machinery overlapping DM: range forwarding, flush fan-out, queue-limit stacking,
inline-encryption propagation and I/O lifetime management.

Prefer upstream DM infrastructure where it can preserve the full contract.
Merely placing DM devices beneath the current projection disk does not eliminate
that disk's custom data path. Exported DM symbols alone do not establish a
replacement either. A DM-composed view or custom DM target needs concrete proof
before selecting a kernel cutover; neither is approved by this draft.

Preserve real partition publication and Android discovery semantics, backing
access modes, encryption behavior, hiding and sealing. Hiding currently removes
physical PARTNAME identities, not raw-device access. Existing seal limitations
must be described honestly: partition RO flags can be cleared by root, and
pre-existing writable opens are not a storage firewall. Preserve the intended
features while evaluating improvements, rather than presenting those limitations
as a reason to drop them.

Configfs remains a reasonable interface for the custom controls. ppconf should
stay thin, and informational commands should not mount or otherwise mutate state.
