#include <linux/errno.h>
#include <linux/init.h>
#include <linux/compiler.h>
#include "klog.h" // IWYU pragma: keep
#include "hook/syscall_hook_manager.h"
#include "hook/setuid_hook.h"
#include "hook/syscall_hook.h"
#include "hook/syscall_event_bridge.h"

static syscall_fn_t original_execve;
static syscall_fn_t original_execveat;
static syscall_fn_t original_setresuid;

static long __nocfi direct_execve(const struct pt_regs *regs)
{
    return ksu_hook_execve(READ_ONCE(original_execve), regs);
}

static long __nocfi direct_execveat(const struct pt_regs *regs)
{
    return ksu_hook_execveat(READ_ONCE(original_execveat), regs);
}

static long __nocfi direct_setresuid(const struct pt_regs *regs)
{
    return ksu_hook_setresuid(READ_ONCE(original_setresuid), regs);
}

int __init ksu_syscall_hook_manager_init(void)
{
    int ret;

    ksu_setuid_hook_init();
    ret = ksu_syscall_table_hook(__NR_setresuid, direct_setresuid, &original_setresuid);
    if (ret)
        goto fail;
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
    if (!ksu_syscall_hooks_published())
        ksu_setuid_hook_exit();
    /* Do not free kernel_umount state: a published wrapper may still run. */
    return ret;
}

void ksu_syscall_hook_manager_exit(void)
{
    ksu_syscall_hook_exit();
    /* Only reachable for a module that never published hooks. Published
     * modules are pinned: detached table slots do not prove chain quiescence.
     */
    if (!ksu_syscall_hooks_published())
        ksu_setuid_hook_exit();
}
