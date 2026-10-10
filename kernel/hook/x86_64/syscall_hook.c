#ifdef __x86_64__
#include <linux/init.h>
#include <linux/errno.h>
#include <linux/string.h>
#include "../syscall_hook.h"
#include <linux/nospec.h>
#include <linux/version.h>
#include <linux/objtool.h>
#include "infra/symbol_resolver.h"
#include "../patch_memory.h"
#include "arch.h"
#include "klog.h"

/* Hardened x86 kernels bypass sys_call_table. Keep the existing optional
 * indirect-dispatch adaptation, independently of the removed ni dispatcher.
 */
#ifdef CONFIG_KERNELESP_X86_PATCH_SYSCALL_DISPATCHER
static void *syscall_patch_addr;
static char syscall_patch_original[14];
static char syscall_patch_installed[14];

#if LINUX_VERSION_CODE >= KERNEL_VERSION(5, 16, 0)
static long __nocfi my_x64_sys_call(const struct pt_regs *regs, unsigned int nr)
{
    if (unlikely(nr >= NR_syscalls))
        return -ENOSYS;
    nr = array_index_nospec(nr, NR_syscalls);
    return ksu_syscall_table[nr](regs);
}
#ifdef ANNOTATE_NOCFI_SYM
/* Dispatch the saved syscall table through the existing nocfi adapter. */
ANNOTATE_NOCFI_SYM(my_x64_sys_call);
#endif
#else
static long (*syscall_enter_from_user_mode_fn)(struct pt_regs *regs, long syscall);
static void (*syscall_exit_to_user_mode_fn)(struct pt_regs *regs);

static void __nocfi my_do_syscall_64(struct pt_regs *regs, int nr)
{
    unsigned int unr;

    nr = syscall_enter_from_user_mode_fn(regs, nr);
    nr = syscall_get_nr(current, regs);
    unr = nr;
    /* Existing AVD adaptation: these pre-5.16 targets do not support x32. */
    if (likely(unr < NR_syscalls)) {
        unr = array_index_nospec(unr, NR_syscalls);
        regs->ax = ksu_syscall_table[unr](regs);
    } else if (nr != -1) {
        regs->ax = -ENOSYS;
    }
    syscall_exit_to_user_mode_fn(regs);
}
#endif

static int patch_abs_jump(const char *sym, void *target)
{
    static const char endbr64[] = { 0xf3, 0x0f, 0x1e, 0xfa };
    char jump[14] = { 0xff, 0x25, 0, 0, 0, 0 };
    void *addr = (void *)find_kernel_symbol_exact(sym);
    int ret;

    if (!addr)
        return -ENOENT;
    if (!memcmp(addr, endbr64, sizeof(endbr64)))
        addr = (char *)addr + sizeof(endbr64);
    memcpy(jump + 6, &target, sizeof(target));
    memcpy(syscall_patch_original, addr, sizeof(jump));
    memcpy(syscall_patch_installed, jump, sizeof(jump));
    ksu_syscall_hook_pin();
    ret = ksu_patch_text_checked(addr, syscall_patch_original, jump, sizeof(jump), KSU_PATCH_TEXT_FLUSH_ICACHE);
    if (!ret || memcmp(addr, syscall_patch_original, sizeof(jump)))
        syscall_patch_addr = addr;
    if (ret)
        pr_err("patch %s failed: %d\n", sym, ret);
    return ret;
}
#endif

int __init __nocfi ksu_syscall_hook_init(void)
{
    ksu_syscall_table = (syscall_fn_t *)ksu_resolve_symbol_for_functable_hook("sys_call_table");
    pr_info("sys_call_table=0x%lx\n", (unsigned long)ksu_syscall_table);
    if (!ksu_syscall_table)
        return -ENOENT;
#ifdef CONFIG_KERNELESP_X86_PATCH_SYSCALL_DISPATCHER
#if LINUX_VERSION_CODE < KERNEL_VERSION(5, 16, 0)
    syscall_enter_from_user_mode_fn = (void *)find_kernel_symbol_exact("syscall_enter_from_user_mode");
    syscall_exit_to_user_mode_fn = (void *)find_kernel_symbol_exact("syscall_exit_to_user_mode");
    if (!syscall_enter_from_user_mode_fn || !syscall_exit_to_user_mode_fn)
        return -ENOENT;
    return patch_abs_jump("do_syscall_64", my_do_syscall_64);
#else
    return patch_abs_jump("x64_sys_call", my_x64_sys_call);
#endif
#else
    return 0;
#endif
}

void ksu_arch_syscall_hook_exit(void)
{
#ifdef CONFIG_KERNELESP_X86_PATCH_SYSCALL_DISPATCHER
    int ret;

    if (!syscall_patch_addr)
        return;
    ret = ksu_patch_text_checked(syscall_patch_addr, syscall_patch_installed, syscall_patch_original,
                                 sizeof(syscall_patch_original), KSU_PATCH_TEXT_FLUSH_ICACHE);
    if (ret)
        pr_err("restore x86 syscall dispatch refused/failed: %d; retaining callback state\n", ret);
    else
        syscall_patch_addr = NULL;
#endif
}
#endif
