# SPDX-License-Identifier: GPL-2.0-only
#
# esu thin.ko -- separate GPL-2.0-only device-mapper thin-provisioning
# module vendored from the Linux kernel dm-thin family (see PROVENANCE.md).
#
# The module is always built out of tree against an already configured kernel
# tree, so the kernel sources and their build output directory are provided by
# the caller:
#
#   make -C modules/thin KMI_SRC=<source> KMI_OUT=<output>
#
# modules/thin/build.sh wraps the same invocation for scripted use. JOBS
# defaults to 13 and is capped at 13; the KMI architecture is arm64.

# Build generation identity
#
# The generation string identifies one complete, coordinated ESP payload: the
# PID-1 stage, the esu core module, the esud daemon and every ESP module
# report the exact same value. It is taken from ESU_GENERATION when set,
# otherwise from the full 40-byte lowercase Git HEAD hash of this repository,
# and it must be a nonempty ASCII token so it can be embedded into a C string
# literal unescaped.
#
# Keep this logic in sync with kernel/Kbuild, userspace/esud/build.rs and
# userspace/esuinit/build.rs.
#
# Capture the raw value once: environment/command-line values must not be
# expanded as Make expressions. Single-quote escaping below also keeps them
# data, not shell source, while validating.
MDIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
GIT := $(shell PATH="$$PATH":/usr/bin:/usr/local/bin which git)

ifneq ($(origin ESU_GENERATION),undefined)
ESU_GENERATION_VALUE := $(value ESU_GENERATION)
ESU_GENERATION_SOURCE := environment
else
ESU_GENERATION_SOURCE := git HEAD
ifeq ($(GIT),)
$(error esu thin: cannot derive the build generation: ESU_GENERATION is unset and git was not found in PATH. Set ESU_GENERATION explicitly when building from a source tarball.)
endif
ESU_GENERATION_VALUE := $(shell cd '$(subst ','"'"',$(MDIR))' && '$(subst ','"'"',$(GIT))' rev-parse HEAD 2>/dev/null)
ESU_GENERATION_GIT_VALID := $(shell generation='$(subst ','"'"',$(ESU_GENERATION_VALUE))'; LC_ALL=C; export LC_ALL; invalid=`printf '%s' "$$generation" | tr -d '0-9a-f'`; if [ "$${#generation}" -eq 40 ] && [ -z "$$invalid" ]; then echo yes; else echo no; fi)
ifneq ($(ESU_GENERATION_GIT_VALID),yes)
$(error esu thin: cannot derive the build generation: ESU_GENERATION is unset and git rev-parse HEAD failed or did not return 40 lowercase hex bytes. Set ESU_GENERATION explicitly when building from a source tarball.)
endif
endif

# Do not rely on export reaching $(shell ...): Make does not export newly
# assigned variables to that expansion on all supported versions. Validate a
# safely quoted literal, and reject every invalid byte (including newlines).
ESU_GENERATION_VALID := $(shell generation='$(subst ','"'"',$(ESU_GENERATION_VALUE))'; LC_ALL=C; export LC_ALL; invalid=`printf '%s' "$$generation" | tr -d 'A-Za-z0-9._-'`; if [ -n "$$generation" ] && [ "$${#generation}" -le 63 ] && [ -z "$$invalid" ]; then echo yes; else echo no; fi)
ifneq ($(ESU_GENERATION_VALID),yes)
$(error esu thin: invalid ESU_GENERATION: expected 1-63 ASCII letters/digits/._-; unset it to derive the full Git HEAD hash)
endif

$(info -- esu thin generation: $(ESU_GENERATION_VALUE) (from $(ESU_GENERATION_SOURCE)))

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
ccflags-y += -DESU_GENERATION=\"$(ESU_GENERATION_VALUE)\"

else

.DEFAULT_GOAL := modules
PHONE_MODULE := thin
include ../../scripts/phone-module.mk

.PHONY: all modules clean
all modules: phone

clean:
	@test -n "$(KMI_SRC)" && test -n "$(KMI_OUT)"
	$(MAKE) -C "$(KMI_SRC)" O="$(KMI_OUT)" M="$(CURDIR)" clean

endif
