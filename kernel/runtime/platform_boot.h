/* SPDX-License-Identifier: GPL-3.0-only */
#ifndef ESPINIT_PLATFORM_BOOT_H
#define ESPINIT_PLATFORM_BOOT_H

/* Set exactly once by the validated PID1 payload, never inferred by init. */
enum espinit_platform_boot_mode {
    ESPINIT_PLATFORM_UNSET = 0,
    ESPINIT_PLATFORM_ANDROID = 1,
    ESPINIT_PLATFORM_RECOVERY = 2,
};

int espinit_set_platform_boot_mode(int mode);
int espinit_get_platform_boot_mode(void);

static inline unsigned long espinit_platform_rc_size(int mode, unsigned long normal_size)
{
    return mode == ESPINIT_PLATFORM_ANDROID ? normal_size : 0;
}

#endif
