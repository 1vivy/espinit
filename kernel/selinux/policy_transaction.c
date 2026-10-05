// SPDX-License-Identifier: GPL-3.0-only
#include <linux/err.h>
#include <linux/mutex.h>
#include <linux/rcupdate.h>
#include <linux/slab.h>
#include <linux/version.h>

#include "security.h"
#include "ss/services.h"
#include "xfrm.h"

#include "klog.h" // IWYU pragma: keep
#include "ksu.h"
#include "selinux/policy_transaction.h"
#include "selinux/selinux.h"
#include "selinux/sepolicy.h"

struct selinux_policy *backup_sepolicy;

static DEFINE_MUTEX(espinit_policy_lock);
static bool espinit_policy_applied;

#if (LINUX_VERSION_CODE >= KERNEL_VERSION(6, 4, 0))
extern int avc_ss_reset(u32 seqno);
#else
extern int avc_ss_reset(struct selinux_avc *avc, u32 seqno);
#endif

void espinit_policy_reset_avc(void)
{
#if (LINUX_VERSION_CODE >= KERNEL_VERSION(6, 4, 0))
    avc_ss_reset(0);
    selnl_notify_policyload(0);
    selinux_status_update_policyload(0);
#else
    struct selinux_avc *avc = selinux_state.avc;
    avc_ss_reset(avc, 0);
    selnl_notify_policyload(0);
    selinux_status_update_policyload(&selinux_state, 0);
#endif
    selinux_xfrm_notify_policyload();
}

static int prepare_backup(struct selinux_policy *source)
{
    struct selinux_policy *backup;
    int error;

    if (backup_sepolicy)
        return 0;

    backup = ksu_dup_sepolicy(source);
    if (IS_ERR(backup))
        return PTR_ERR(backup);

    backup->sidtab = kzalloc(sizeof(*backup->sidtab), GFP_KERNEL);
    if (!backup->sidtab) {
        ksu_destroy_sepolicy(backup);
        return -ENOMEM;
    }

    error = policydb_load_isids(&backup->policydb, backup->sidtab);
    if (error) {
        kfree(backup->sidtab);
        ksu_destroy_sepolicy(backup);
        return error;
    }

    backup_sepolicy = backup;
    pr_info("backup sepolicy success! latest_granting=%d\n",
            backup->latest_granting);
    return 0;
}

int espinit_policy_apply_once(espinit_policy_mutator_t mutate)
{
    struct selinux_policy *policy, *old_policy;
    int error;

    mutex_lock(&espinit_policy_lock);
    if (espinit_policy_applied) {
        mutex_unlock(&espinit_policy_lock);
        return 0;
    }

    mutex_lock(&selinux_state.policy_mutex);
    old_policy = rcu_dereference_protected(
        selinux_state.policy,
        lockdep_is_held(&selinux_state.policy_mutex));

    error = prepare_backup(old_policy);
    if (error) {
        pr_err("failed to create backup sepolicy: %d\n", error);
        goto out_unlock_policy;
    }

    policy = ksu_dup_sepolicy(old_policy);
    if (IS_ERR(policy)) {
        error = PTR_ERR(policy);
        pr_err("failed to duplicate sepolicy: %d\n", error);
        goto out_unlock_policy;
    }

    error = mutate(&policy->policydb);
    if (error) {
        ksu_destroy_sepolicy(policy);
        goto out_unlock_policy;
    }

    rcu_assign_pointer(selinux_state.policy, policy);
    synchronize_rcu();
    ksu_destroy_sepolicy(old_policy);
    espinit_policy_reset_avc();
    espinit_policy_applied = true;
    pr_info("espinit SELinux rules applied\n");

out_unlock_policy:
    mutex_unlock(&selinux_state.policy_mutex);
    mutex_unlock(&espinit_policy_lock);
    return error;
}
