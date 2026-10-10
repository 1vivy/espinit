#include <linux/rcupdate.h>
#include <linux/slab.h>
#include <linux/mutex.h>
#include <linux/mm.h>
#include <asm/current.h>
#include <linux/compat.h>
#include <linux/cred.h>
#include <linux/dcache.h>
#include <linux/err.h>
#include <linux/file.h>
#include <linux/fs.h>
#include <linux/version.h>
#include <linux/printk.h>
#include <linux/types.h>
#include <linux/uaccess.h>
#include <linux/namei.h>
#include <linux/uio.h>
#include <linux/stat.h>

#include "arch.h"
#include "klog.h" // IWYU pragma: keep
#include "ksu.h"
#include "runtime/init_integration.h"
#include "selinux/selinux.h"
#include "hook/syscall_hook.h"
#include "hook/syscall_event_bridge.h"
static void stop_init_rc_hook();

struct user_arg_ptr {
#ifdef CONFIG_COMPAT
    bool is_compat;
#endif
    union {
        const char __user *const __user *native;
#ifdef CONFIG_COMPAT
        const compat_uptr_t __user *compat;
#endif
    } ptr;
};

static const char __user *get_user_arg_ptr(struct user_arg_ptr argv, int nr)
{
    const char __user *native;

#ifdef CONFIG_COMPAT
    if (unlikely(argv.is_compat)) {
        compat_uptr_t compat;

        if (get_user(compat, argv.ptr.compat + nr))
            return ERR_PTR(-EFAULT);

        return compat_ptr(compat);
    }
#endif

    if (get_user(native, argv.ptr.native + nr))
        return ERR_PTR(-EFAULT);

    return native;
}


static bool check_argv(struct user_arg_ptr argv, int index, const char *expected, char *buf, size_t buf_len)
{
    const char __user *p;

    p = get_user_arg_ptr(argv, index);
    if (!p || IS_ERR(p))
        goto fail;

    if (strncpy_from_user_nofault(buf, p, buf_len) <= 0)
        goto fail;

    buf[buf_len - 1] = '\0';
    return !strcmp(buf, expected);

fail:
    pr_err("check_argv failed\n");
    return false;
}

void egysk_observe_second_stage(const char *path, struct user_arg_ptr *argv)
{

    // https://cs.android.com/android/platform/superproject/+/android-16.0.0_r2:system/core/init/main.cpp;l=77
    if (current->pid == 1 && !strcmp(path, "/system/bin/init") && argv) {
        char buf[16];
        if (check_argv(*argv, 1, "second_stage", buf, sizeof(buf)) && !apply_kernelsu_rules()) {
            cache_sid();
            egysk_stop_init_execve_hook();
        }
    }

}

static ssize_t (*orig_read)(struct file *, char __user *, size_t, loff_t *);
static ssize_t (*orig_read_iter)(struct kiocb *, struct iov_iter *);
static struct file_operations fops_proxy;

static DEFINE_MUTEX(module_rc_lock);
static char *module_rc_buf;
static size_t module_rc_len;
static ssize_t module_rc_pos;
static bool module_rc_set;
static bool module_rc_loaded;

int egysk_set_module_rc(const void __user *ptr, u32 len)
{
    char *buf = NULL;
    int ret = 0;

    if (len > 65536 || (len && !ptr))
        return -EINVAL;
    mutex_lock(&module_rc_lock);
    if (module_rc_set) {
        ret = -EALREADY;
        goto out;
    }
    if (module_rc_loaded) {
        ret = -EBUSY;
        goto out;
    }
    if (len) {
        buf = kvmalloc(len, GFP_KERNEL);
        if (!buf) {
            ret = -ENOMEM;
            goto out;
        }
        if (copy_from_user(buf, ptr, len)) {
            kvfree(buf);
            ret = -EFAULT;
            goto out;
        }
    }
    module_rc_buf = buf;
    module_rc_len = len;
    module_rc_set = true;
out:
    mutex_unlock(&module_rc_lock);
    return ret;
}

/* Bootstrap and module RC arrive together as one immutable userspace payload. */
static void load_module_rc_once(void)
{
    mutex_lock(&module_rc_lock);
    if (module_rc_loaded)
        goto out;
    if (ksu_no_custom_rc) {
        kvfree(module_rc_buf);
        module_rc_buf = NULL;
        module_rc_len = 0;
    }
    module_rc_loaded = true;
out:
    mutex_unlock(&module_rc_lock);
}

static void free_module_rc(void)
{
    kvfree(module_rc_buf);
    module_rc_buf = NULL;
    module_rc_len = 0;
}

