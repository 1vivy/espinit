// SPDX-License-Identifier: GPL-3.0-only
#include <linux/atomic.h>
#include <linux/kprobes.h>
#include <linux/ptrace.h>
#include <linux/sched.h>
#include <linux/workqueue.h>

#include "klog.h" // IWYU pragma: keep
#include "selinux/selinux.h"
#include "hook/selinux_policy_hook.h"

static void apply_rules_work(struct work_struct *work)
{
    apply_espinit_rules();
}

static DECLARE_WORK(rules_work, apply_rules_work);
static atomic_t policy_loaded = ATOMIC_INIT(0);

static int policy_load_return(struct kretprobe_instance *instance,
                              struct pt_regs *regs)
{
    if (current->pid == 1 && regs_return_value(regs) > 0)
        atomic_set(&policy_loaded, 1);
    return 0;
}

static int exec_return(struct kretprobe_instance *instance, struct pt_regs *regs)
{
    if (current->pid == 1 && regs_return_value(regs) == 0 &&
        atomic_cmpxchg(&policy_loaded, 1, 0) == 1)
        schedule_work(&rules_work);
    return 0;
}

static struct kretprobe policy_load_probe = {
    .kp.symbol_name = "sel_write_load",
    .handler = policy_load_return,
    .maxactive = 1,
};
static struct kretprobe exec_probe = {
    .kp.symbol_name = "do_execveat_common",
    .handler = exec_return,
};

int ksu_selinux_policy_hook_init(void)
{
    int error = register_kretprobe(&policy_load_probe);

    if (error) {
        pr_err("cannot register SELinux policy-load hook: %d\n", error);
        return error;
    }

    error = register_kretprobe(&exec_probe);
    if (error) {
        pr_err("cannot register post-exec policy hook: %d\n", error);
        unregister_kretprobe(&policy_load_probe);
        return error;
    }

    pr_info("registered SELinux policy-load and post-exec hooks\n");
    return 0;
}

void ksu_selinux_policy_hook_exit(void)
{
    unregister_kretprobe(&exec_probe);
    unregister_kretprobe(&policy_load_probe);
    cancel_work_sync(&rules_work);
}
