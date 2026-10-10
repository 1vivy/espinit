#include <linux/errno.h>
#include <linux/init.h>
#include <linux/compiler.h>
#include "klog.h" // IWYU pragma: keep
#include "hook/syscall_hook_manager.h"
#include "hook/syscall_hook.h"
#include "hook/syscall_event_bridge.h"

static syscall_fn_t original_execve;
static syscall_fn_t original_execveat;

static long __nocfi direct_execve(const struct pt_regs *regs)
{
    return ksu_hook_execve(READ_ONCE(original_execve), regs);
}

static long __nocfi direct_execveat(const struct pt_regs *regs)
{
    return ksu_hook_execveat(READ_ONCE(original_execveat), regs);
}


int __init ksu_syscall_hook_manager_init(void)
{
    int ret;

    ret = ksu_syscall_table_hook(__NR_execve, direct_execve, &original_execve);
    if (ret)
        goto fail;
    ret = ksu_syscall_table_hook(__NR_execveat, direct_execveat, &original_execveat);
    if (ret)
        goto fail;
    return 0;
fail:
    pr_err("hook_manager: required direct hook installation failed: %d\n", ret);
    ksu_syscall_hook_exit();
    return ret;
}

void ksu_syscall_hook_manager_exit(void)
{
    ksu_syscall_hook_exit();
}