// https://cs.android.com/android/platform/superproject/main/+/main:system/core/init/parser.cpp;l=144;drc=61197364367c9e404c7da6900658f1b16c42d0da
// https://cs.android.com/android/platform/superproject/main/+/main:system/libbase/file.cpp;l=241-243;drc=61197364367c9e404c7da6900658f1b16c42d0da
// The system will read init.rc file until EOF, whenever read() returns 0,
// Append the supplied supplement only when the original read reaches EOF.

static ssize_t read_proxy(struct file *file, char __user *buf, size_t count, loff_t *pos)
{
    ssize_t ret = 0;
    size_t append_count;
    if (module_rc_pos && module_rc_pos < module_rc_len)
        goto append_module_rc;

    ret = orig_read(file, buf, count, pos);
    if (ret != 0) {
        return ret;
    }
    if (module_rc_pos >= module_rc_len) {
        return ret;
    }
    pr_info("read_proxy: orig read finished, start append rc\n");

append_module_rc:
    if (module_rc_pos < module_rc_len && (size_t)ret < count) {
        append_count = module_rc_len - module_rc_pos;
        if (append_count > count - ret)
            append_count = count - ret;
        if (copy_to_user(buf + ret, module_rc_buf + module_rc_pos, append_count)) {
            pr_info("read_proxy: module append error, totally appended %zd\n", module_rc_pos);
            return ret ? ret : -EFAULT;
        }
        pr_info("read_proxy: append module %zu\n", append_count);
        module_rc_pos += append_count;
        ret += append_count;
        if (module_rc_pos == (ssize_t)module_rc_len) {
            pr_info("read_proxy: module append done\n");
            free_module_rc();
        }
    }

    return ret;
}

static ssize_t read_iter_proxy(struct kiocb *iocb, struct iov_iter *to)
{
    ssize_t ret = 0;
    size_t append_count;
    if (module_rc_pos && module_rc_pos < module_rc_len)
        goto append_module_rc;

    ret = orig_read_iter(iocb, to);
    if (ret != 0) {
        return ret;
    }
    if (module_rc_pos >= module_rc_len) {
        return ret;
    }
    pr_info("read_iter_proxy: orig read finished, start append rc\n");

append_module_rc:
    if (module_rc_pos < module_rc_len) {
        append_count = copy_to_iter(module_rc_buf + module_rc_pos, module_rc_len - module_rc_pos, to);
        if (!append_count) {
            pr_info("read_iter_proxy: module append error, appended %zd\n", module_rc_pos);
            return ret ? ret : (iov_iter_count(to) ? -EFAULT : 0);
        }
        pr_info("read_iter_proxy: append module %zu\n", append_count);
        module_rc_pos += append_count;
        ret += append_count;
        if (module_rc_pos == (ssize_t)module_rc_len) {
            pr_info("read_iter_proxy: module append done\n");
            free_module_rc();
        }
    }
    return ret;
}

static bool is_init_rc(struct file *fp)
{
    if (current->pid != 1 || strcmp(current->comm, "init")) {
        // we are only interest in `init` process
        return false;
    }

    if (!d_is_reg(fp->f_path.dentry)) {
        return false;
    }

    const char *short_name = fp->f_path.dentry->d_name.name;
    if (strcmp(short_name, "init.rc")) {
        // we are only interest `init.rc` file name file
        return false;
    }
    char path[256];
    char *dpath = d_path(&fp->f_path, path, sizeof(path));

    if (IS_ERR(dpath)) {
        return false;
    }

    if (strcmp(dpath, "/system/etc/init/hw/init.rc")) {
        return false;
    }

    return true;
}

static int ksu_install_rc_hook(struct file *file)
{
    static bool rc_hooked;

    if (!is_init_rc(file) || rc_hooked)
        return 0;
    load_module_rc_once();
    rc_hooked = true;
    stop_init_rc_hook();

    pr_info("read init.rc, supplement: %zu bytes\n", module_rc_len);

    // Now we need to proxy the read and modify the result!
    // But, we can not modify the file_operations directly, because it's in read-only memory.
    // We just replace the whole file_operations with a proxy one.
    memcpy(&fops_proxy, file->f_op, sizeof(struct file_operations));
    orig_read = file->f_op->read;
    if (orig_read) {
        fops_proxy.read = read_proxy;
    }
    orig_read_iter = file->f_op->read_iter;
    if (orig_read_iter) {
        fops_proxy.read_iter = read_iter_proxy;
    }
    // replace the file_operations
    file->f_op = &fops_proxy;
    return 0;
}

