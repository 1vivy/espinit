// SPDX-License-Identifier: GPL-2.0-only
/*
 * esu thin.ko entry point.
 *
 * Brings up the vendored dm-thin family and exposes the read-only readiness
 * parameter used by the esu PID1 self-check:
 *   ready       "Y" only after every subsystem and both device-mapper targets
 *               initialized, and "N" again as soon as unload teardown starts.
 *
 * The private symbol renames in private-rename.h keep the vendored device-mapper
 * code disjoint from the kernel's own dm-* symbols, and the device-mapper target
 * names stay the proven "thin-pool" and "thin". See PROVENANCE.md.
 */
#include <linux/init.h>
#include <linux/module.h>
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

MODULE_DESCRIPTION("esu thin provisioning targets (thin-pool, thin)");
MODULE_AUTHOR("esu");
MODULE_LICENSE("GPL v2");
