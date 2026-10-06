/* SPDX-License-Identifier: GPL-3.0-only */
#ifndef ESU_SELINUX_POLICY_HOOK_H
#define ESU_SELINUX_POLICY_HOOK_H

int ksu_selinux_policy_hook_init(void);
void ksu_selinux_policy_hook_exit(void);

#endif