static int ksu_handle_sys_read(unsigned int fd)
{
    struct file *file = fget(fd);
    int ret;

    if (!file)
        return 0;
    ret = ksu_install_rc_hook(file);
    fput(file);
    return ret;
}


static void egysk_init_execve_hook_common(const char __user *filename_user, const char __user *const __user *argv_user)
{
    struct user_arg_ptr argv = { .ptr.native = argv_user };
    char path[32];
    long ret;
    unsigned long addr;
    const char __user *fn;

    if (!filename_user)
        return;

    addr = untagged_addr((unsigned long)filename_user);
    fn = (const char __user *)addr;

    memset(path, 0, sizeof(path));
    ret = strncpy_from_user(path, fn, 32);
    if (ret < 0 || ret >= sizeof(path)) {
        pr_err("Access filename failed for execve_handler_pre\n");
        return;
    }

    egysk_observe_second_stage(path, &argv);
}

void egysk_init_execve_hook(const struct pt_regs *regs)
{
    const char __user *filename_user = (const char __user *)PT_REGS_SYSCALL_PARM1(regs);
    const char __user *const __user *argv_user = (const char __user *const __user *)PT_REGS_PARM2(regs);

    egysk_init_execve_hook_common(filename_user, argv_user);
}

void egysk_init_execveat_hook(const struct pt_regs *regs)
{
    const char __user *filename_user = (const char __user *)PT_REGS_PARM2(regs);
    const char __user *const __user *argv_user = (const char __user *const __user *)PT_REGS_PARM3(regs);

    egysk_init_execve_hook_common(filename_user, argv_user);
}

static long (*orig_sys_read)(const struct pt_regs *regs);
static long ksu_sys_read(const struct pt_regs *regs)
{
    unsigned int fd = PT_REGS_SYSCALL_PARM1(regs);
    int ret = ksu_handle_sys_read(fd);

    if (ret)
        return ret;
    return orig_sys_read(regs);
}

static long (*orig_sys_fstat)(const struct pt_regs *regs);
static long ksu_sys_fstat(const struct pt_regs *regs)
{
    unsigned int fd = PT_REGS_SYSCALL_PARM1(regs);
    void __user *statbuf = (void __user *)PT_REGS_PARM2(regs);
    bool is_rc = false;
    long ret;

    struct file *file = fget(fd);
    if (file) {
        if (is_init_rc(file)) {
            pr_info("stat init.rc");
            is_rc = true;
            load_module_rc_once();
        }
        fput(file);
    }

    ret = orig_sys_fstat(regs);
    if (ret)
        return ret;

    if (is_rc) {
        void __user *st_size_ptr = statbuf + offsetof(struct stat, st_size);
        long size, new_size;
        size_t extra = module_rc_len;
        if (!copy_from_user_nofault(&size, st_size_ptr, sizeof(long))) {
            new_size = size + extra;
            pr_info("adding rc supplement: %ld -> %ld", size, new_size);
            if (!copy_to_user_nofault(st_size_ptr, &new_size, sizeof(long))) {
                pr_info("added rc len");
            } else {
                pr_err("add rc len failed: statbuf 0x%lx", (unsigned long)st_size_ptr);
                return -EFAULT;
            }
        } else {
            pr_err("read statbuf 0x%lx failed", (unsigned long)st_size_ptr);
            return -EFAULT;
        }
    }

    return ret;
}

static void stop_init_rc_hook()
{
    int read_ret = ksu_syscall_table_unhook(__NR_read);
    int stat_ret = ksu_syscall_table_unhook(__NR_fstat);

    if (!read_ret && !stat_ret)
        pr_info("unregister init_rc syscall hook\n");
}

// Egysk init RC supplement support
int __init egysk_init_integration_init(void)
{
    int ret;

    ret = ksu_syscall_table_hook(__NR_read, ksu_sys_read, &orig_sys_read);
    if (ret)
        goto fail;
    ret = ksu_syscall_table_hook(__NR_fstat, ksu_sys_fstat, &orig_sys_fstat);
    if (ret)
        goto fail;

    return 0;
fail:
    pr_err("egysk: required init_rc hook installation failed: %d\n", ret);
    ksu_syscall_hook_exit();
    return ret;
}

void __exit egysk_init_integration_exit()
{
    /* Forced unload of a published module is unsupported. Do not free data
     * while saved-original chains or file_operations can still reach us.
     */
    if (ksu_syscall_hooks_published())
        return;

    if (module_rc_buf) {
        free_module_rc();
    }
}
