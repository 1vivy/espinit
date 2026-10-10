#include <linux/anon_inodes.h>
#include <linux/cred.h>
#include <linux/err.h>
#include <linux/fdtable.h>
#include <linux/file.h>
#include <linux/fs.h>
#include <linux/kprobes.h>
#include <linux/pid.h>
#include <linux/slab.h>
#include <linux/syscalls.h>
#include <linux/task_work.h>
#include <linux/uaccess.h>
#include <linux/version.h>

#include "uapi/supercall.h"
#include "supercall/internal.h"
#include "supercall/supercall.h"
#include "arch.h"
#include "util.h"
#include "klog.h" // IWYU pragma: keep

struct ksu_install_fd_tw {
    struct callback_head cb;
    int __user *outp;
};

static int anon_egysk_release(struct inode *inode, struct file *filp)
{
    pr_info("egysk fd released\n");
    return 0;
}

static long anon_egysk_ioctl(struct file *filp, unsigned int cmd, unsigned long arg)
{
    return ksu_supercall_handle_ioctl(filp, cmd, (void __user *)arg);
}

static const struct file_operations anon_egysk_fops = {
    .owner = THIS_MODULE,
    .unlocked_ioctl = anon_egysk_ioctl,
    .compat_ioctl = anon_egysk_ioctl,
    .release = anon_egysk_release,
};

int ksu_install_fd(void)
{
    struct file *filp;
    int fd;

    fd = get_unused_fd_flags(O_CLOEXEC);
    if (fd < 0) {
        pr_err("egysk_install_fd: failed to get unused fd\n");
        return fd;
    }

    filp = anon_inode_getfile(EGYSK_CONTROL_NAME, &anon_egysk_fops, NULL, O_RDWR);
    if (IS_ERR(filp)) {
        pr_err("egysk_install_fd: failed to create anon inode file\n");
        put_unused_fd(fd);
        return PTR_ERR(filp);
    }

    fd_install(fd, filp);
    pr_info("egysk fd installed: %d for pid %d\n", fd, current->pid);
    return fd;
}

static void ksu_install_fd_tw_func(struct callback_head *cb)
{
    struct ksu_install_fd_tw *tw = container_of(cb, struct ksu_install_fd_tw, cb);
    int fd = ksu_install_fd();

    pr_info("[%d] install egysk fd: %d\n", current->pid, fd);
    if (copy_to_user(tw->outp, &fd, sizeof(fd))) {
        pr_err("install egysk fd reply err\n");
        ksu_close_fd(fd);
    }

    kfree(tw);
}

static int reboot_handler_pre(struct kprobe *p, struct pt_regs *regs)
{
    struct pt_regs *real_regs = PT_REAL_REGS(regs);
    int magic1 = (int)PT_REGS_SYSCALL_PARM1(real_regs);
    int magic2 = (int)PT_REGS_PARM2(real_regs);

    if (magic1 == EGYSK_INSTALL_MAGIC1 && magic2 == EGYSK_INSTALL_MAGIC2 &&
        uid_eq(current_euid(), GLOBAL_ROOT_UID)) {
        struct ksu_install_fd_tw *tw;
        unsigned long arg4 = (unsigned long)PT_REGS_SYSCALL_PARM4(real_regs);

        tw = kzalloc(sizeof(*tw), GFP_ATOMIC);
        if (!tw)
            return 0;

        tw->outp = (int __user *)arg4;
        tw->cb.func = ksu_install_fd_tw_func;

        if (task_work_add(current, &tw->cb, TWA_RESUME)) {
            kfree(tw);
            pr_warn("install fd add task_work failed\n");
        }
    }

    return 0;
}

static struct kprobe reboot_kp = {
    .symbol_name = REBOOT_SYMBOL,
    .pre_handler = reboot_handler_pre,
};

int __init ksu_supercalls_init(void)
{
    int rc;

    ksu_supercall_dump_commands();

    rc = register_kprobe(&reboot_kp);
    if (rc) {
        pr_err("reboot kprobe failed: %d\n", rc);
    } else {
        pr_info("reboot kprobe registered successfully\n");
    }
    return rc;
}

void ksu_supercalls_exit(void)
{
    unregister_kprobe(&reboot_kp);
}
