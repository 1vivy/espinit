#ifndef __KSU_H_KSUD
#define __KSU_H_KSUD

#include <asm/syscall.h>

#define KSUD_PATH "/metadata/esu/esud"

void ksu_esud_init();
void ksu_esud_exit();

void ksu_execve_hook_esud(const struct pt_regs *regs);
void ksu_execveat_hook_esud(const struct pt_regs *regs);
void ksu_stop_input_hook_runtime(void);

#endif
