#ifndef __KSU_H_SYSCALL_EVENT_BRIDGE
#define __KSU_H_SYSCALL_EVENT_BRIDGE

#include <asm/ptrace.h>
#include "hook/syscall_hook.h"

long ksu_hook_execve(syscall_fn_t original, const struct pt_regs *regs);
long ksu_hook_execveat(syscall_fn_t original, const struct pt_regs *regs);

void egysk_stop_init_execve_hook(void);

#endif // __KSU_H_SYSCALL_EVENT_BRIDGE
