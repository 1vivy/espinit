# SPDX-License-Identifier: GPL-2.0-only
#
# espinit thin.ko -- separate GPL-2.0-only device-mapper thin-provisioning
# module vendored from the Linux kernel dm-thin family (see PROVENANCE.md).
#
# The module is always built out of tree against an already configured kernel
# tree, so the kernel sources and their build output directory are provided by
# the caller:
#
#   make -C modules/thin KERNEL_SRC=<source> KERNEL_OUT=<output> KERNEL_CONFIG=<capture>
#
# modules/thin/build.sh wraps the same invocation for scripted use. JOBS
# defaults to 13 and is capped at 13; architecture follows the target config.

# Build generation identity
#
# The generation string identifies one complete, coordinated ESP payload: the
# PID-1 stage, the espinit core module, the espinitd daemon and every ESP module
# report the exact same value. It is taken from ESPINIT_GENERATION when set,
# otherwise from the full 40-byte lowercase Git HEAD hash of this repository,
# and it must be a nonempty ASCII token so it can be embedded into a C string
# literal unescaped.
#
# Keep this logic in sync with kernel/Kbuild, userspace/ksud/build.rs and
# userspace/ksuinit/build.rs.
#
# Capture the raw value once: environment/command-line values must not be
# expanded as Make expressions. Single-quote escaping below also keeps them
# data, not shell source, while validating.
MDIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
GIT := $(shell PATH="$$PATH":/usr/bin:/usr/local/bin which git)

ifneq ($(origin ESPINIT_GENERATION),undefined)
ESPINIT_GENERATION_VALUE := $(value ESPINIT_GENERATION)
ESPINIT_GENERATION_SOURCE := environment
else
ESPINIT_GENERATION_SOURCE := git HEAD
ifeq ($(GIT),)
$(error espinit thin: cannot derive the build generation: ESPINIT_GENERATION is unset and git was not found in PATH. Set ESPINIT_GENERATION explicitly when building from a source tarball.)
endif
ESPINIT_GENERATION_VALUE := $(shell cd '$(subst ','"'"',$(MDIR))' && '$(subst ','"'"',$(GIT))' rev-parse HEAD 2>/dev/null)
ESPINIT_GENERATION_GIT_VALID := $(shell generation='$(subst ','"'"',$(ESPINIT_GENERATION_VALUE))'; LC_ALL=C; export LC_ALL; invalid=`printf '%s' "$$generation" | tr -d '0-9a-f'`; if [ "$${#generation}" -eq 40 ] && [ -z "$$invalid" ]; then echo yes; else echo no; fi)
ifneq ($(ESPINIT_GENERATION_GIT_VALID),yes)
$(error espinit thin: cannot derive the build generation: ESPINIT_GENERATION is unset and git rev-parse HEAD failed or did not return 40 lowercase hex bytes. Set ESPINIT_GENERATION explicitly when building from a source tarball.)
endif
endif

# Do not rely on export reaching $(shell ...): Make does not export newly
# assigned variables to that expansion on all supported versions. Validate a
# safely quoted literal, and reject every invalid byte (including newlines).
ESPINIT_GENERATION_VALID := $(shell generation='$(subst ','"'"',$(ESPINIT_GENERATION_VALUE))'; LC_ALL=C; export LC_ALL; invalid=`printf '%s' "$$generation" | tr -d 'A-Za-z0-9._-'`; if [ -n "$$generation" ] && [ "$${#generation}" -le 63 ] && [ -z "$$invalid" ]; then echo yes; else echo no; fi)
ifneq ($(ESPINIT_GENERATION_VALID),yes)
$(error espinit thin: invalid ESPINIT_GENERATION: expected 1-63 ASCII letters/digits/._-; unset it to derive the full Git HEAD hash)
endif

$(info -- espinit thin generation: $(ESPINIT_GENERATION_VALUE) (from $(ESPINIT_GENERATION_SOURCE)))

ifneq ($(KERNELRELEASE),)

# The object name decides the module name: thin.ko loads as `thin`, which is
# the logical name the ESP manifest must use. Internal symbols keep the proven
# thinpool_private_* names and the device-mapper targets keep the proven
# `thin-pool` and `thin` names; only the module file and its metadata change.
obj-m := thin.o

thin-y := thin-main.o \
	src/dm-io.o \
	src/dm-kcopyd.o \
	src/dm-bufio.o \
	src/dm-bio-prison-v1.o \
	src/dm-bio-prison-v2.o \
	src/persistent-data/dm-array.o \
	src/persistent-data/dm-bitset.o \
	src/persistent-data/dm-block-manager.o \
	src/persistent-data/dm-space-map-common.o \
	src/persistent-data/dm-space-map-disk.o \
	src/persistent-data/dm-space-map-metadata.o \
	src/persistent-data/dm-transaction-manager.o \
	src/persistent-data/dm-btree.o \
	src/persistent-data/dm-btree-remove.o \
	src/persistent-data/dm-btree-spine.o \
	src/dm-thin-metadata.o \
	src/dm-thin.o

ccflags-y += -I$(srctree)/drivers/md
ccflags-y += -I$(src)/src
ccflags-y += -I$(src)/src/persistent-data
ccflags-y += -include $(src)/private-rename.h
ccflags-y += -DESPINIT_GENERATION=\"$(ESPINIT_GENERATION_VALUE)\"

else

.DEFAULT_GOAL := modules
PHONE_MODULE := thin
include ../../scripts/phone-module.mk

.PHONY: all modules clean
all modules: phone

clean:
	@test -n "$(KERNEL_SRC)" && test -n "$(KERNEL_OUT)"
	$(MAKE) -C "$(KERNEL_SRC)" O="$(KERNEL_OUT)" M="$(CURDIR)" clean

endif
