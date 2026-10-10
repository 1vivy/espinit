#ifndef __EGYSK_H_SELINUX
#define __EGYSK_H_SELINUX

#include <linux/types.h>
#include <linux/compiler_types.h>

#define KERNEL_SU_DOMAIN "egysk"
#define KERNEL_SU_FILE "egysk_file"
#define KERNEL_SU_CONTEXT "u:r:" KERNEL_SU_DOMAIN ":s0"
#define KSU_FILE_CONTEXT "u:object_r:" KERNEL_SU_FILE ":s0"

bool getenforce(void);
void cache_sid(void);
int apply_kernelsu_rules(void);
int handle_sepolicy(void __user *user_data, u64 data_len);
extern u32 egysk_sid;
extern u32 egysk_file_sid;
extern u32 egysk_log_file_sid;

#endif
