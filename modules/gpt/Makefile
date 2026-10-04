# SPDX-License-Identifier: GPL-2.0-only
ifneq ($(KERNELRELEASE),)
obj-m := gpt.o

# Keep generation capture/validation in sync with kernel/Kbuild.
MDIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
GIT := $(shell PATH="$$PATH":/usr/bin:/usr/local/bin which git)
ifneq ($(origin ESPINIT_GENERATION),undefined)
ESPINIT_GENERATION_VALUE := $(value ESPINIT_GENERATION)
ESPINIT_GENERATION_SOURCE := environment
else
ESPINIT_GENERATION_SOURCE := git HEAD
ifeq ($(GIT),)
$(error gpt: cannot derive the build generation: ESPINIT_GENERATION is unset and git was not found in PATH. Set ESPINIT_GENERATION explicitly when building from a source tarball.)
endif
ESPINIT_GENERATION_VALUE := $(shell cd '$(subst ','"'"',$(MDIR))' && '$(subst ','"'"',$(GIT))' rev-parse HEAD 2>/dev/null)
ESPINIT_GENERATION_GIT_VALID := $(shell generation='$(subst ','"'"',$(ESPINIT_GENERATION_VALUE))'; LC_ALL=C; export LC_ALL; invalid=`printf '%s' "$$generation" | tr -d '0-9a-f'`; if [ "$${#generation}" -eq 40 ] && [ -z "$$invalid" ]; then echo yes; else echo no; fi)
ifneq ($(ESPINIT_GENERATION_GIT_VALID),yes)
$(error gpt: cannot derive the build generation: ESPINIT_GENERATION is unset and git rev-parse HEAD failed or did not return 40 lowercase hex bytes. Set ESPINIT_GENERATION explicitly when building from a source tarball.)
endif
endif
ESPINIT_GENERATION_VALID := $(shell generation='$(subst ','"'"',$(ESPINIT_GENERATION_VALUE))'; LC_ALL=C; export LC_ALL; invalid=`printf '%s' "$$generation" | tr -d 'A-Za-z0-9._-'`; if [ -n "$$generation" ] && [ "$${#generation}" -le 63 ] && [ -z "$$invalid" ]; then echo yes; else echo no; fi)
ifneq ($(ESPINIT_GENERATION_VALID),yes)
$(error gpt: invalid ESPINIT_GENERATION: expected 1-63 ASCII letters/digits/._-; unset it to derive the full Git HEAD hash)
endif
$(info -- gpt generation: $(ESPINIT_GENERATION_VALUE) (from $(ESPINIT_GENERATION_SOURCE)))
ccflags-y += -DESPINIT_GENERATION=\"$(ESPINIT_GENERATION_VALUE)\"
else
KDIR ?= /lib/modules/$(shell uname -r)/build
JOBS ?= 13
JOBS_VALID := $(shell jobs='$(subst ','"'"',$(JOBS))'; invalid=`printf '%s' "$$jobs" | tr -d '0-9'`; if [ -n "$$jobs" ] && [ -z "$$invalid" ] && [ "$$jobs" -ge 1 ] && [ "$$jobs" -le 13 ]; then echo yes; else echo no; fi)
ifneq ($(JOBS_VALID),yes)
$(error gpt: invalid JOBS='$(JOBS)': expected an integer between 1 and 13)
endif
.PHONY: all clean
all:
	$(MAKE) -C "$(KDIR)" M="$(CURDIR)" LLVM=1 modules -j$(JOBS)
clean:
	$(MAKE) -C "$(KDIR)" M="$(CURDIR)" clean
endif
