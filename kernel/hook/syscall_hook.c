#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/errno.h>
#include <linux/kernel.h>
#include "hook/syscall_hook.h"
#include "hook/patch_memory.h"
#include "klog.h" // IWYU pragma: keep

#ifndef __NR_syscalls
#define __NR_syscalls (__NR_syscall_max + 1)
#endif

syscall_fn_t *ksu_syscall_table;
static DEFINE_MUTEX(hooked_entries_lock);
static struct {
    int nr;
    syscall_fn_t original;
    syscall_fn_t replacement;
} hooked_entries[8];
static unsigned int hooked_count;
static bool published;

/* A foreign hook can save our address, even after we restore our table entry.
 * No table scan or RCU grace period proves that such a chain has disappeared.
 * Keep this module and all callback state resident once publication is attempted.
 * Forced module unloading is unsupported, like forced unloading any pinned LKM.
 */
void ksu_syscall_hook_pin(void)
{
    if (published)
        return;
#ifdef MODULE
    __module_get(THIS_MODULE);
#endif
    published = true;
}

bool ksu_syscall_hooks_published(void)
{
    return published;
}

int ksu_syscall_table_hook(int nr, syscall_fn_t fn, syscall_fn_t *old)
{
    syscall_fn_t original;
    unsigned int i;
    int ret;

    if (!ksu_syscall_table)
        return -ENOENT;
    if (nr < 0 || nr >= __NR_syscalls || !fn || !old)
        return -EINVAL;
    mutex_lock(&hooked_entries_lock);
    for (i = 0; i < hooked_count; i++) {
        if (hooked_entries[i].nr == nr) {
            ret = -EEXIST;
            goto out;
        }
    }
    if (hooked_count == ARRAY_SIZE(hooked_entries)) {
        ret = -ENOSPC;
        goto out;
    }
    original = READ_ONCE(ksu_syscall_table[nr]);
    if (!original || original == fn) {
        ret = -EINVAL;
        goto out;
    }
    /* Publish the saved target before the wrapper can run. Never dispatch back
     * through the current table, which contains our own wrapper (or a chain).
     */
    WRITE_ONCE(*old, original);
    ksu_syscall_hook_pin();
    ret = ksu_patch_text_checked(&ksu_syscall_table[nr], &original, &fn, sizeof(fn), KSU_PATCH_TEXT_FLUSH_DCACHE);
    if (!ret || READ_ONCE(ksu_syscall_table[nr]) != original) {
        /* Also retain state after a possibly partial failed write. */
        hooked_entries[hooked_count].nr = nr;
        hooked_entries[hooked_count].original = original;
        hooked_entries[hooked_count++].replacement = fn;
    }
    if (ret)
        pr_err("hook syscall %d failed: %d\n", nr, ret);
out:
    mutex_unlock(&hooked_entries_lock);
    return ret;
}

int ksu_syscall_table_unhook(int nr)
{
    unsigned int i;
    int ret = -ENOENT;

    mutex_lock(&hooked_entries_lock);
    for (i = 0; i < hooked_count; i++) {
        if (hooked_entries[i].nr != nr)
            continue;
        ret = ksu_patch_text_checked(&ksu_syscall_table[nr], &hooked_entries[i].replacement,
                                     &hooked_entries[i].original, sizeof(syscall_fn_t), KSU_PATCH_TEXT_FLUSH_DCACHE);
        if (!ret) {
            hooked_entries[i] = hooked_entries[--hooked_count];
            pr_info("unhooked syscall %d\n", nr);
        } else {
            /* Another owner may still call us through its saved original. */
            pr_err("unhook syscall %d refused/failed: %d; retaining callback state\n", nr, ret);
        }
        break;
    }
    mutex_unlock(&hooked_entries_lock);
    return ret;
}

void ksu_syscall_hook_exit(void)
{
    int nrs[ARRAY_SIZE(hooked_entries)];
    unsigned int i, count;

    mutex_lock(&hooked_entries_lock);
    count = hooked_count;
    for (i = 0; i < count; i++)
        nrs[i] = hooked_entries[i].nr;
    mutex_unlock(&hooked_entries_lock);
    for (i = 0; i < count; i++)
        ksu_syscall_table_unhook(nrs[i]);
    ksu_arch_syscall_hook_exit();
    /* Never release the publication pin or saved targets here: self-unhook
     * can execute inside a callback, and foreign hook chains outlive detachment.
     */
}
