// SPDX-License-Identifier: GPL-2.0-only
/* Keep super.c verbatim; the combined module owns the init/exit entry points. */
#include <linux/module.h>
int esu_fs_init(void);
void esu_fs_exit(void);
#undef module_init
#undef module_exit
#define module_init(fn) int esu_fs_init(void) { return fn(); }
#define module_exit(fn) void esu_fs_exit(void) { fn(); }
#include "super.c"
