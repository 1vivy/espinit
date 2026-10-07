/* SPDX-License-Identifier: GPL-3.0-only */
#include <assert.h>
#include <stddef.h>
#include "../../uapi/supercall.h"

int main(void)
{
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
