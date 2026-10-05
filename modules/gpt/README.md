# In-memory GPT projection

This GPL-2.0-only external module targets Linux 6.12. Build against the exact
kernel output and exported-symbol metadata used by the payload:

```sh
make -C modules/gpt KERNEL_SRC=/path/to/exact/source KERNEL_OUT=/path/to/exact/output \
    KERNEL_CONFIG=/path/to/captured-phone.config ESPINIT_GENERATION=payload-generation
```

Load `gpt.ko` without a generation override. `/dev/gptctl` is a root-only misc
device; both ioctls also require `CAP_SYS_ADMIN`. `generation` and `ready` are
read-only sysfs parameters. The generation is compiled from `ESPINIT_GENERATION`
(1–63 ASCII letters/digits/._-), or the full 40-byte lowercase Git HEAD when unset,
using the same validation as `kernel/Kbuild`. Size assertions pin its storage;
load-time parameters that change the compiled identity are rejected. Loading
does not create a disk or assert readiness.

## ABI v1

`gpt_uapi.h` is canonical. Structures contain fixed-width fields and no pointers,
and the 32-bit compat ioctl uses the identical wire layout. Compile-time checks
pin the 56-byte projection, 8-byte device, 5648-byte APPLY, 16-byte QUERY and
ioctl values (`0x56104701`, `0x80104702`). APPLY requires version 1, 1–64
projections, at most 256 hidden devices, zero flags/reserved fields and zero
unused array slots. Names are nonempty NUL-terminated printable ASCII (at most
36 bytes), with zero trailing bytes. Projection names and backend device numbers
must each be unique; hidden device numbers must be unique within hide[]. A
physical partition may appear in both arrays, intentionally.

Open backends and prepare all metadata before creating `espinit-gpt` with the
block core's dynamic device numbering. Its initial `device_add_disk` scan reads
the RAM-backed protective MBR and primary/backup GPT headers and entry arrays.
Headers and arrays carry independent standard CRC32 checksums. The logical
block size is the maximum backend logical block size (512–4096 bytes). Each
projection covers its entire backend at a 1-MiB-aligned virtual start; extents do
not overlap. Capacities/offsets must be divisible by that logical block size and
fit sector and signed byte capacity limits. Primary metadata includes its final
page's zero padding so the partition scanner's page-cache reads can succeed.
The backup array is page-aligned after the last aligned extent. Metadata and
reserved-page padding are read-only; all other gaps reject I/O.

Mapped I/O uses `bio_alloc_clone`, sharing payload vectors and preserving
crypto and integrity context. Partition backends hold both the partition and
its whole-disk bdev; clones target the whole disk plus saved `bd_start_sect`.
Whole-disk dm/loop backends start at zero. Sector translation **does not alter
inline-encryption DUNs**. An initialized slotless stacking profile starts with
all key types supported, intersects every lower backend's modes, data-unit sizes,
maximum DUN width, key types and registered-key capacity, forwards key eviction
to every backend, and delegates software-secret derivation to the first capable
backend. Thus wrapped keys pass through unchanged while the synthetic queue
advertises only common capability; APPLY fails if capabilities do not intersect.
Explicit read-only projections reject every operation except reads and flushes
and mark their scanned synthetic bdev read-only. Queue limits are initialized
with `blk_set_stacking_limits` and intersect every held lower bdev's limits
through `queue_limits_stack_bdev`, including discard and write-zeroes.
Mapped clones forward every operation admitted by that stacked upper queue via
the exported `submit_bio`; there is no read/write-only operation allowlist.
Metadata still accepts only unencrypted reads. Sectorless flushes, including
zero-length `REQ_OP_WRITE|REQ_PREFLUSH`, fan out once to every writable backend.
Unregistering the disk drops an in-flight bias and waits on a completion, so
clone callbacks cannot race backend release.

Only after the complete synthetic partition table has been verified does APPLY
clear every supplied physical partition's PARTNAME and set its per-bdev
`BD_READ_ONLY`. No physical GPT sectors are written. Original names and RO bits
are saved and restored on rollback/unload. The physical disk open mutex protects
each endpoint update. Physical partitions lacking metadata are still made RO.
Hide entries must actually be partitions; a raw LU is rejected in hide[].

This is **not a raw-LU firewall**: parent logical units remain accessible, and
already-open clients are not revoked. In particular, writable projections keep
held lower whole-disk handles so physical-partition hiding does not also make
those projections read-only. Enumeration, dependency activation and exclusion
of competing storage users are the managed boot caller's responsibility.
The managed loader excludes its mounted ESP from `hide[]` so a later hard
failure can remount it briefly for the durable receipt. That exception must
remain protected by platform permissions; it is not a projected endpoint.

APPLY is serialized. A successful APPLY may occur only once per module load;
subsequent calls return `EBUSY`. Failure withdraws any created disk and restores
any saved physical state, leaving QUERY inactive and ready=N; a corrected APPLY
can be retried. QUERY reports ABI version, active status and actual projection
count. ready=Y is published last, after hiding is complete. Module unloading
restores endpoints, removes the synthetic disk, drains I/O and releases resources
in reverse order. The ioctl mutex excludes QUERY from intermediate APPLY state;
normal kernel disk registration can expose transient sysfs/uevents before a
failed scan is rolled back, so callers must not hand off boot on load alone.

## Symbol and verification boundary

The implementation uses Linux 6.12 block APIs and inline flag helpers, not
manual partition creation, a late rescan, symbol allowlists or a raw-sector
interception hook. The Cuttlefish kernel exports every import, so its build
passes normal modpost. The phone kernel intentionally omits these five required
symbols from its KMI while retaining them in `vmlinux`/kallsyms:

```
blk_set_stacking_limits
queue_limits_stack_bdev
blk_crypto_profile_init
blk_crypto_intersect_capabilities
blk_crypto_profile_destroy
```

The shared phone recipe enables `KBUILD_MODPOST_WARN=1` for these deliberate
non-KMI imports and then runs `scripts/phone_modules.py` against the exact
configured output and `vmlinux`. It requires genuine nonempty import versions,
including `module_layout`, checks every available import CRC against the target
`Module.symvers`, and proves every undefined import is defined in that image.
Gpt exports nothing and therefore needs no `__kcrctab`. This exemption never
applies to import versions. The existing kallsyms relocation loader handles the
non-KMI imports; it cannot repair ABI/config or missing export CRCs. See the
repository [build contract](../../README.md#build-notes), including the required
`.ko.compat.json` receipt that must accompany this module during packaging.

Repository validation proves host manifest/config/UAPI behavior; exact
Cuttlefish- and phone-kernel builds; every phone unresolved import against the
exact `vmlinux`; and isolated boot under the exact Cuttlefish kernel covering
module load, generation/readiness, APPLY/QUERY, projected names, mapped reads
and writes, flush, metadata-write rejection, read-only rejection, and unload.
That runtime proof uses disposable RAM backends. Physical UFS endpoint hiding,
restoration, and wrapped-key inline crypto remain phone validation items. Never
run those checks on live storage without explicit authorization.
