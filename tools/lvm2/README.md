# tools/lvm2 — static Android `lvm` for the esu payload

**Status (2026-10-08)** — builds `out/lvm2/{aarch64,x86_64}/lvm`, a statically
linked LVM2 2.03.43 binary for bionic. The esu payload ships it as
`esu/bin/lvm` together with `tools/lvm2/lvm.conf` (copied verbatim to
`esu/bin/lvm.conf`); the OTA module symlinks `lvcreate`, `lvremove`, `lvchange`,
`lvs`, `vgs`, `pvs` onto it. It is what creates and removes the per-OTA thick
staging LVs (`rom<n>-stage-<base>`) inside the `rom` VG, so it must run from the
esu PID 1 / esud context without any Android userspace library.

```
tools/lvm2/build-android.sh aarch64     # or x86_64
```

Output: `out/lvm2/<arch>/lvm` (stripped, ~5 MiB). `out/` is git-ignored, and so
are the download cache `out/lvm2/.cache/` and the build trees
`out/lvm2/.work/<arch>/`. `ESU_NDK` overrides the NDK root (default
`/home/vivy/Projects/efisp-projects/gobbl/.cache/toolchains/android-ndk-r29`,
`<arch>-linux-android35-clang`); `JOBS` overrides `nproc`.

## Sources

| source | version | sha256 | URL |
| --- | --- | --- | --- |
| LVM2 | 2.03.43 | `d87ec0dac9061f1fa58ebced5c6b1360c87d4c5cd7b455dc0ebe1d3f036920d7` | `https://sourceware.org/pub/lvm2/releases/LVM2.2.03.43.tgz` |
| libaio | 0.3.113 | `2c44d1c5fd0d43752287c9ae1eb9c023f04ef848ea8d4aafa46e9aedb678200b` | `https://ftp.debian.org/debian/pool/main/liba/libaio/libaio_0.3.113.orig.tar.gz` |

Both hashes are enforced by the script; the LVM2 one is the value the plan and
termux-packages pin. The libaio hash is for the Debian `orig.tar.gz` (a repack of
the upstream 0.3.113 tarball, root directory `libaio-0.3.113/`); upstream's own
`pagure.io` archive has a different byte stream, so the Debian mirror is the
pinned source here. libaio is built with `ENABLE_SHARED=0` and installed into
`out/lvm2/.work/<arch>/libaio-prefix/`; LVM2 is pointed at it with
`CPPFLAGS`/`AIO_CFLAGS`/`AIO_LIBS`.

## Patches

Applied in filename order, all from `patches/`.

