#include <linux/cred.h>
#include <linux/types.h>

#include "supercall/internal.h"

bool only_root(void)
{
    return current_uid().val == 0;
}

bool always_allow(void)
{
    return true;
}
