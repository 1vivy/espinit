# SPDX-License-Identifier: GPL-2.0-only
ifneq ($(KERNELRELEASE),)
obj-m := efivarfs.o
efivarfs-y := super.o inode.o file.o vars-patched.o
ccflags-y += -Werror
targets += vars-patched.c
clean-files += vars-patched.c
quiet_cmd_esu_vars = PATCH   $@
      cmd_esu_vars = patch --silent --output=$@ $< < $(src)/patches/0001-esu-project-guid.patch
$(obj)/vars-patched.c: $(src)/vars.c $(src)/patches/0001-esu-project-guid.patch FORCE
	$(call if_changed,esu_vars)
else
.DEFAULT_GOAL := all
PHONE_MODULE := efivarfs
include ../../scripts/phone-module.mk
.PHONY: all modules clean
all modules: phone
clean:
	@test -n "$(KMI_SRC)" && test -n "$(KMI_OUT)"
	$(MAKE) -C "$(KMI_SRC)" O="$(KMI_OUT)" M="$(CURDIR)" clean
endif
