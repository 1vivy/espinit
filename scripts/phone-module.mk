# Shared arm64 KMI contract. Included only by outer module Makefiles.
KMI_SRC ?=
KMI_OUT ?=
JOBS ?= 13
ifneq ($(origin ESU_GENERATION),undefined)
export ESU_GENERATION
endif
KMI_VERIFIER := $(abspath $(dir $(lastword $(MAKEFILE_LIST)))/kmi_modules.py)

.PHONY: phone
phone:
	python3 "$(KMI_VERIFIER)" build --module "$(PHONE_MODULE)" --kmi-src "$(KMI_SRC)" --kmi-out "$(KMI_OUT)" --jobs "$(JOBS)"
