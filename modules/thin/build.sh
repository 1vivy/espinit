#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
#
# Build modules/thin/thin.ko against an already configured kernel tree.
#
#   KMI_SRC=<source> KMI_OUT=<output> modules/thin/build.sh
#
# KMI_SRC/KMI_OUT identify the prepared android16-6.12 generation-6 build.
# Complete Module.symvers and System.map are required; modules_prepare alone
# cannot supply ABI CRCs.
#
# JOBS (default 13, maximum 13) is forwarded; architecture is arm64.
# ESU_GENERATION is
# forwarded when set; the Makefile derives it from this repository's Git HEAD
# otherwise and rejects any value that is not 1-63 ASCII letters/digits/._-
# before compiling.
set -eu

KMI_SRC=${KMI_SRC:-}
KMI_OUT=${KMI_OUT:-}

if [ -z "$KMI_SRC" ] || [ -z "$KMI_OUT" ]; then
	echo "esu thin: KMI_SRC and KMI_OUT are required" >&2
	echo "usage: KMI_SRC=<source> KMI_OUT=<output> [JOBS=1..13] $0" >&2
	exit 2
fi

if [ ! -d "$KMI_SRC" ]; then
	echo "esu thin: KMI_SRC is not a directory: $KMI_SRC" >&2
	exit 2
fi

if [ ! -f "$KMI_SRC/Makefile" ]; then
	echo "esu thin: KMI_SRC does not look like a kernel source tree: $KMI_SRC" >&2
	exit 2
fi

MODULE_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

make -C "$MODULE_DIR" \
	KMI_SRC="$KMI_SRC" KMI_OUT="$KMI_OUT" \
	JOBS="${JOBS:-13}" modules

echo "esu thin: built $MODULE_DIR/thin.ko"
