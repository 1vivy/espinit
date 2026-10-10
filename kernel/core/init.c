#include <linux/export.h>
#include <linux/version.h>
#include <linux/fs.h>
#include <linux/kobject.h>
#include <linux/module.h>
#include <linux/rcupdate.h>
#include <linux/sched.h>
#include <linux/workqueue.h>
#include <linux/moduleparam.h>

#include "klog.h" // IWYU pragma: keep
#include "hook/syscall_hook_manager.h"
#include "runtime/init_integration.h"
#include "selinux/selinux.h"
#include "hook/syscall_hook.h"
#include "infra/symbol_resolver.h"
#include "supercall/supercall.h"

#if defined(__x86_64__) && !defined(CONFIG_EGYSK_X86_PATCH_SYSCALL_DISPATCHER)
#include <asm/cpufeature.h>
#include <linux/version.h>
#ifndef X86_FEATURE_INDIRECT_SAFE
#error "FATAL: Your kernel is missing the indirect syscall bypass patches!"
#endif
#endif

// workaround for A12-5.10 kernel
// Some third-party kernel (e.g. linegaeOS) uses wrong toolchain, which supports
// CC_HAVE_STACKPROTECTOR_SYSREG while gki's toolchain doesn't.
// Therefore, egysk lkm, which uses gki toolchain, requires this __stack_chk_guard,
// while those third-party kernel can't provide.
// Thus, we manually provide it instead of using kernel's:
#if defined(CONFIG_STACKPROTECTOR) &&                                                                                  \
    (defined(CONFIG_ARM64) && defined(MODULE) && !defined(CONFIG_STACKPROTECTOR_PER_TASK))
#include <linux/stackprotector.h>
#include <linux/random.h>
unsigned long __stack_chk_guard __ro_after_init __attribute__((visibility("hidden")));

__attribute__((no_stack_protector)) void __init egysk_setup_stack_chk_guard()
{
    unsigned long canary;

    /* Try to get a semi random initial value. */
    get_random_bytes(&canary, sizeof(canary));
    canary ^= LINUX_VERSION_CODE;
    canary &= CANARY_MASK;
    __stack_chk_guard = canary;
}

__attribute__((naked)) int __init egysk_init_early(void)
{
    asm("mov x19, x30;\n"
        "bl egysk_setup_stack_chk_guard;\n"
        "mov x30, x19;\n"
        "b egysk_init;\n");
}
#define NEED_OWN_STACKPROTECTOR 1
#else
#define NEED_OWN_STACKPROTECTOR 0
#endif


bool ksu_no_custom_rc = false;
module_param_named(norc, ksu_no_custom_rc, bool, 0);

static int hook_init_error;
static bool hooks_ready;
module_param_named(hook_init_error, hook_init_error, int, 0444);

bool egysk_hooks_ready(void)
{
    return READ_ONCE(hooks_ready);
}

int __init egysk_init(void)
{
    int ret;
#if defined(__x86_64__) && !defined(CONFIG_EGYSK_X86_PATCH_SYSCALL_DISPATCHER)
    // If the kernel has the hardening patch, X86_FEATURE_INDIRECT_SAFE must be set
    if (!boot_cpu_has(X86_FEATURE_INDIRECT_SAFE)) {
        pr_alert("*************************************************************");
        pr_alert("**     NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE    **");
        pr_alert("**                                                         **");
        pr_alert("**        X86_FEATURE_INDIRECT_SAFE is not enabled!        **");
        pr_alert("**      egysk will abort initialization to prevent         **");
        pr_alert("**                     kernel panic.                       **");
        pr_alert("**                                                         **");
        pr_alert("**     NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE    **");
        pr_alert("*************************************************************");
        return -ENOSYS;
    }
#endif

#ifdef CONFIG_EGYSK_DEBUG
    pr_alert("*************************************************************");
    pr_alert("**     NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE    **");
    pr_alert("**                                                         **");
    pr_alert("**         You are running egysk in DEBUG mode             **");
    pr_alert("**                                                         **");
    pr_alert("**     NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE    **");
    pr_alert("*************************************************************");
#endif


    ksu_init_symbol_resolver();
    /* Supporting state must survive a failed installation if any callback
     * has been exposed to another owner's saved-original chain.
     */


    ret = ksu_supercalls_init();
    if (ret)
        return ret;

    ret = ksu_syscall_hook_init();
    if (ret)
        goto hook_failure;
    ret = ksu_syscall_hook_manager_init();
    if (ret)
        goto hook_failure;
    ret = egysk_init_integration_init();
    if (ret)
        goto hook_failure;

    WRITE_ONCE(hooks_ready, true);

    return 0;

hook_failure:
    hook_init_error = ret;
    ksu_syscall_hook_exit();
    pr_err("egysk: required hooks failed: %d; core NOT ready\n", ret);
    if (ksu_syscall_hooks_published()) {
        /* Returning an init error would free code still reachable through
         * an in-flight callback or a foreign saved-original pointer.
         * GET_INFO remains available but EGYSK_STATE_READY stays clear.
         */
        pr_err("egysk: retaining exposed module and callback state for boot lifetime\n");
        return 0;
    }
    ksu_supercalls_exit();
    return ret;
}

void __exit egysk_exit(void)
{
    if (ksu_syscall_hooks_published()) {
        pr_err("egysk: forced unload of published hooks is unsupported\n");
        return;
    }
    WRITE_ONCE(hooks_ready, false);
    // Phase 1: Stop all hooks first to prevent new callbacks
    ksu_syscall_hook_manager_exit();

    ksu_supercalls_exit();

    egysk_init_integration_exit();

    // Wait for any in-flight RCU readers
    synchronize_rcu();

}

#if NEED_OWN_STACKPROTECTOR
module_init(egysk_init_early);
#else
module_init(egysk_init);
#endif
module_exit(egysk_exit);

MODULE_LICENSE("GPL");
MODULE_AUTHOR("weishu");
MODULE_DESCRIPTION("Egysk init RC and policy helper");
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 13, 0)
MODULE_IMPORT_NS("VFS_internal_I_am_really_a_filesystem_and_am_NOT_a_driver");
#else
MODULE_IMPORT_NS(VFS_internal_I_am_really_a_filesystem_and_am_NOT_a_driver);
#endif
