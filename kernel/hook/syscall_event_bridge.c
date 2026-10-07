#include "linux/compiler.h"
#include "linux/cred.h"
#include "linux/jump_label.h"
#include "linux/printk.h"
#include "selinux/selinux.h"
#include <asm/syscall.h>
#include <linux/ptrace.h>
#include <linux/static_key.h>

#include "arch.h"
#include "klog.h" // IWYU pragma: keep
#include "hook/setuid_hook.h"
#include "runtime/esud.h"
#include "hook/syscall_hook.h"
#include "hook/syscall_event_bridge.h"

static int ksu_handle_init_esud(const char __user **filename_user)
{
    char path[64];
    unsigned long addr;
    const char __user *fn;
    long ret;

    if (unlikely(!filename_user))
        return 0;

    addr = untagged_addr((unsigned long)*filename_user);
    fn = (const char __user *)addr;
    ret = strncpy_from_user(path, fn, sizeof(path));
    if (ret < 0)
        return 0;

    path[sizeof(path) - 1] = '\0';
    if (unlikely(strcmp(path, KSUD_PATH) == 0)) {
        pr_info("hook_manager: escape to root for init executing esud: %d\n", current->pid);
        escape_to_root_for_init();
    }

    return 0;
}

DEFINE_STATIC_KEY_TRUE(esud_execve_key);

void ksu_stop_esud_execve_hook()
{
    static_branch_disable(&esud_execve_key);
}

static long __nocfi ksu_hook_execve_common(syscall_fn_t original, const struct pt_regs *regs, bool execveat)
{
    const char __user **filename_user =
        execveat ? (const char __user **)&PT_REGS_PARM2(regs) : (const char __user **)&PT_REGS_SYSCALL_PARM1(regs);
    bool current_is_init = is_init(current_cred());

    if (static_branch_unlikely(&esud_execve_key)) {
        if (execveat) {
            ksu_execveat_hook_esud(regs);
        } else {
            ksu_execve_hook_esud(regs);
        }
    }

    if (current->pid != 1 && current_is_init)
        ksu_handle_init_esud(filename_user);

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

long __nocfi ksu_hook_setresuid(syscall_fn_t original, const struct pt_regs *regs)
{
    uid_t old_uid = current_uid().val;
    long ret = original(regs);

    if (ret < 0)
        return ret;

    ksu_handle_setresuid(old_uid, current_uid().val);
    return ret;
}
