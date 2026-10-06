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
