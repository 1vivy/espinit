# SPDX-License-Identifier: GPL-2.0-only
# Explicit ACK identity; source layout is unrelated to release module directories.
KMI_SRC ?=
KMI_OUT ?=
KMI_BRANCH ?=
KMI_GENERATION ?=
KMI_ARCH ?=
JOBS ?= 13
KMI_VERIFIER := $(abspath $(dir $(lastword $(MAKEFILE_LIST)))/kmi_modules.py)
.PHONY: all modules phone clean
all modules: phone
phone:
	python3 "$(KMI_VERIFIER)" build --module-dir "$(CURDIR)" --module "$(PHONE_MODULE)" --branch "$(KMI_BRANCH)" --generation "$(KMI_GENERATION)" --arch "$(KMI_ARCH)" --kmi-src "$(KMI_SRC)" --kmi-out "$(KMI_OUT)" --jobs "$(JOBS)"
clean:
	@test -n "$(KMI_SRC)" && test -n "$(KMI_OUT)"
	$(MAKE) -C "$(KMI_SRC)" O="$(KMI_OUT)" M="$(CURDIR)" clean
