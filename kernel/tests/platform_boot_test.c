/* SPDX-License-Identifier: GPL-3.0-only */
#include <assert.h>
#include <stddef.h>
#include "../../uapi/supercall.h"

int main(void)
{
    assert(ESU_UAPI_VERSION == 4);
    assert(sizeof(struct esu_get_info_cmd) == 16);
    assert(ESU_IOCTL_GET_INFO == 0x80104502);
    assert(ESU_IOCTL_SET_SEPOLICY == 0xc0004504);
    assert(sizeof(struct ksu_set_sepolicy_cmd) == 16);
    assert(offsetof(struct ksu_set_sepolicy_cmd, data) == 8);
    assert(ESU_IOCTL_GET_SEPOLICY == 0xc0104516);
    assert(sizeof(struct esu_get_sepolicy_cmd) == 16);
    assert(offsetof(struct esu_get_sepolicy_cmd, ptr) == 0);
    assert(offsetof(struct esu_get_sepolicy_cmd, len) == 8);
    assert(ESU_POLICY_MAX_SIZE == 64U * 1024U * 1024U);
    assert(_IOC_TYPE(ESU_IOCTL_GET_SEPOLICY) == 'E');
    assert(_IOC_NR(ESU_IOCTL_GET_SEPOLICY) == 22);
    assert(_IOC_SIZE(ESU_IOCTL_GET_SEPOLICY) == 16);
    assert(_IOC_DIR(ESU_IOCTL_GET_SEPOLICY) == (_IOC_READ | _IOC_WRITE));
    assert(ESU_INSTALL_MAGIC1 == 0x45535049);
    assert(ESU_INSTALL_MAGIC2 == 0x4e495446);
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
