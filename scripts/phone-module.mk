# Shared exact-phone contract. Included only by outer module Makefiles.
KERNEL_SRC ?=
KERNEL_OUT ?=
KERNEL_CONFIG ?=
JOBS ?= 13
ifneq ($(origin ESPINIT_GENERATION),undefined)
export ESPINIT_GENERATION
endif
PHONE_VERIFIER := $(abspath $(dir $(lastword $(MAKEFILE_LIST)))/phone_modules.py)

.PHONY: phone phone-config
phone-config:
	python3 "$(PHONE_VERIFIER)" config --kernel-src "$(KERNEL_SRC)" --kernel-out "$(KERNEL_OUT)" --kernel-config "$(KERNEL_CONFIG)"
phone:
	python3 "$(PHONE_VERIFIER)" build --module "$(PHONE_MODULE)" --kernel-src "$(KERNEL_SRC)" --kernel-out "$(KERNEL_OUT)" --kernel-config "$(KERNEL_CONFIG)" --jobs "$(JOBS)"
