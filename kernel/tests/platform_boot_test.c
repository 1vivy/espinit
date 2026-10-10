/* SPDX-License-Identifier: GPL-3.0-only */
#include <assert.h>
#include <stddef.h>
#include <string.h>
#include "../../uapi/supercall.h"

int main(void)
{
    assert(EGYSK_UAPI_VERSION == 4);
    assert(sizeof(struct egysk_get_info_cmd) == 16);
    assert(EGYSK_IOCTL_GET_INFO == 0x80104502);
    assert(EGYSK_IOCTL_SET_SEPOLICY == 0xc0004504);
    assert(sizeof(struct ksu_set_sepolicy_cmd) == 16);
    assert(offsetof(struct ksu_set_sepolicy_cmd, data) == 8);
    assert(EGYSK_IOCTL_GET_SEPOLICY == 0xc0104516);
    assert(sizeof(struct egysk_get_sepolicy_cmd) == 16);
    assert(offsetof(struct egysk_get_sepolicy_cmd, ptr) == 0);
    assert(offsetof(struct egysk_get_sepolicy_cmd, len) == 8);
    assert(EGYSK_POLICY_MAX_SIZE == 64U * 1024U * 1024U);
    assert(_IOC_TYPE(EGYSK_IOCTL_GET_SEPOLICY) == 'E');
    assert(_IOC_NR(EGYSK_IOCTL_GET_SEPOLICY) == 22);
    assert(_IOC_SIZE(EGYSK_IOCTL_GET_SEPOLICY) == 16);
    assert(_IOC_DIR(EGYSK_IOCTL_GET_SEPOLICY) == (_IOC_READ | _IOC_WRITE));
    assert(EGYSK_INSTALL_MAGIC1 == 0x45535049);
    assert(EGYSK_INSTALL_MAGIC2 == 0x4e495446);
    assert(sizeof(struct egysk_module_rc_cmd) == 16);
    assert(offsetof(struct egysk_module_rc_cmd, ptr) == 0);
    assert(offsetof(struct egysk_module_rc_cmd, len) == 8);
    assert(offsetof(struct egysk_module_rc_cmd, reserved) == 12);
    assert(_IOC_TYPE(EGYSK_IOCTL_SET_MODULE_RC) == 'E');
    assert(_IOC_NR(EGYSK_IOCTL_SET_MODULE_RC) == 21);
    assert(_IOC_SIZE(EGYSK_IOCTL_SET_MODULE_RC) == 16);
    assert(_IOC_DIR(EGYSK_IOCTL_SET_MODULE_RC) == _IOC_WRITE);
    assert(EGYSK_IOCTL_SET_MODULE_RC == 0x40104515);
    assert(offsetof(struct egysk_get_info_cmd, version) == 0);
    assert(offsetof(struct egysk_get_info_cmd, flags) == 4);
    assert(offsetof(struct egysk_get_info_cmd, uapi_version) == 8);
    assert(offsetof(struct egysk_get_info_cmd, state) == 12);
    assert(strcmp(EGYSK_CONTROL_NAME, "[egysk]") == 0);
    return 0;
}
