/* SPDX-License-Identifier: GPL-2.0 */
#ifndef ESU_IMPORT_H
#define ESU_IMPORT_H

/*
 * Declare a deliberate non-KMI import. esuinit's relocator resolves only the
 * names listed in .esu_imports from kallsyms; every other undefined symbol is
 * left to the kernel, which checks it against exports and modversions CRCs.
 *
 *   ESU_IMPORT(blk_set_stacking_limits);
 *
 * A module with no ESU_IMPORT keeps the legacy behavior (all undefined
 * symbols resolved from kallsyms).
 */
#define __ESU_IMPORT_ID(a, b) a##b
#define __ESU_IMPORT_UNIQ(sym, line) __ESU_IMPORT_ID(__esu_import_##sym##_, line)
#define ESU_IMPORT(sym)                                                        \
	static const char __ESU_IMPORT_UNIQ(sym, __LINE__)[]                   \
		__used __section(".esu_imports") __aligned(1) = #sym

#endif /* ESU_IMPORT_H */