- `0001-termux-fix-stack.patch` — termux-packages
  `root-packages/lvm2/fix-stack.patch`, commit
  `b63c64cfffa9cd44cd9ab45b8df7fd38c86684d0` (2026-09-30, "bump(root/lvm2):
  2.03.43"), <https://raw.githubusercontent.com/termux/termux-packages/b63c64cfffa9cd44cd9ab45b8df7fd38c86684d0/root-packages/lvm2/fix-stack.patch>.
  Byte-identical to upstream. It renames the `stack` macro in
  `tools/man-generator.c`; it applies with fuzz 2 (upstream's copy of the file
  has moved on since the patch was written, the hunk still lands on the
  `#define stack` line).
- `0002-bionic-rename-stack-macro.patch` — generated here, but the same
  transformation termux-packages performs in `root-packages/lvm2/build.sh`
  `termux_step_pre_configure` at the commit above
  (<https://raw.githubusercontent.com/termux/termux-packages/b63c64cfffa9cd44cd9ab45b8df7fd38c86684d0/root-packages/lvm2/build.sh>):
  replace every standalone `stack` token in every `*.c`/`*.h` with `log_stack`.
  477 replacements in 88 files. Vendored as a patch so the build is offline,
  auditable and identical for both arches.

  Why it is needed: `lib/log/log.h:111` defines `#define stack
  log_debug("<backtrace>")` as an object-like macro, and bionic's
  `sysroot/usr/include/linux/sched.h` declares `struct clone_args { …
  __aligned_u64 stack; }`. Any translation unit that includes `log.h` and then
  `<linux/sched.h>` (all of libdm, through `pthread.h` → `sched.h`) fails to
  compile: `error: expected parameter declarator` on that field. Renaming the
  macro is the only fix that keeps both sides intact.

No other termux lvm2 patch exists — `root-packages/lvm2/` contains only
`build.sh`, `fix-stack.patch` and `libdevmapper.subpackage.sh`.

## Configure flags

Every flag below was confirmed against `./configure --help` of LVM2 2.03.43
before use. The plan's flag list needed three corrections:

- `--disable-lvmlockd` **does not exist** (there is no `enable_lvmlockd`
  variable). Replaced by the documented `--disable-use-lvmlockd`; lvmlockd
  itself is only built by `--enable-lvmlockd-{sanlock,dlm,idm}`, all off by
  default.
- `--disable-nvme-wwid` **added**: without it configure fails with
  `--enable-nvme-wwid requires libnvme library >= 1.1`, because the host
  pkg-config finds a glibc libnvme that the target could never link against.
- `--disable-udev_sync`, `--disable-udev_rules`, `--disable-dmeventd`,
  `--disable-lvmpolld`, `--disable-cmdlib` are undocumented but valid: configure
  defines `enable_udev_sync`, `enable_udev_rules`, `enable_dmeventd`,
  `enable_lvmpolld`, `enable_cmdlib`, so the `--disable-` negation is accepted
  and yields the intended `no`. Kept as the plan wrote them.
- `--enable-static_link`, `--disable-readline`, `--disable-selinux`,
  `--disable-blkid_wiping`, `--disable-fsadm`, `--disable-lvmimportvdo`,
  `--with-default-locking-dir`, `--with-default-run-dir`,
  `--with-default-pid-dir`, `--with-confdir`, `--with-default-system-dir` are
  exactly as the plan lists them; all defaults under `/dev/block/esd/`
  (`lock`, `run`, `etc`).

Two things the plan's build line did not carry, both required for the link:

- `CPPFLAGS`/`AIO_CFLAGS`/`AIO_LIBS` for libaio. `lib/device/bcache.c` includes
  `<libaio.h>` unconditionally, and LVM2 only looks for it through
  `CPPFLAGS`/`AIO_CFLAGS`; `-L<libaio>` alone is not enough.
- `ac_cv_func_realloc_0_nonnull=yes ac_cv_func_malloc_0_nonnull=yes`. configure
  cannot run target binaries, so `AC_FUNC_REALLOC` guesses "broken" and emits
  `#define realloc rpl_realloc` — a symbol LVM2 2.03.43 no longer provides
  (there is no `lib/replace/`), which fails the link of every tool. bionic's
  `malloc(0)`/`realloc(NULL, 0)` return non-NULL exactly like glibc's, so pinning
  the cache variable is the correct answer rather than a workaround.

`CFLAGS="-O2 -static"`/`LDFLAGS="-static -L<libaio>"` follow the plan;
`-O2` is LVM2's own default optimisation made explicit. `--with-symvers` is left
at its default (`gnu`) — symbol versioning is compatible with the static link.

Known cosmetic leftovers: configure autodetects *host* helper binaries
(`thin_check`, `cache_check`, `cache_repair`, `vdoformat`) and bakes their paths
into the binary; the OTA path only ever creates/removes linear thick LVs, so
those helpers are never executed. `--disable-blkdeactivate` is not passed, so
configure still generates the (uninstalled) shell script.

## Verification

```
bash tools/lvm2/build-android.sh x86_64
bash tools/lvm2/build-android.sh aarch64
file out/lvm2/*/lvm                     # statically linked, ARM aarch64 / x86-64
readelf -d out/lvm2/*/lvm               # no NEEDED entries
out/lvm2/x86_64/lvm version             # 2.03.43, runs on a glibc host
qemu-aarch64-static out/lvm2/aarch64/lvm version
```

The script itself asserts the static link (no `NEEDED`) and prints the smoke
output of `lvm version` for both arches. Observed on 2026-10-08 with NDK r29:

```
$ file out/lvm2/aarch64/lvm out/lvm2/x86_64/lvm
out/lvm2/aarch64/lvm: ELF 64-bit LSB executable, ARM aarch64, version 1 (SYSV), statically linked, for Android 35, built by NDK r29 (14206865), stripped
out/lvm2/x86_64/lvm:  ELF 64-bit LSB executable, x86-64, version 1 (SYSV), statically linked, for Android 35, built by NDK r29 (14206865), stripped
$ readelf -d out/lvm2/x86_64/lvm
There is no dynamic section in this file.
$ out/lvm2/x86_64/lvm version            # runs natively on a glibc host
  LVM version:     2.03.43(2) (2026-09-30)
  Library version: 1.02.217 (2026-09-30)
$ qemu-aarch64-static out/lvm2/aarch64/lvm version
  LVM version:     2.03.43(2) (2026-09-30)
```

`qemu-aarch64-static` is installed here; `qemu-x86_64` is not needed because the
x86_64 binary is native. Both are ~2.5 MiB stripped. The shipped `lvm.conf` was
also round-tripped through both binaries:

```
$ lvm dumpconfig --type current --config "$(<tools/lvm2/lvm.conf)" \
    devices/use_devicesfile devices/dir devices/scan devices/filter \
    activation/udev_sync activation/udev_rules \
    activation/verify_udev_operations activation/monitoring global/use_lvmlockd
use_devicesfile=0
dir="/dev/block/esd"
scan="/dev/block/esd/pv"
filter=["a|^/dev/block/esd/pv/a$|","r|.*|"]
udev_sync=0
udev_rules=0
verify_udev_operations=0
monitoring=0
use_lvmlockd=0
```

On the phone the plan's ladder pushes `out/lvm2/aarch64/lvm` and expects
`lvm version` to print `2.03.43`.
