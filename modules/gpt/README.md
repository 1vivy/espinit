# In-memory GPT projection

This GPL-2.0-only external module targets Linux 6.12. Build against the exact
kernel output and exported-symbol metadata used by the payload:

```sh
make -C modules/gpt KERNEL_SRC=/path/to/exact/source KERNEL_OUT=/path/to/exact/output \
    KERNEL_CONFIG=/path/to/captured-phone.config ESPINIT_GENERATION=payload-generation
```

Load `gpt.ko` without a generation override. `/dev/gptctl` is a root-only misc
device; both ioctls also require `CAP_SYS_ADMIN`. `generation`, `ready` and
`sealed` are read-only sysfs parameters. The generation is compiled from
`ESPINIT_GENERATION`
(1–63 ASCII letters/digits/._-), or the full 40-byte lowercase Git HEAD when unset,
using the same validation as `kernel/Kbuild`. Size assertions pin its storage;
load-time parameters that change the compiled identity are rejected. Loading
does not create a disk or assert readiness.

## ABI v2

`gpt_uapi.h` is canonical. Structures contain fixed-width fields and no pointers,
and the 32-bit compat ioctl uses the identical wire layout. Compile-time checks
pin the 56-byte projection, 8-byte device, 9232-byte APPLY, 16-byte QUERY and
ioctl values (`0x64104701`, `0x80104702`). APPLY requires version 2, 1–128
projections, at most 256 hidden devices, only the documented flag bits (zero or
`GPT_APPLY_FLAG_SEAL`), and zero reserved fields and unused array slots. Names
are nonempty NUL-terminated printable ASCII (at most 36 bytes), with zero
trailing bytes. Projection names and backend device numbers must each be unique;
hidden device numbers must be unique within hide[]. A physical partition may
appear in both arrays, intentionally.

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
clear every supplied physical partition's PARTNAME. No physical GPT sectors are
written. Original names are saved and restored on rollback/unload. The physical
disk open mutex protects each endpoint update. Hide entries must actually be
partitions; a raw LU is rejected in `hide[]`.

Hidden physical partitions retain their native access mode unless the APPLY
requests the seal below. A projected DM/LVM backend can resolve through the same
physical bdev, and `BD_READ_ONLY` is global to that bdev: setting it on the
hidden PV would reject projected writes too. This is **not a raw-LU firewall**:
parent logical units remain accessible, and
already-open clients are not revoked. Enumeration, dependency activation and
exclusion of competing storage users are the managed boot caller's
responsibility.
The managed loader excludes its mounted ESP from `hide[]` so a later hard
failure can remount it briefly for the durable receipt. That exception must
remain protected by platform permissions; it is not a projected endpoint.

### Sealing

`GPT_APPLY_FLAG_SEAL` makes the physical storage this view does not project
read-only for the life of the view. It runs last, after the whole view is live
and every PARTNAME is hidden; a seal that cannot be applied completely fails the
whole APPLY. The candidate set is the storage the view already references: every
projection backend (a partition backend contributes its whole disk) and every
hidden physical partition. The phone kernel does not export the block class a
complete gendisk walk would need, so the module deliberately seals exactly the
disks it was handed; a managed ROM references every physical partition it hides,
and an unreferenced disk is left untouched.

A candidate disk that is not the lower disk of a writable projection becomes
read-only at the gendisk through `set_disk_ro()`. That sets `GD_READ_ONLY`,
which `BLKROSET` cannot clear, so the disk can no longer be opened for writing;
`ro` reports 1 for the disk and every one of its partitions. A candidate disk
that stays writable keeps its partitions writable except for those that are
neither a writable backend nor owned by another subsystem: a partition with an
exclusive holder is skipped, which keeps the dm-held LVM physical volume usable
under the thin pool that projects it. Those partitions take the per-bdev
`BD_READ_ONLY` flag.

Both levels are recorded and undone in reverse when the view is destroyed, and
`sealed` reports 1 exactly while they are in force. Two documented limits:
`bio_check_ro()` only warns, so a client that opened a device before the seal
keeps writing through that open; and `BD_READ_ONLY` is a per-bdev flag that root
can clear with `BLKROSET`, so it is friction rather than a boundary.
`GD_READ_ONLY` has no such ioctl.

APPLY is serialized. A successful APPLY may occur only once per module load;
subsequent calls return `EBUSY`. Failure withdraws any created disk and restores
any saved physical state, leaving QUERY inactive and ready=N; a corrected APPLY
can be retried. QUERY reports ABI version, active status and actual projection
count. ready=Y is published last, after hiding and sealing are complete. Module
unloading restores endpoints, removes the synthetic disk, drains I/O and releases
resources in reverse order. The ioctl mutex excludes QUERY from intermediate
APPLY state;
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
That runtime proof uses disposable RAM backends. A second, sealed APPLY covers
the seal: a referenced disk that is not a writable backend reports `ro=1` in
sysfs and rejects a later write, `sealed` reports 1, and unloading the module
restores `ro=0` and `sealed=0`. Physical UFS endpoint hiding,
restoration, and wrapped-key inline crypto remain phone validation items. Never
run those checks on live storage without explicit authorization.

### RAM-backend runtime smoke

The runtime proof is one disposable QEMU boot of the exact Cuttlefish kernel, so
it never touches physical storage:

1. Build `gpt.ko` against the kernel output whose `Module.symvers` matches the
   booted `bzImage`; `Module.symvers` is the CRC source, so a nearby build of the
   same release is rejected at load with `disagrees about version of symbol
   module_layout`.
2. Boot that `bzImage` under QEMU with an initramfs holding a static `/init`, the
   module, `/dev/console` and `/dev/ttyS0`. The Cuttlefish built-in command line
   keeps `ttynull` as the preferred console and the 8250 driver does not register
   `ttyS0`, so the harness reports through `/dev/kmsg`; add `printk.devkmsg=on`,
   because the per-open kmsg ratelimit otherwise silently drops later records.
3. The harness loads the module and issues one sealed APPLY over RAM disks: a
   writable projection, a read-only projection, and one partition of the writable
   projection's disk. It requires the exact QUERY count, `ready=Y`, `sealed=1`,
   `ro=1` for the read-only projection's disk and for the unprojected partition,
   `ro=0` for the writable projection's disk, a refused write to each sealed
   endpoint (the open succeeds, `write` returns `EPERM` from `blkdev_write_iter`)
   while a write to the writable projection succeeds, and `ro=0` again for both
   endpoints after the module is unloaded.
4. A second module load applies 128 projections in one APPLY and requires QUERY
   to report all of them.
