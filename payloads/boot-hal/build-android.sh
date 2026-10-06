#!/usr/bin/env bash
set -euo pipefail
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
: "${ESU_NDK:?Set ESU_NDK to an Android NDK with API 35}"
ndk=$ESU_NDK
llvm="$ndk/toolchains/llvm/prebuilt/linux-x86_64"
test -x "$llvm/bin/aarch64-linux-android35-clang"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$llvm/bin/aarch64-linux-android35-clang"
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-3}
export RUSTFLAGS="-Dwarnings -C link-arg=-Wl,-z,relro,-z,now"
exec "${CARGO:-$HOME/.cargo/bin/cargo}" +nightly-2026-08-08 build \
    --manifest-path "$here/Cargo.toml" --locked --offline --release \
    --target aarch64-linux-android --jobs "$CARGO_BUILD_JOBS"
