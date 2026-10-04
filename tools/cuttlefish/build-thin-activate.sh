#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-only
#
# Build tools/cuttlefish/thin-activate.c as the static x86_64 Android helper the
# Cuttlefish payload carries at bin/thin-activate inside the ESP image.
#
#   ANDROID_NDK_ROOT=/path/to/ndk tools/cuttlefish/build-thin-activate.sh [out]
#
# The output path defaults to tools/cuttlefish/thin-activate; pass a path to
# keep the checkout clean. This is deliberately one bounded clang invocation,
# not a build system: no Bazel, no Gradle, no Cargo, no target matrix. Only the
# NDK sysroot is used, which is what makes the result a self-contained Android
# static binary for an initramfs payload. The API level (26) matches the one the
# Rust userspace targets in .cargo/config.example.toml.
#
# This script builds the helper only. It says nothing about whether the payload
# assembles or boots; see tools/cuttlefish/README.md for that boundary.
set -eu

NDK=${ANDROID_NDK_ROOT:-${ANDROID_NDK_HOME:-${ANDROID_NDK:-}}}

if [ -z "$NDK" ]; then
	echo "thin-activate: ANDROID_NDK_ROOT is required" >&2
	echo "usage: ANDROID_NDK_ROOT=<ndk> $0 [output-path]" >&2
	exit 2
fi

HOST_TAG=linux-x86_64
if [ "$(uname -s)" = "Darwin" ]; then
	HOST_TAG=darwin-x86_64
fi

CLANG="$NDK/toolchains/llvm/prebuilt/$HOST_TAG/bin/x86_64-linux-android26-clang"

if [ ! -x "$CLANG" ]; then
	echo "thin-activate: no x86_64 Android clang at $CLANG" >&2
	exit 2
fi

TOOL_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
OUTPUT=${1:-"$TOOL_DIR/thin-activate"}

"$CLANG" \
	-static -O2 -std=gnu11 -Wall -Wextra -Werror \
	-o "$OUTPUT" "$TOOL_DIR/thin-activate.c"

echo "thin-activate: built $OUTPUT"
