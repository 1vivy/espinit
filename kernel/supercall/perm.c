#include <linux/cred.h>
#include <linux/types.h>

#include "supercall/internal.h"

bool only_root(void)
{
    return uid_eq(current_euid(), GLOBAL_ROOT_UID);
}
