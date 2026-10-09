# Cuttlefish integration lane (esu)

This directory contains the Cuttlefish-only esu assembler and the instrumented
ROM 2 OTA proof. `ota_proof.py` prints a PASS/FAIL/BLOCKED table and command
excerpts; its scratch logs are not a lab record. Failed prerequisites never
become device evidence. The older C adapter remains available for its fixed
thin-tuple lane; the managed ROM 2 proof builds the product Rust activator.

## Boundary

| Owned here | Reused from gobbl-lab |
| --- | --- |
| Explicit-input assembly, x86_64 artifact builders, instrumented OTA rows | container/session supervision and pinned CF assets |
| Disposable PV/ESP/identity setup, slot-selection emulation, guest assertions | paused assembly and validated GPT framing |
| Pure helper tests and PASS/FAIL command logs | no lab record creation or physical-device lane |

## Assemble the payload

The lane remains paused; packaging tests do not claim guest compatibility.
All inputs are explicit:

```sh
tools/cuttlefish/assemble.py \
  --stock-init-boot /build/stock/init_boot.img \
  --avbtool /tools/avbtool --avb-key /keys/cuttlefish.pem \
  --esuinit /build/esuinit --esud /build/esud \
  --busybox /build/busybox \
  --thin-activate /build/thin-activate --fw-views /build/fw-views \
  --host-esud /build/host-esud --lvm /build/lvm \
  --lvm-conf tools/lvm2/lvm.conf --ota-stage /build/ota-stage \
  --core-module /build/kernelesp.ko --thin-module /build/thin.ko \
  --gpt-module /build/gpt.ko --efivarfs-module /build/efivarfs.ko \
  --efivar-store-module /build/efivar_store.ko \
  --kmi-out /build/kmi-out --metadata-filesystem ext4 \
  --rom-id rom1 --output-dir /build/cf-payload
```

`--boot-hal /build/gobbl-boot-hal` is the one optional input: supply the built
replacement binary to package the Boot HAL module, or omit it
to assemble a payload without a Boot HAL directory, module-order entry, binary
or required-file check. Every other input is required, and an explicitly
supplied HAL must be a nonempty regular file like the rest. Build it with
`ESU_NDK=/path/to/android-ndk-r29 bash esu/modules/boot-hal/layer/build-android.sh`; the
crate builds `--locked --offline` against the generic-bootctl submodule in
`esu/modules/boot-hal/generic-bootctl` and produces an AArch64 PIE whose only
shared dependencies stay `libbinder_ndk.so`, `libc.so` and `libdl.so`.

All five modules require schema-2 `.ko.compat.json` receipts beside them.
The shared KMI verifier cross-checks x86_64 modules against the CF kernel output.
The host `esud boot-patch` is the producer of the overlay and verified module set.
`--metadata-filesystem` validates the lane's requested ext4/f2fs selection;
it no longer creates a platform staging manifest.

Outputs include `init_boot.img`, `esp.img`, `payload.json`, the canonical
`producer/` tree/receipt, and the preserved `stock-init_boot.img` used as an OTA
source base. The kernel-free v3/v4 stock image must authenticate with the supplied
key. Its legacy-LZ4 stream is preserved and the producer's legacy-LZ4 overlay is
appended without introducing an `init` member.
The daemon lives directly in ESP `esu/bin/esud`; no metadata installation occurs.

The ESP contains schema-1 configuration with module order
`["thin", "ota", "fw-views"]`, prepended with `"boot-hal"` when supplied.
It carries `bin/lvm`, byte-exact `bin/lvm.conf`, `bin/ota-stage`, and the shipped
`ota` scripts/policy. Flag files travel by presence. Module sets reside at
`esu/kmi/android16-6.12-6/`, including `set.json` and compatibility receipts.
With `--boot-hal`, the payload carries the built HAL as `esu/bin/esu-bootctl` and the
Boot HAL module ships `module.prop`, `sepolicy.rule` and `initrc/boot-hal.rc` (its own
`esu.bootctl` init service); nothing is overlaid onto `/vendor`. Without it none of
those paths exist in the ESP.
Kernel modules enter the overlay and the verified ESP KMI module set.

The managed ROM placeholder intentionally has an impossible backend; the lab
must replace it with real projection data. Runtime selection and number are
bdsvars BootedRom/Slot authority, not bootconfig or ROM TOML defaults.

`payload.json` records schema 1, `build_id`, `build_id_inputs`, and each image's
SHA256 and size. Build ID hashes the sorted input SHA256 values (duplicates
retained, each 64hex value followed by LF), taking the first 12 lowercase hex.
Both ESP `esu/build-id` and cpio `/esu-build-id` contain those 12 characters plus
LF. Input hashes include source files, generated configuration/scripts and the
copied module metadata. FAT timestamps need not be reproducible.

