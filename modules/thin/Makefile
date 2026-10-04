# SPDX-License-Identifier: GPL-2.0-only
#
# espinit thin.ko -- separate GPL-2.0-only device-mapper thin-provisioning
# module vendored from the Linux kernel dm-thin family (see PROVENANCE.md).
#
# The module is always built out of tree against an already configured kernel
# tree, so the kernel sources and their build output directory are provided by
# the caller:
#
#   make -C modules/thin KERNEL_SRC=/path/to/kernel KERNEL_OUT=/path/to/out
#
# modules/thin/build.sh wraps the same invocation for scripted use. JOBS
# defaults to 13 and is capped at 13; ARCH defaults to the phone's arm64.

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

KERNEL_SRC ?=
KERNEL_OUT ?=
ARCH ?= arm64
JOBS ?= 13

JOBS_VALID := $(shell jobs='$(subst ','"'"',$(JOBS))'; invalid=`printf '%s' "$$jobs" | tr -d '0-9'`; if [ -n "$$jobs" ] && [ -z "$$invalid" ] && [ "$$jobs" -ge 1 ] && [ "$$jobs" -le 13 ]; then echo yes; else echo no; fi)
ifneq ($(JOBS_VALID),yes)
$(error espinit thin: invalid JOBS='$(JOBS)': expected an integer between 1 and 13)
endif

# The vendored 6.12 sources predate warning options that newer clang releases
# promote to errors under CONFIG_WERROR; these suppressions are the ones the
# proven build used and never change generated code.
THIN_HOSTCFLAGS := -Wno-unknown-warning-option -Wno-error=incompatible-pointer-types-discards-qualifiers
THIN_KCFLAGS := -Wno-unknown-warning-option -Wno-error=default-const-init-var-unsafe -Wno-error=default-const-init-field-unsafe -Wno-error=unterminated-string-initialization -Wno-error=uninitialized

define thin_require_output_tree
	@test -n "$(KERNEL_SRC)" || { echo "espinit thin: set KERNEL_SRC to the configured kernel tree" >&2; exit 2; }
	@test -n "$(KERNEL_OUT)" || { echo "espinit thin: set KERNEL_OUT to the kernel build output directory" >&2; exit 2; }
endef

.PHONY: all modules clean

all: modules

modules:
	$(thin_require_output_tree)
	$(MAKE) -C "$(KERNEL_SRC)" O="$(KERNEL_OUT)" M="$(CURDIR)" \
		ARCH="$(ARCH)" LLVM=1 KBUILD_GENDWARFKSYMS_STABLE=1 \
		HOSTCFLAGS="$(THIN_HOSTCFLAGS)" KCFLAGS="$(THIN_KCFLAGS)" \
		modules -j$(JOBS)

clean:
	$(thin_require_output_tree)
	$(MAKE) -C "$(KERNEL_SRC)" O="$(KERNEL_OUT)" M="$(CURDIR)" clean

endif
