#include <linux/compiler.h>
#include <linux/jump_label.h>
#include <linux/sched.h>
#include <linux/objtool.h>
#include "runtime/init_integration.h"
#include "hook/syscall_hook.h"
#include "hook/syscall_event_bridge.h"

DEFINE_STATIC_KEY_TRUE(egysk_init_execve_key);

void egysk_stop_init_execve_hook(void)
{
    static_branch_disable(&egysk_init_execve_key);
}

static long __nocfi ksu_hook_execve_common(syscall_fn_t original, const struct pt_regs *regs, bool execveat)
{
    /* Only PID 1's policy-ready second-stage exec is observed. Never mutate
     * the syscall arguments, path or caller credentials; call the saved owner.
     */
    if (current->pid == 1 && static_branch_unlikely(&egysk_init_execve_key)) {
        if (execveat)
            egysk_init_execveat_hook(regs);
        else
            egysk_init_execve_hook(regs);
    }
    return original(regs);
}

long __nocfi ksu_hook_execve(syscall_fn_t original, const struct pt_regs *regs)
{
    return ksu_hook_execve_common(original, regs, false);
}

long __nocfi ksu_hook_execveat(syscall_fn_t original, const struct pt_regs *regs)
{
    return ksu_hook_execve_common(original, regs, true);
}

#ifdef ANNOTATE_NOCFI_SYM
/* The saved-owner call is intentionally nocfi, including when inlined here. */
ANNOTATE_NOCFI_SYM(ksu_hook_execve);
ANNOTATE_NOCFI_SYM(ksu_hook_execveat);
#endif
