#ifndef __KSU_H_KSUD
#define __KSU_H_KSUD

#include <asm/syscall.h>
#include <linux/types.h>
#include <linux/compiler_types.h>

#define KSUD_PATH "/debug_ramdisk/esu/bin/esud"

int ksu_esud_init(void);
void ksu_esud_exit();
int esu_set_module_rc(const void __user *ptr, u32 len);

void ksu_execve_hook_esud(const struct pt_regs *regs);
void ksu_execveat_hook_esud(const struct pt_regs *regs);
void ksu_stop_input_hook_runtime(void);

#endif
