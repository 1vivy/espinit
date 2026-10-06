/* SPDX-License-Identifier: GPL-3.0-only */
#ifndef ESU_POLICY_TRANSACTION_H
#define ESU_POLICY_TRANSACTION_H

struct policydb;

typedef int (*esu_policy_mutator_t)(struct policydb *db);

int esu_policy_apply_once(esu_policy_mutator_t mutate);
void esu_policy_reset_avc(void);

#endif
