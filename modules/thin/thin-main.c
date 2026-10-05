// SPDX-License-Identifier: GPL-2.0-only
/*
 * espinit thin.ko entry point.
 *
 * Brings up the vendored dm-thin family in one module and exposes exactly the
 * two read-only parameters the espinit PID-1 self-check reads:
 *
 *   generation  build generation compiled into this module; it must equal the
 *               payload generation derived from ESPINIT_GENERATION or the Git
 *               HEAD of this repository.
 *   ready       "Y" only after every subsystem and both device-mapper targets
 *               initialized, and "N" again as soon as unload teardown starts.
 *
 * The private symbol renames in private-rename.h keep the vendored device-mapper
 * code disjoint from the kernel's own dm-* symbols, and the device-mapper target
 * names stay the proven "thin-pool" and "thin". See PROVENANCE.md.
 */
#include <linux/init.h>
#include <linux/module.h>
#include <linux/string.h>

#ifndef ESPINIT_GENERATION
#error "ESPINIT_GENERATION must be defined by modules/thin/Makefile"
#endif


static char generation[] = ESPINIT_GENERATION;

static_assert(sizeof(generation) > 1 && sizeof(generation) <= 64,
	      "generation must be a nonempty value of at most 63 bytes");
module_param_string(generation, generation, sizeof(generation), 0444);
MODULE_PARM_DESC(generation, "espinit build generation (read-only)");

static bool ready;
module_param(ready, bool, 0444);
MODULE_PARM_DESC(ready,
		 "all thin subsystems and targets initialized (read-only)");

int thinpool_private_dm_io_init(void);
void thinpool_private_dm_io_exit(void);
int dm_kcopyd_init(void);
void dm_kcopyd_exit(void);
int thinpool_private_bufio_init(void);
void thinpool_private_bufio_exit(void);
int thinpool_private_prison_init(void);
void thinpool_private_prison_exit(void);
int thinpool_private_thin_init(void);
void thinpool_private_thin_exit(void);

static int __init thinpool_private_init(void)
{
	int r;

	ready = false;
	if (strcmp(generation, ESPINIT_GENERATION))
		return -EINVAL;

	r = thinpool_private_dm_io_init();
	if (r)
		return r;

	r = dm_kcopyd_init();
	if (r) {
		thinpool_private_dm_io_exit();
		return r;
	}

	r = thinpool_private_bufio_init();
	if (r) {
		dm_kcopyd_exit();
		thinpool_private_dm_io_exit();
		return r;
	}

	r = thinpool_private_prison_init();
	if (r) {
		thinpool_private_bufio_exit();
		dm_kcopyd_exit();
		thinpool_private_dm_io_exit();
		return r;
	}

	r = thinpool_private_thin_init();
	if (r) {
		thinpool_private_prison_exit();
		thinpool_private_bufio_exit();
		dm_kcopyd_exit();
		thinpool_private_dm_io_exit();
		return r;
	}

	ready = true;
	return 0;
}

static void __exit thinpool_private_exit(void)
{
	ready = false;
	thinpool_private_thin_exit();
	thinpool_private_prison_exit();
	thinpool_private_bufio_exit();
	dm_kcopyd_exit();
	thinpool_private_dm_io_exit();
}

module_init(thinpool_private_init);
module_exit(thinpool_private_exit);

MODULE_DESCRIPTION("espinit thin provisioning targets (thin-pool, thin)");
MODULE_AUTHOR("espinit");
MODULE_LICENSE("GPL v2");
