#ifndef __KSU_H_KSU_SYSCALL_HOOK
#define __KSU_H_KSU_SYSCALL_HOOK
#include <asm/syscall.h>
#include <asm/unistd.h>

#if defined(__x86_64__)
typedef sys_call_ptr_t syscall_fn_t;
#elif defined(__riscv)
typedef long (*syscall_fn_t)(const struct pt_regs *);
#endif

extern syscall_fn_t *ksu_syscall_table;

/* Saved original is written before publication. Returns an installation error;
 * duplicates and full tracking storage are rejected before changing the table.
 */
int ksu_syscall_table_hook(int nr, syscall_fn_t fn, syscall_fn_t *old);
/* Restore only if we still own the entry, compared inside stop_machine.
 * On -EBUSY another owner may chain to us; retain tracking and callback state.
 * Safe from inside read/fstat: no callback drain or freeing is performed.
 */
int ksu_syscall_table_unhook(int nr);
int ksu_syscall_hook_init(void);
void ksu_syscall_hook_exit(void);
void ksu_arch_syscall_hook_exit(void);
void ksu_syscall_hook_pin(void);
bool ksu_syscall_hooks_published(void);
bool esu_hooks_ready(void);

#endif
