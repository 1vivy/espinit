# SPDX-License-Identifier: GPL-2.0-only
ifneq ($(KERNELRELEASE),)
obj-m := gpt.o

else
.DEFAULT_GOAL := all
PHONE_MODULE := gpt
include ../../scripts/phone-module.mk
.PHONY: all modules clean
all modules: phone
clean:
	@test -n "$(KMI_SRC)" && test -n "$(KMI_OUT)"
	$(MAKE) -C "$(KMI_SRC)" O="$(KMI_OUT)" M="$(CURDIR)" clean
endif
