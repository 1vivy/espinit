/* SPDX-License-Identifier: GPL-3.0-only */
#include <assert.h>
#include <stddef.h>
#include "../../uapi/supercall.h"
#include "../runtime/platform_boot.h"

int main(void)
{
    const unsigned long normal_size = 4096;

    assert(esu_platform_rc_size(ESU_PLATFORM_ANDROID, normal_size) == normal_size);
    assert(esu_platform_rc_size(ESU_PLATFORM_RECOVERY, normal_size) == 0);
    assert(esu_platform_rc_size(ESU_PLATFORM_UNSET, normal_size) == 0);
    assert(esu_platform_rc_size(-1, normal_size) == 0);
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
