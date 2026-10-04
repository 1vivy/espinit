#ifndef __KSU_H_SUPERCALL_INTERNAL
#define __KSU_H_SUPERCALL_INTERNAL

#include <linux/fs.h>
#include <linux/types.h>
#include <linux/uaccess.h>

bool only_root(void);
bool always_allow(void);

long ksu_supercall_handle_ioctl(const struct file *filp, unsigned int cmd, void __user *argp);
void ksu_supercall_dump_commands(void);
void ksu_supercall_cleanup_state(void);

#endif // __KSU_H_SUPERCALL_INTERNAL
