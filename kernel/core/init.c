#include <linux/cred.h>
#include <linux/export.h>
#include <linux/fs.h>
#include <linux/kobject.h>
#include <linux/module.h>
#include <linux/rcupdate.h>
#include <linux/sched.h>
#include <linux/workqueue.h>
#include <linux/moduleparam.h>

#include "policy/feature.h"
#include "klog.h" // IWYU pragma: keep
#include "hook/syscall_hook_manager.h"
#include "hook/lsm_hook.h"
#include "runtime/esud.h"
#include "runtime/esud_boot.h"
#include "selinux/selinux.h"
#include "hook/syscall_hook.h"
#include "feature/selinux_hide.h"
#include "infra/file_wrapper.h"
#include "infra/symbol_resolver.h"
#include "supercall/supercall.h"

#if defined(__x86_64__) && !defined(CONFIG_KERNELESP_X86_PATCH_SYSCALL_DISPATCHER)
#include <asm/cpufeature.h>
#include <linux/version.h>
#ifndef X86_FEATURE_INDIRECT_SAFE
#error "FATAL: Your kernel is missing the indirect syscall bypass patches!"
#endif
#endif

// workaround for A12-5.10 kernel
// Some third-party kernel (e.g. linegaeOS) uses wrong toolchain, which supports
// CC_HAVE_STACKPROTECTOR_SYSREG while gki's toolchain doesn't.
// Therefore, esu lkm, which uses gki toolchain, requires this __stack_chk_guard,
// while those third-party kernel can't provide.
// Thus, we manually provide it instead of using kernel's:
#if defined(CONFIG_STACKPROTECTOR) &&                                                                                  \
    (defined(CONFIG_ARM64) && defined(MODULE) && !defined(CONFIG_STACKPROTECTOR_PER_TASK))
#include <linux/stackprotector.h>
#include <linux/random.h>
unsigned long __stack_chk_guard __ro_after_init __attribute__((visibility("hidden")));

__attribute__((no_stack_protector)) void __init esu_setup_stack_chk_guard()
{
    unsigned long canary;

    /* Try to get a semi random initial value. */
    get_random_bytes(&canary, sizeof(canary));
    canary ^= LINUX_VERSION_CODE;
    canary &= CANARY_MASK;
    __stack_chk_guard = canary;
}

__attribute__((naked)) int __init esu_init_early(void)
{
    asm("mov x19, x30;\n"
        "bl esu_setup_stack_chk_guard;\n"
        "mov x30, x19;\n"
        "b esu_init;\n");
}
#define NEED_OWN_STACKPROTECTOR 1
#else
#define NEED_OWN_STACKPROTECTOR 0
#endif

struct cred *ksu_cred;

bool ksu_no_custom_rc = false;
module_param_named(norc, ksu_no_custom_rc, bool, 0);

int __init esu_init(void)
{
#if defined(__x86_64__) && !defined(CONFIG_KERNELESP_X86_PATCH_SYSCALL_DISPATCHER)
    // If the kernel has the hardening patch, X86_FEATURE_INDIRECT_SAFE must be set
    if (!boot_cpu_has(X86_FEATURE_INDIRECT_SAFE)) {
        pr_alert("*************************************************************");
        pr_alert("**     NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE    **");
        pr_alert("**                                                         **");
        pr_alert("**        X86_FEATURE_INDIRECT_SAFE is not enabled!        **");
        pr_alert("**       esu will abort initialization to prevent      **");
        pr_alert("**                     kernel panic.                       **");
        pr_alert("**                                                         **");
        pr_alert("**     NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE    **");
        pr_alert("*************************************************************");
        return -ENOSYS;
    }
#endif

#ifdef CONFIG_KERNELESP_DEBUG
    pr_alert("*************************************************************");
    pr_alert("**     NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE    **");
    pr_alert("**                                                         **");
    pr_alert("**          You are running esu in DEBUG mode          **");
    pr_alert("**                                                         **");
    pr_alert("**     NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE NOTICE    **");
    pr_alert("*************************************************************");
#endif

    ksu_cred = prepare_creds();
    if (!ksu_cred) {
        pr_err("prepare cred failed!\n");
        return -ENOSYS;
    }

    ksu_init_symbol_resolver();
    ksu_syscall_hook_init();

    ksu_feature_init();
    ksu_lsm_hook_init();
    ksu_selinux_hide_init();

    ksu_supercalls_init();

    ksu_syscall_hook_manager_init();

    ksu_esud_init();

    ksu_file_wrapper_init();

#ifdef MODULE
#ifndef CONFIG_KERNELESP_DEBUG
    kobject_del(&THIS_MODULE->mkobj.kobj);
#endif
#endif
    return 0;
}

void __exit esu_exit(void)
{
    // Phase 1: Stop all hooks first to prevent new callbacks
    ksu_syscall_hook_manager_exit();

    ksu_supercalls_exit();

    ksu_esud_exit();

    // Wait for any in-flight RCU readers
    synchronize_rcu();

    // Phase 2: Now safe to release data structures
    ksu_selinux_hide_exit();
    ksu_lsm_hook_exit();
    ksu_feature_exit();

    put_cred(ksu_cred);
}

#if NEED_OWN_STACKPROTECTOR
module_init(esu_init_early);
#else
module_init(esu_init);
#endif
module_exit(esu_exit);

MODULE_LICENSE("GPL");
MODULE_AUTHOR("weishu");
MODULE_DESCRIPTION("kernelesp early-boot substrate");
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 13, 0)
MODULE_IMPORT_NS("VFS_internal_I_am_really_a_filesystem_and_am_NOT_a_driver");
#else
MODULE_IMPORT_NS(VFS_internal_I_am_really_a_filesystem_and_am_NOT_a_driver);
#endif
