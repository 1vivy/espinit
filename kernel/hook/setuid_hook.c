#include <linux/compiler.h>
#include <linux/cred.h>
#include <linux/version.h>
#include <linux/printk.h>
#include <linux/sched.h>
#include <linux/types.h>
#include <linux/uidgid.h>

#include "hook/setuid_hook.h"
#include "klog.h" // IWYU pragma: keep
#include "feature/kernel_umount.h"

int ksu_handle_setresuid(uid_t old_uid, uid_t new_uid)
{
    // we rely on the fact that zygote always call setresuid(3) with same uids

    pr_info("handle_setresuid from %d to %d\n", old_uid, new_uid);

    // Handle kernel umount
    ksu_handle_umount(old_uid, new_uid);

    return 0;
}

void __init ksu_setuid_hook_init(void)
{
    ksu_kernel_umount_init();
}

void ksu_setuid_hook_exit(void)
{
    pr_info("esu setuid hook exit\n");
    ksu_kernel_umount_exit();
}
