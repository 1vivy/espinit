#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
#
# Build modules/thin/thin.ko against an already configured kernel tree.
#
#   KERNEL_SRC=<source> KERNEL_OUT=<output> KERNEL_CONFIG=<capture> modules/thin/build.sh
#
# KERNEL_SRC/KERNEL_OUT identify the exact prepared phone build; KERNEL_CONFIG
# is an independent capture of that phone's full config. A complete vmlinux and
# Module.symvers are required: modules_prepare alone cannot supply ABI CRCs.
#
# JOBS (default 13, maximum 13) is forwarded to the shared recipe; architecture
# comes from the exact target config. ESPINIT_GENERATION is
# forwarded when set; the Makefile derives it from this repository's Git HEAD
# otherwise and rejects any value that is not 1-63 ASCII letters/digits/._-
# before compiling.
set -eu

KERNEL_SRC=${KERNEL_SRC:-}
KERNEL_OUT=${KERNEL_OUT:-}
KERNEL_CONFIG=${KERNEL_CONFIG:-}

if [ -z "$KERNEL_SRC" ] || [ -z "$KERNEL_OUT" ] || [ -z "$KERNEL_CONFIG" ]; then
	echo "espinit thin: KERNEL_SRC, KERNEL_OUT and KERNEL_CONFIG are required" >&2
	echo "usage: KERNEL_SRC=<exact source> KERNEL_OUT=<exact output> KERNEL_CONFIG=<phone config capture> [JOBS=1..13] $0" >&2
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
	KERNEL_SRC="$KERNEL_SRC" KERNEL_OUT="$KERNEL_OUT" KERNEL_CONFIG="$KERNEL_CONFIG" \
	JOBS="${JOBS:-13}" modules

echo "espinit thin: built $MODULE_DIR/thin.ko"