Host tools are invoked by argument vector, not a shell: `avbtool`,
`unpack_bootimg`, `mkbootimg`, gzip/lz4 and mtools. `--esp-size-mib`
overrides the default 64 MiB. `--overwrite` replaces only the known assembler
outputs. Temporary files live beside the output and are removed on exit.

## Instrumented ROM 2 OTA proof

Run on this host with a fresh scratch directory:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
export ESU_NDK=/path/to/android-ndk-r29
bash tools/cuttlefish/build-ota-userspace.sh /scratch/cf-artifacts
python3 tools/cuttlefish/build-ota-modules.py --source /build/cf-common \
  --kmi-out /build/cf-out19 --output /scratch/cf-modules
python3 tools/cuttlefish/ota_proof.py --artifacts /scratch/cf-artifacts \
  --modules /scratch/cf-modules --output /scratch/cf-proof
python3 -m unittest tools.cuttlefish.test_assemble tools.cuttlefish.test_ota_proof
ruff check tools/cuttlefish
```

Rebuild `gobbl/target/release/rom-bootgen` from the current checkout with
`cargo build --release --locked -p rom-bootgen` in gobbl (a debug build is not
used by this lane). The userspace builder also builds the upstream
`efivar-store` CLI as `<artifacts>/efvs`. Static x86_64 lvm is taken from
`out/lvm2/x86_64/lvm`; build it with `tools/lvm2/build-android.sh x86_64`.
`--base`, `--fixture`, `--busybox`, `--avb-key`, `--lab`, and `--kmi-out` select
explicit host inputs. The default fixture is the cached signed testkey self-OTA.
Promotion compares each installed base and `esu.cpio` against hashes captured
from the staging LVs and `esu.stage.cpio` before reboot. The second idle boot
also verifies that all three OTA switches have returned to error targets.

The disk helper creates VG `rom` on the disposable userdata component using the
provisioner's extent/reserve arithmetic (including pool metadata/spare), installs
three exact-size stock bases with rom-bootgen, and selects two eligible firmware
A/B pairs from the actual CF GPT. It appends only missing non-image physical
passthrough projections and refuses more than 128 projections. Undeclared image
bases stay stock: a physical backend for `vbmeta_a`, for example, would correctly
fail product admission with `KernelSetBackend`. The product Rust activator
activates every visible LV, including a ROM2-only VG without ROM1 volumes;
the older fixed-geometry C adapter is not used.
The DM table-status decoder follows Linux's output ABI: `next` is relative to
the first target, and the final record's alignment padding is outside
`data_size`. This keeps the real error-to-linear table reload admissible.
Before PID 1 starts, both disposable persistent GPT copies split the 1 MiB `frp`
extent into independent 512 KiB `frp` and `bdsvars` partitions and repair both
CRCs. Stock FRP retains its name, GUID, and original first-half contents; EFVS
uses only the tail under a distinct deterministic GUID. A 512 KiB EFVS image
passes the store's actual geometry admission. This provides identity bootstrap
before projection without exposing Android's FRP service to the EFVS store.
Only the bdsvars half receives offline BootedRom/Slot/Stage/MergeStatus seeds.
Runtime Stage probes use `/dev/efivars`, matching the boot-HAL service, rather
than PID 1's pre-handoff `/sys/firmware/efi/efivars` mount.
The instrumented boot-HAL service execs the unchanged binary with stdout/stderr
in `/dev/esu-bootctl.log`, captured after updater attempts. Unlike kmsg, this
accepts long LVM failure messages without inducing a stderr-write panic.
The assembled `ota` PID1 scripts emit `CF-ESU_STAGE=` into the kernel log before
executing the unchanged product helper, exposing its actual environment.
The thin/ota/fw-views PID1 helpers route stdout/stderr to `/dev/kmsg`, so failures
remain in `kernel.log` even before adb exists. Failed rows reconstruct the ESP
overlay and retain `esp-failure-receipt.json`; downstream rows are `NOT RUN`
with the prerequisite diagnosis and kernel-log path.
`super_2` is seeded with sparse writes, so the 8 GiB CF super's zero ranges do not
consume the smaller reserved thin pool. Its virtual LV size still matches super.
CF's kernel has no devtmpfs, so esuinit creates the loop-control and allocated
loop block nodes from their sysfs device identities before attaching ESP images.
Existing nodes with the wrong type or identity are rejected, never replaced.
Image-role GPT projection access follows the resolved backend, not the writable
install-time role: ESP bases are read-only; a staging switch is read-only when
serving the booted letter and writable only for the non-booted letter.
`thin-activate` assigns LVM's DM UUID to every recreated map: `LVM-` plus the
hyphen-stripped VG/LV ids, with `pool`, `tpool`, `tdata` or `tmeta` layer suffixes
where appropriate. The UUID vectors are pinned to a real host thin-pool/thick-LV
replica. Without them, `lvremove` could release extents while UUID-less maps
still referenced them; the proof retains its strict mapper-absence assertion.
The instrumented OTA PID1 wrapper records the active map names and UUIDs before
creating switches, retaining early-boot identity evidence without requiring adb.
The boot-HAL and OTA policies each declare their shared block-node type before
using it, since policy batches are applied module by module. The kernel policy
interface takes explicit type names, not SELinux's `self` shorthand.
Executable staging admits only the six shipped LVM command symlinks in `bin`,
with relative target `lvm` and a regular staged target; other links remain fatal.
The injected post-fs-data action creates `/data/adb` with stock init's
`encryption=Require` policy before esud writes its log tree, preserving the
stock imported USB init action's later encryption check on fresh userdata.

CF boots stock U-Boot, not Surfacer. Target slot selection is explicitly emulated
by stopping CF and starting with `--boot_slot=b`, preserving transaction disks.
Every direct start marks the owned session active, so the next stop really shuts
down that instance group rather than issuing a second start against a live VM.
No `source=stage`/`source=esp` Surfacer trail is claimed. This instrument uses
permissive boots; enforcing is not a proved row. It exclusively addresses the
container's loopback adb endpoint and always cleans up its owned CF container.
Only other `gbl-shim-` containers are refused. Unrelated CF instances are left
untouched; the session's hashed vsock CID is checked against live crosvm CIDs,
and the host ports are allocated by Docker on loopback.
Each run allocates a private host adb server, exports both server-port settings
and `ADB_SERVER_SOCKET` to every child, and kills only that server at cleanup.
CF transports never enter the default phone adb server.
Before every cold restart the lane rejects changed immutable disk inputs, then
refreshes its own composite timestamp and its existing overlay timestamp last.
Guest-mutated metadata/misc must not trigger stock GPT regeneration; the live
overlay must stay newer than its backing composite so CVD does not reset it.


## `thin-activate`

Built with one bounded NDK invocation:

```sh
ANDROID_NDK_ROOT=/opt/android-ndk tools/cuttlefish/build-thin-activate.sh /tmp/thin-activate
# x86_64-linux-android26-clang -static -O2 -std=gnu11 -Wall -Wextra -Werror
```

The helper reads exactly one key, from `/proc/cmdline` or `/proc/bootconfig`
(disagreement is fatal):

```
androidboot.esu.thin=<PARTUUID>:<metadata sectors>:<data sectors>:<thin id>:<volume sectors>
```

with the pinned lab tuple `...:131072:16646144:1:16777216`, meaning:

| Field | Value | Use |
| --- | --- | --- |
| PARTUUID | userdata GPT partition UUID | resolved by exact PARTUUID from `/sys/class/block` |
| metadata sectors | 131072 | `linear <part> 0` -> `userdata_thin_meta` |
| data sectors | 16646144 | `linear <part> 131072` -> `userdata_thin_data` |
| thin id | 1 | `thin <pool> <id>`; an existing id is reopened by dm-thin |
| volume sectors | 16777216 | length of `userdata_lp` |

It then builds `thin-pool` over the two linear devices with `128`-sector data
blocks, a `128`-block low water mark and `1 skip_block_zeroing` (mandatory in
this fork: a pool that would zero newly provisioned blocks is rejected), and
finally `userdata_lp` as a thin volume of the tuple's length. `linear`,
`thin-pool` and `thin` must be registered with the kernel (ramdisk `thin.ko`
provides the last two) or the helper stops before creating anything.

Failures are one concrete stderr line and a non-zero exit: malformed or
overflowing tuple fields, a PARTUUID that matches no partition, several
matches, a whole-disk match, a geometry exceeding the partition, a missing
device-mapper target, an ioctl error, and a pre-existing `userdata_thin_meta`,
`userdata_thin_data`, `userdata_thin_pool` or `userdata_lp` whose type, length
or parameter string is not exactly this stack. Devices created by a failed run
are removed again; a pre-existing matching stack is reused, so a reboot of an
unchanged pool is idempotent. `/dev/mapper/userdata_lp` is published best
effort: esu resolves the backend name from
`/sys/class/block/dm-*/dm/name`, and Android's ueventd creates the node after
handoff.

## Verification boundary

- Building and inspecting the helper and payload proves only their host-side
  shape; the lab lane must boot the payload and prove the guest-visible result.
- The default `--esp-size-mib=64` is only a default: the lab lane regenerates the
  `cuttlefish_example_custom` GPT entry from the emitted file size, so pass the
  size the payload needs.
- The stock image must be an AVB-signed, kernel-free `init_boot` with header v3
  or v4; the supplied RSA-4096 key must verify it.
