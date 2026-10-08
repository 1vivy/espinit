#!/usr/bin/env bash
# Build the statically linked Android `lvm` that ships in the esu payload as
# `esu/bin/lvm` (LVM2's `lvm` multi-call binary: lvcreate/lvremove/lvs/vgs/pvs
# all arrive as symlinks to it, see esu/modules/ota).
#
#   tools/lvm2/build-android.sh <aarch64|x86_64>
#
# The binary is linked against bionic only (no libc.so at runtime), so it runs
# on any Android userspace and on this glibc host as well.
#
# Everything it downloads and builds lands under kernelesp/out/ (git-ignored):
#   out/lvm2/.cache/          tarballs, sha256-pinned (never rebuilt from scratch)
#   out/lvm2/.work/<arch>/    extracted sources, libaio prefix, build trees
#   out/lvm2/<arch>/lvm       the stripped static binary (the artifact)
#
# Environment:
#   ESU_NDK  NDK root (default: gobbl's cached NDK r29)
#   JOBS     parallel make width (default: nproc)
#
# See README.md next to this script for the patch provenance and the deviations
# from the plan this script was written from.
set -euo pipefail

LVM2_VERSION=2.03.43
LVM2_SHA256=d87ec0dac9061f1fa58ebced5c6b1360c87d4c5cd7b455dc0ebe1d3f036920d7
LVM2_URL=https://sourceware.org/pub/lvm2/releases/LVM2.${LVM2_VERSION}.tgz
LIBAIO_VERSION=0.3.113
LIBAIO_SHA256=2c44d1c5fd0d43752287c9ae1eb9c023f04ef848ea8d4aafa46e9aedb678200b
LIBAIO_URL=https://ftp.debian.org/debian/pool/main/liba/libaio/libaio_${LIBAIO_VERSION}.orig.tar.gz
DEFAULT_NDK=/home/vivy/Projects/efisp-projects/gobbl/.cache/toolchains/android-ndk-r29
API=35

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH= cd -- "$here/../.." && pwd)
out="$root/out/lvm2"
cache="$out/.cache"

die() { printf 'build-android: %s\n' "$*" >&2; exit 1; }
say() { printf '== %s\n' "$*"; }

arch=${1:-}
case "$arch" in
  aarch64 | x86_64) ;;
  *) die "usage: $0 <aarch64|x86_64>" ;;
esac

ndk=${ESU_NDK:-$DEFAULT_NDK}
llvm="$ndk/toolchains/llvm/prebuilt/linux-x86_64"
clang="$llvm/bin/${arch}-linux-android${API}-clang"
[ -x "$clang" ] || die "no NDK clang at $clang (set ESU_NDK)"
jobs=${JOBS:-$(nproc)}

work="$out/.work/$arch"
stage="$out/$arch"
mkdir -p "$cache" "$stage"

fetch() { # url sha256 filename
  local url=$1 sha=$2 name=$3 file="$cache/$3"
  if [ ! -f "$file" ]; then
    say "fetch $url"
    curl -fsSL -o "$file.part" "$url" || die "download failed: $url"
    mv "$file.part" "$file"
  fi
  printf '%s  %s\n' "$sha" "$file" | sha256sum -c - >/dev/null ||
    die "sha256 mismatch: $file (expected $sha)"
}

fetch "$LVM2_URL" "$LVM2_SHA256" "LVM2.$LVM2_VERSION.tgz"
fetch "$LIBAIO_URL" "$LIBAIO_SHA256" "libaio-$LIBAIO_VERSION.tar.gz"

say "extract into $work"
rm -rf "$work"
mkdir -p "$work"
tar -xzf "$cache/LVM2.$LVM2_VERSION.tgz" -C "$work"
tar -xzf "$cache/libaio-$LIBAIO_VERSION.tar.gz" -C "$work"
src="$work/LVM2.$LVM2_VERSION"
[ -d "$src" ] || die "unexpected tarball layout in $cache/LVM2.$LVM2_VERSION.tgz"

