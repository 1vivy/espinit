#!/bin/bash
# Build CF Android binaries in an isolated target directory. No device access.
set -euo pipefail
ROOT=$(realpath "$(dirname "$0")/../..")
OUT=$(realpath -m "${1:?output directory required}")
NDK=${ESU_NDK:?set ESU_NDK to Android NDK r29}
LLVM=$NDK/toolchains/llvm/prebuilt/linux-x86_64
export PATH="$HOME/.cargo/bin:$LLVM/bin:$PATH"
export CARGO_TARGET_DIR=$OUT/target
export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER=$LLVM/bin/x86_64-linux-android35-clang
export CC_x86_64_linux_android=$CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER
export CXX_x86_64_linux_android=$LLVM/bin/x86_64-linux-android35-clang++
export AR_x86_64_linux_android=$LLVM/bin/llvm-ar
export BINDGEN_EXTRA_CLANG_ARGS_x86_64_linux_android="--sysroot=$LLVM/sysroot -I$LLVM/sysroot/usr/include/x86_64-linux-android"
BUILTINS=$($CC_x86_64_linux_android --print-resource-dir)/lib/linux/libclang_rt.builtins-x86_64-android.a
mkdir -p "$OUT"
if [ "${2:-all}" != ota-only ]; then
export RUSTFLAGS="-C target-feature=+crt-static -C link-arg=-Wl,-z,max-page-size=16384 -C link-arg=$BUILTINS"
cargo build --manifest-path "$ROOT/Cargo.toml" --locked --release --target x86_64-linux-android -p esuinit -p thin-activate -p fw-views
for name in esuinit thin-activate fw-views; do cp "$OUT/target/x86_64-linux-android/release/$name" "$OUT/$name"; done
export RUSTFLAGS="-C link-arg=-Wl,-z,relro,-z,now -C link-arg=$BUILTINS"
cargo build --manifest-path "$ROOT/Cargo.toml" --locked --release --target x86_64-linux-android -p esud
cp "$OUT/target/x86_64-linux-android/release/esud" "$OUT/esud"
unset RUSTFLAGS
cargo build --manifest-path "$ROOT/Cargo.toml" --locked --release -p esud
cp "$OUT/target/release/esud" "$OUT/host-esud"
bash "$ROOT/esu/modules/boot-hal/layer/build-android.sh" x86_64
cp "$OUT/target/x86_64-linux-android/release/gobbl-boot-hal" "$OUT/esu-bootctl"
cp "$ROOT/out/lvm2/x86_64/lvm" "$OUT/lvm"
fi
export RUSTFLAGS="-C target-feature=+crt-static -C link-arg=-Wl,-z,max-page-size=16384 -C link-arg=$BUILTINS"
cargo build --manifest-path "$ROOT/Cargo.toml" --locked --release --target x86_64-linux-android -p ota-stage
cp "$OUT/target/x86_64-linux-android/release/ota-stage" "$OUT/ota-stage"
if [ "${2:-all}" != ota-only ]; then
    unset RUSTFLAGS
    cargo build --locked --offline --release --manifest-path "$ROOT/modules/efivar_store/.source/cli/Cargo.toml"
    cp "$OUT/target/release/efivar-store" "$OUT/efvs"
fi
