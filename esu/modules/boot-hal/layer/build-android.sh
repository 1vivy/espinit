#!/usr/bin/env bash
# Builds the esu boot-control HAL for one Android architecture: the esu layer in
# this directory linked against the generic-bootctl submodule (../generic-bootctl,
# pinned by the gitlink). Path dependencies only: --locked --offline needs no
# network or extra tree.
#
#   build-android.sh [aarch64|x86_64]      # aarch64 is the default (the phone)
#
# x86_64 is what the Cuttlefish proof lane builds; the same manifest, the same
# flags, only the target triple and the NDK compiler differ.
set -euo pipefail
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
arch=${1:-aarch64}
case "$arch" in
    aarch64 | x86_64) ;;
    *)
        echo "unknown architecture: $arch (aarch64 or x86_64)" >&2
        exit 2
        ;;
esac
triple="$arch-linux-android"
: "${ESU_NDK:?Set ESU_NDK to an Android NDK with API 35}"
ndk=$ESU_NDK
llvm="$ndk/toolchains/llvm/prebuilt/linux-x86_64"
clang="$llvm/bin/$triple""35-clang"
test -x "$clang"
upper=$(printf '%s' "$triple" | tr 'a-z-' 'A-Z_')
export "CARGO_TARGET_${upper}_LINKER=$clang"
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-3}
# -llog resolves __android_log_print, the tag the denial path logs under.
export RUSTFLAGS="-Dwarnings -C link-arg=-Wl,-z,relro,-z,now -C link-arg=-llog"
exec "${CARGO:-$HOME/.cargo/bin/cargo}" +nightly-2026-08-08 build \
    --manifest-path "$here/Cargo.toml" --locked --offline --release \
    --target "$triple" --jobs "$CARGO_BUILD_JOBS"
