/* SPDX-License-Identifier: GPL-2.0-only */
#if defined(__riscv)
#include <linux/init.h>
#include <linux/errno.h>
#include "../syscall_hook.h"
#include "infra/symbol_resolver.h"
#include "klog.h"

int __init ksu_syscall_hook_init(void)
{
    ksu_syscall_table = (syscall_fn_t *)ksu_resolve_symbol_for_functable_hook("sys_call_table");
    pr_info("sys_call_table=0x%lx\n", (unsigned long)ksu_syscall_table);
    return ksu_syscall_table ? 0 : -ENOENT;
}

void ksu_arch_syscall_hook_exit(void)
{
}
#endif
