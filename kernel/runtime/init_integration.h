#ifndef __EGYSK_H_INIT_INTEGRATION
#define __EGYSK_H_INIT_INTEGRATION

#include <asm/syscall.h>
#include <linux/types.h>
#include <linux/compiler_types.h>


int egysk_init_integration_init(void);
void egysk_init_integration_exit();
int egysk_set_module_rc(const void __user *ptr, u32 len);

void egysk_init_execve_hook(const struct pt_regs *regs);
void egysk_init_execveat_hook(const struct pt_regs *regs);
bool egysk_hooks_ready(void);

#endif
