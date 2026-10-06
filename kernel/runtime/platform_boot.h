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
 * Android and recovery select their own variant: `on init` owns the ESP
 * lifecycle, the staged binaries, module RC publication and the overlays whose
 * target partitions exist, so recovery needs it too, while `on post-fs-data`
 * and `on property:sys.boot_completed=1` stay defined for both and simply
 * never fire without /data and a completed boot. `ksud_path` and
 * `kernel_su_domain` are KSUD_PATH and KERNEL_SU_DOMAIN from the kernel esu
 * headers.
 */
// clang-format off
#define ESU_PLATFORM_RC_SERVICE_HEAD(ksud_path, kernel_su_domain) \
    "\n" \
    "service esu-early " ksud_path " early\n" \
    "    user root\n" \
    "    group root\n" \
    "    seclabel u:r:" kernel_su_domain ":s0\n" \
    "    disabled\n" \
    "    oneshot\n"

/*
 * Android arms reboot_on_failure so a failed esu-early aborts the boot.
 * Recovery has no rescue path to fall back to, so its RC omits the line and
 * lets init log the failure instead of looping the recovery session.
 */
#define ESU_PLATFORM_RC_REBOOT_ON_FAILURE "    reboot_on_failure reboot\n"

#define ESU_PLATFORM_RC_TAIL(ksud_path, kernel_su_domain) \
    "\n" \
    "on init\n" \
    "    exec u:r:" kernel_su_domain ":s0 root -- /system/bin/toybox chcon -R u:object_r:esu_file:s0 /debug_ramdisk/esu\n" \
    "    mkdir /dev/efivars 0755 root root\n" \
    "    mount efivarfs none /dev/efivars nosuid nodev noexec context=u:object_r:esu_file:s0\n" \
    "    exec_start esu-early\n" \
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

#define ESU_PLATFORM_RC_ANDROID(ksud_path, kernel_su_domain) \
    ESU_PLATFORM_RC_SERVICE_HEAD(ksud_path, kernel_su_domain) \
    ESU_PLATFORM_RC_REBOOT_ON_FAILURE \
    ESU_PLATFORM_RC_TAIL(ksud_path, kernel_su_domain)

#define ESU_PLATFORM_RC_RECOVERY(ksud_path, kernel_su_domain) \
    ESU_PLATFORM_RC_SERVICE_HEAD(ksud_path, kernel_su_domain) \
    ESU_PLATFORM_RC_TAIL(ksud_path, kernel_su_domain)
// clang-format on

#endif
