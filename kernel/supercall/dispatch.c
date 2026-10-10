#include <linux/cred.h>
#include <linux/module.h>
#include <linux/uaccess.h>
#include "uapi/supercall.h"
#include "supercall/internal.h"
#include "supercall/supercall.h"
#include "runtime/esud.h"
#include "selinux/selinux.h"
#include "selinux/sepolicy.h"
#include "ksu.h"
#include "klog.h"

static int do_get_info(void __user *arg)
{
    struct esu_get_info_cmd cmd = {
        .version = KERNEL_SU_VERSION,
        .uapi_version = ESU_UAPI_VERSION,
    };
#ifdef MODULE
    cmd.flags = ESU_GET_INFO_FLAG_LKM;
    if (THIS_MODULE->state == MODULE_STATE_LIVE && esu_hooks_ready())
#else
    if (esu_hooks_ready())
#endif
        cmd.state = ESU_STATE_READY;
    return copy_to_user(arg, &cmd, sizeof(cmd)) ? -EFAULT : 0;
}

static int do_set_module_rc(void __user *arg)
{
    struct esu_module_rc_cmd cmd;

    if (copy_from_user(&cmd, arg, sizeof(cmd)))
        return -EFAULT;
    if (cmd.reserved || cmd.len > 65536 || (cmd.len && !cmd.ptr))
        return -EINVAL;
    return esu_set_module_rc(u64_to_user_ptr(cmd.ptr), cmd.len);
}

static int do_set_sepolicy(void __user *arg)
{
    struct ksu_set_sepolicy_cmd cmd;

    if (copy_from_user(&cmd, arg, sizeof(cmd)))
        return -EFAULT;
    return handle_sepolicy(u64_to_user_ptr(cmd.data), cmd.data_len);
}

static const struct ksu_ioctl_cmd_map ksu_ioctl_handlers[] = {
    { ESU_IOCTL_GET_INFO, "GET_INFO", do_get_info, only_root },
    { ESU_IOCTL_SET_MODULE_RC, "SET_MODULE_RC", do_set_module_rc, only_root },
    { ESU_IOCTL_SET_SEPOLICY, "SET_SEPOLICY", do_set_sepolicy, only_root },
    { ESU_IOCTL_GET_SEPOLICY, "GET_SEPOLICY", ksu_get_sepolicy, only_root },
};

long ksu_supercall_handle_ioctl(const struct file *filp, unsigned int cmd, void __user *argp)
{
    unsigned int i;

    for (i = 0; i < ARRAY_SIZE(ksu_ioctl_handlers); i++) {
        if (cmd != ksu_ioctl_handlers[i].cmd)
            continue;
        if (!ksu_ioctl_handlers[i].perm_check())
            return -EPERM;
        return ksu_ioctl_handlers[i].handler(argp);
    }
    return -ENOTTY;
}

void __init ksu_supercall_dump_commands(void)
{
    unsigned int i;

    for (i = 0; i < ARRAY_SIZE(ksu_ioctl_handlers); i++)
        pr_info("kernelsu-esp %-18s = 0x%08x\n", ksu_ioctl_handlers[i].name,
                ksu_ioctl_handlers[i].cmd);
}
