#include "selinux.h"
#include "security.h"
#include <linux/security.h>
#include <linux/version.h>
#include <linux/string.h>
#include "klog.h"

u32 esp_sid __read_mostly;
u32 esp_file_sid __read_mostly;
u32 esp_log_file_sid __read_mostly;

bool getenforce(void)
{
#ifdef CONFIG_SECURITY_SELINUX_DISABLE
    if (selinux_state.disabled)
        return false;
#endif
#ifdef CONFIG_SECURITY_SELINUX_DEVELOP
    return selinux_state.enforcing;
#else
    return true;
#endif
}

static void cache_context(const char *context, u32 *sid)
{
    u32 value = 0;
    int ret = security_secctx_to_secid(context, strlen(context), &value);

    if (ret)
        pr_warn("kernelsu-esp: context %s unavailable: %d\n", context, ret);
    WRITE_ONCE(*sid, value);
}

void cache_sid(void)
{
    cache_context(KERNEL_SU_CONTEXT, &esp_sid);
    cache_context(KSU_FILE_CONTEXT, &esp_file_sid);
    cache_context("u:object_r:esp_log_file:s0", &esp_log_file_sid);
}
