#include "selinux.h"
#include "security.h"
#include <linux/security.h>
#include <linux/version.h>
#include <linux/string.h>
#include "klog.h"

u32 egysk_sid __read_mostly;
u32 egysk_file_sid __read_mostly;
u32 egysk_log_file_sid __read_mostly;

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
        pr_warn("egysk: context %s unavailable: %d\n", context, ret);
    WRITE_ONCE(*sid, value);
}

void cache_sid(void)
{
    cache_context(KERNEL_SU_CONTEXT, &egysk_sid);
    cache_context(KSU_FILE_CONTEXT, &egysk_file_sid);
    cache_context("u:object_r:egysk_log_file:s0", &egysk_log_file_sid);
}
