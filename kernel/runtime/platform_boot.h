/* SPDX-License-Identifier: GPL-3.0-only */
#ifndef ESU_PLATFORM_BOOT_H
#define ESU_PLATFORM_BOOT_H

/* Set exactly once by the validated PID1 payload, never inferred by init. */
enum esu_platform_boot_mode {
    ESU_PLATFORM_UNSET = 0,
    ESU_PLATFORM_ANDROID = 1,
    ESU_PLATFORM_RECOVERY = 2,
};

int esu_set_platform_boot_mode(int mode);
int esu_get_platform_boot_mode(void);

static inline unsigned long esu_platform_rc_size(int mode, unsigned long normal_size)
{
    return mode == ESU_PLATFORM_ANDROID ? normal_size : 0;
}

#endif
