/* SPDX-License-Identifier: GPL-3.0-only */
#include <assert.h>
#include <stddef.h>
#include <string.h>
#include "../../uapi/supercall.h"
#include "../runtime/platform_boot.h"

int main(void)
{
    static const char android_rc[] = ESU_PLATFORM_RC_ANDROID("/ksud", "esu");
    static const char recovery_rc[] = ESU_PLATFORM_RC_RECOVERY("/ksud", "esu");

    /* Recovery runs the same `on init` esu path as Android: ESP lifecycle,
     * staged binaries, module RC publication and its overlays. */
    assert(strstr(android_rc, "on init\n"));
    assert(strstr(recovery_rc, "on init\n"));
    assert(strstr(recovery_rc, "/ksud early"));
    assert(strstr(recovery_rc, "seclabel u:r:esu:s0"));
    assert(strstr(recovery_rc, "/ksud post-fs-data"));
    assert(strstr(recovery_rc, "/ksud boot-completed"));
    /* Only Android may turn a failed esu service into a reboot: recovery has
     * no rescue path to fall back to and must log the failure instead. */
    assert(strstr(android_rc, ESU_PLATFORM_RC_REBOOT_ON_FAILURE));
    assert(!strstr(recovery_rc, "reboot_on_failure"));
    assert(sizeof(android_rc) - sizeof(recovery_rc) == sizeof(ESU_PLATFORM_RC_REBOOT_ON_FAILURE) - 1);

    assert(sizeof(struct esu_module_rc_cmd) == 16);
    assert(offsetof(struct esu_module_rc_cmd, ptr) == 0);
    assert(offsetof(struct esu_module_rc_cmd, len) == 8);
    assert(offsetof(struct esu_module_rc_cmd, reserved) == 12);
    assert(_IOC_TYPE(ESU_IOCTL_SET_MODULE_RC) == 'E');
    assert(_IOC_NR(ESU_IOCTL_SET_MODULE_RC) == 21);
    assert(_IOC_SIZE(ESU_IOCTL_SET_MODULE_RC) == 16);
    assert(_IOC_DIR(ESU_IOCTL_SET_MODULE_RC) == _IOC_WRITE);
    return 0;
}
