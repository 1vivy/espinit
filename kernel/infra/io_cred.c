// SPDX-License-Identifier: GPL-2.0-only
//
// Optional credential provider for block-backed variable stores.
//
// esuinit's relocating loader binds a module's undefined symbols from vmlinux
// only (it stops at the first module line of /proc/kallsyms), so a module
// cannot link against kernelesp. Consumers instead resolve these two symbols
// at run time with symbol_get(), which also pins this module while they are in
// use. A consumer that does not find them keeps the caller's own credentials.
//
// The pair brackets file I/O that must run in the esu domain regardless of
// which process triggered it: a backing file opened by PID 1 (kernel sid) is
// otherwise denied `fd use` for domains such as hal_bootctl_default.

#include <linux/cred.h>
#include <linux/export.h>

#include "ksu.h"

const struct cred *efivar_store_io_enter(void)
{
    return override_creds(ksu_cred);
}
EXPORT_SYMBOL_GPL(efivar_store_io_enter);

void efivar_store_io_leave(const struct cred *old)
{
    revert_creds(old);
}
EXPORT_SYMBOL_GPL(efivar_store_io_leave);
