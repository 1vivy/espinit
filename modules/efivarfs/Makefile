# SPDX-License-Identifier: GPL-2.0-only
ifneq ($(KERNELRELEASE),)
obj-m := efivarfs.o
efivarfs-y := super-wrapper.o inode.o file.o vars.o bdsvars.o
ccflags-y += -include $(src)/esu-guid.h -Werror
else
.DEFAULT_GOAL := all
PHONE_MODULE := efivarfs
include ../../scripts/phone-module.mk
.PHONY: all modules clean host-test
all modules: phone
clean:
	@test -n "$(KMI_SRC)" && test -n "$(KMI_OUT)"
	$(MAKE) -C "$(KMI_SRC)" O="$(KMI_OUT)" M="$(CURDIR)" clean
host-test:
	python3 test/test_backend.py
endif