for p in "$here"/patches/*.patch; do
  say "apply $(basename "$p")"
  patch -p1 --no-backup-if-mismatch -d "$src" <"$p" || die "patch failed: $p"
done

# libaio is the only external dependency: built static, installed into a private
# prefix so both the header check and the link below find it.
libaio_src="$work/libaio-$LIBAIO_VERSION"
libaio_prefix="$work/libaio-prefix"
say "build libaio $LIBAIO_VERSION (static)"
env CC="$clang" AR="$llvm/bin/llvm-ar" RANLIB="$llvm/bin/llvm-ranlib" \
  CFLAGS="-O2 -fPIC" ENABLE_SHARED=0 \
  make -C "$libaio_src" -j"$jobs"
env CC="$clang" AR="$llvm/bin/llvm-ar" RANLIB="$llvm/bin/llvm-ranlib" \
  CFLAGS="-O2 -fPIC" ENABLE_SHARED=0 \
  make -C "$libaio_src" install prefix="$libaio_prefix"
[ -f "$libaio_prefix/lib/libaio.a" ] || die "libaio.a was not installed"

say "configure LVM2 $LVM2_VERSION for $arch-linux-android"
cd "$src"
# ac_cv_func_{malloc,realloc}_0_nonnull: configure cannot run target binaries, so
# AC_FUNC_MALLOC/AC_FUNC_REALLOC would guess "broken" and #define realloc
# rpl_realloc, a symbol LVM2 2.03.43 no longer provides (there is no lib/replace).
# bionic's malloc(0)/realloc(NULL, 0) return non-NULL, same as glibc, so pinning
# the cache variable is the correct answer, not a workaround.
env CC="$clang" AR="$llvm/bin/llvm-ar" RANLIB="$llvm/bin/llvm-ranlib" \
  NM="$llvm/bin/llvm-nm" STRIP="$llvm/bin/llvm-strip" \
  CPPFLAGS="-I$libaio_prefix/include" \
  AIO_CFLAGS="-I$libaio_prefix/include" \
  AIO_LIBS="-L$libaio_prefix/lib -laio" \
  CFLAGS="-O2 -static" \
  LDFLAGS="-static -L$libaio_prefix/lib" \
  ac_cv_func_realloc_0_nonnull=yes \
  ac_cv_func_malloc_0_nonnull=yes \
  ./configure \
  --host="${arch}-linux-android" \
  --enable-static_link \
  --disable-readline \
  --disable-selinux \
  --disable-blkid_wiping \
  --disable-nvme-wwid \
  --disable-udev_sync \
  --disable-udev_rules \
  --disable-dmeventd \
  --disable-lvmpolld \
  --disable-use-lvmlockd \
  --disable-cmdlib \
  --disable-fsadm \
  --disable-lvmimportvdo \
  --with-default-locking-dir=/dev/block/esd/lock \
  --with-default-run-dir=/dev/block/esd/run \
  --with-default-pid-dir=/dev/block/esd/run \
  --with-confdir=/dev/block/esd/etc \
  --with-default-system-dir=/dev/block/esd/etc

say "make -j$jobs"
make -j"$jobs"

say "install $stage/lvm"
install -D -m 0755 tools/lvm "$stage/lvm"
"$llvm/bin/llvm-strip" "$stage/lvm"

say "verify"
file "$stage/lvm"
dynamic=$("$llvm/bin/llvm-readelf" -d "$stage/lvm")
case "$dynamic" in
  *NEEDED*)
    printf '%s\n' "$dynamic" >&2
    die "$stage/lvm has dynamic dependencies; expected a static link"
    ;;
esac
say "no NEEDED entries: statically linked"
du -h "$stage/lvm"

# Best-effort smoke: the native arch runs directly on this host, the other one
# through qemu if it is installed (a static bionic binary needs nothing else).
runner=()
if [ "$arch" != "$(uname -m)" ]; then
  for candidate in "qemu-${arch}-static" "qemu-${arch}"; do
    if command -v "$candidate" >/dev/null 2>&1; then
      runner=("$candidate")
      break
    fi
  done
fi
if smoke_out=$("${runner[@]}" "$stage/lvm" version 2>&1); then
  smoke_rc=0
else
  smoke_rc=$?
fi
smoke_version=$(printf '%s\n' "$smoke_out" | sed -n '/LVM version/p')
if [ -n "$smoke_version" ]; then
  say "smoke ${runner[*]:-(native)}: $smoke_version"
else
  say "smoke: $stage/lvm did not run on this host (rc=$smoke_rc): $(printf '%s\n' "$smoke_out" | sed -n '1p')"
fi

say "done: $stage/lvm"
