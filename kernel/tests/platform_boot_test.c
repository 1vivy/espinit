/* SPDX-License-Identifier: GPL-3.0-only */
#include <assert.h>
#include "../runtime/platform_boot.h"

int main(void)
{
    const unsigned long normal_size = 4096;

    assert(espinit_platform_rc_size(ESPINIT_PLATFORM_ANDROID, normal_size) == normal_size);
    assert(espinit_platform_rc_size(ESPINIT_PLATFORM_RECOVERY, normal_size) == 0);
    assert(espinit_platform_rc_size(ESPINIT_PLATFORM_UNSET, normal_size) == 0);
    assert(espinit_platform_rc_size(-1, normal_size) == 0);
    return 0;
}
