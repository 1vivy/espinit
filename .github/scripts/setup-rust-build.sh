# shellcheck shell=bash
# Source from Bash with the Rust target triple and Android API level as arguments.
# ANDROID_NDK_HOME must point to the NDK root.
TRIPLE=$1
ANDROID_SDK_LEVEL=$2
LLVM_PATH="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64"
LLVM_BIN="$LLVM_PATH/bin"
CLANG_PATH="$LLVM_BIN/${TRIPLE}${ANDROID_SDK_LEVEL}-clang"
CLANGXX_PATH="$LLVM_BIN/${TRIPLE}${ANDROID_SDK_LEVEL}-clang++"
UTRIPLE="$(echo $TRIPLE | sed 's/-/_/g')"
UUTRIPLE="$(echo $UTRIPLE | tr a-z A-Z)"

# `export NAME_$suffix=value` is not reliably parsed as an assignment word, so the
# dynamic names are built with printf -v and then exported by name.
printf -v "CC_$UTRIPLE" '%s' "$CLANG_PATH"
printf -v "CXX_$UTRIPLE" '%s' "$CLANGXX_PATH"
printf -v "AR_$UTRIPLE" '%s' "$LLVM_BIN/llvm-ar"
printf -v "CARGO_TARGET_${UUTRIPLE}_LINKER" '%s' "$CLANG_PATH"
printf -v "BINDGEN_EXTRA_CLANG_ARGS_$UTRIPLE" '%s' "--sysroot=$LLVM_PATH/sysroot -I$LLVM_PATH/sysroot/usr/include/$TRIPLE"
export "CC_$UTRIPLE" "CXX_$UTRIPLE" "AR_$UTRIPLE"
export "CARGO_TARGET_${UUTRIPLE}_LINKER" "BINDGEN_EXTRA_CLANG_ARGS_$UTRIPLE"
