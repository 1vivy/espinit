/* SPDX-License-Identifier: GPL-3.0-only */
#ifndef ESU_PLATFORM_BOOT_H
#define ESU_PLATFORM_BOOT_H

/* Selected explicitly by a root caller, never inferred by init. */
enum esu_platform_boot_mode {
    ESU_PLATFORM_UNSET = 0,
    ESU_PLATFORM_ANDROID = 1,
    ESU_PLATFORM_RECOVERY = 2,
};

int esu_set_platform_boot_mode(int mode);
int esu_get_platform_boot_mode(void);

/*
 * Core init RC appended to Android's init.rc by the read/fstat proxies.
 * Plain synchronous execs retain upstream KernelSU failure handling in both
 * Android and recovery. The daemon owns explicitly critical module failures.
 * `ksud_path` and `kernel_su_domain` are KSUD_PATH and KERNEL_SU_DOMAIN.
 */
// clang-format off
#define ESU_PLATFORM_RC(ksud_path, kernel_su_domain) \
    "\n" \
    "on init\n" \
    "    exec u:r:" kernel_su_domain ":s0 root -- /system/bin/toybox chcon -R u:object_r:esu_file:s0 /debug_ramdisk/esu\n" \
    "    mkdir /dev/efivars 0755 root root\n" \
    "    mount efivarfs none /dev/efivars nosuid nodev noexec context=u:object_r:esu_file:s0\n" \
    "    exec u:r:" kernel_su_domain ":s0 root -- " ksud_path " early\n" \
    "\n" \
    "on post-fs\n" \
    "    exec u:r:" kernel_su_domain ":s0 root -- " ksud_path " post-fs\n" \
    "on post-fs-data\n" \
    "    start logd\n" \
    "    exec u:r:" kernel_su_domain ":s0 root -- " ksud_path " post-fs-data\n" \
    "on nonencrypted\n" \
    "    exec u:r:" kernel_su_domain ":s0 root -- " ksud_path " services\n" \
    "on property:vold.decrypt=trigger_restart_framework\n" \
    "    exec u:r:" kernel_su_domain ":s0 root -- " ksud_path " services\n" \
    "on property:sys.boot_completed=1\n" \
    "    exec u:r:" kernel_su_domain ":s0 root -- " ksud_path " boot-completed\n"
// clang-format on

#endif
