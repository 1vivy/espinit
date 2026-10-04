#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
#
# Build modules/thin/thin.ko against an already configured kernel tree.
#
#   KERNEL_SRC=/path/to/kernel KERNEL_OUT=/path/to/out modules/thin/build.sh
#
# KERNEL_SRC and KERNEL_OUT are required: thin.ko is a separate module built out
# of tree and never inside the kernel sources. KERNEL_OUT must be the output
# directory of a configured tree (defconfig plus `make modules_prepare`).
#
# JOBS (default 13, maximum 13) and ARCH (default arm64) are forwarded to
# modules/thin/Makefile, which owns their validation. ESPINIT_GENERATION is
# forwarded when set; the Makefile derives it from this repository's Git HEAD
# otherwise and rejects any value that is not 1-63 ASCII letters/digits/._-
# before compiling.
set -eu

KERNEL_SRC=${KERNEL_SRC:-}
KERNEL_OUT=${KERNEL_OUT:-}

if [ -z "$KERNEL_SRC" ] || [ -z "$KERNEL_OUT" ]; then
	echo "espinit thin: KERNEL_SRC and KERNEL_OUT are required" >&2
	echo "usage: KERNEL_SRC=<configured kernel tree> KERNEL_OUT=<build output dir> [JOBS=1..13] [ARCH=arm64] $0" >&2
	exit 2
fi

if [ ! -d "$KERNEL_SRC" ]; then
	echo "espinit thin: KERNEL_SRC is not a directory: $KERNEL_SRC" >&2
	exit 2
fi

if [ ! -f "$KERNEL_SRC/Makefile" ]; then
	echo "espinit thin: KERNEL_SRC does not look like a kernel source tree: $KERNEL_SRC" >&2
	exit 2
fi

MODULE_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

make -C "$MODULE_DIR" \
	KERNEL_SRC="$KERNEL_SRC" KERNEL_OUT="$KERNEL_OUT" \
	JOBS="${JOBS:-13}" ARCH="${ARCH:-arm64}" modules

echo "espinit thin: built $MODULE_DIR/thin.ko"
