/* SPDX-License-Identifier: GPL-3.0-only */
#ifndef ESPINIT_POLICY_TRANSACTION_H
#define ESPINIT_POLICY_TRANSACTION_H

struct policydb;

typedef int (*espinit_policy_mutator_t)(struct policydb *db);

int espinit_policy_apply_once(espinit_policy_mutator_t mutate);
void espinit_policy_reset_avc(void);

#endif
