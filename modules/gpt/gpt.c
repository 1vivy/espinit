// SPDX-License-Identifier: GPL-2.0-only
/* A single, ephemeral GPT view. No physical GPT sector is ever modified. */
#include <linux/module.h>
#include <linux/build_bug.h>
#include <linux/blkdev.h>
#include <linux/blk-crypto.h>
#include <linux/blk-crypto-profile.h>
#include <linux/bio.h>
#include <linux/miscdevice.h>
#include <linux/uaccess.h>
#include <linux/capability.h>
#include <linux/compat.h>
#include <linux/crc32.h>
#include <linux/highmem.h>
#include <linux/overflow.h>
#include <linux/random.h>
#include <linux/slab.h>
#include <linux/completion.h>
#include "gpt_uapi.h"

static_assert(sizeof(struct gpt_projection) == 56);
static_assert(sizeof(struct gpt_device) == 8);
static_assert(sizeof(struct gpt_apply) == 9232);
static_assert(sizeof(struct gpt_query) == 16);
static_assert(GPT_IOCTL_APPLY == 0x64104701U);
static_assert(GPT_IOCTL_QUERY == 0x80104702U);

#define GPT_ENTRIES 128U
#define GPT_ENTRY_SIZE 128U
#define GPT_ALIGNMENT 2048ULL /* 1 MiB, in 512-byte sectors */
struct view_map {
	struct file *backend, *whole;
	struct block_device *lower;
	sector_t start, length, offset;
	bool ro;
};
struct hidden_part {
	struct file *file;
	struct block_device *bdev;
	u8 volname[PARTITION_META_INFO_VOLNAMELTH];
	bool changed;
};
struct gpt_view {
	struct gendisk *disk;
	struct bio_set clones;
	bool pool_ready, added;
	unsigned int count, hide_count, block_size;
	sector_t sectors, backup_start;
	size_t primary_bytes, backup_bytes;
	u8 *primary, *backup;
	struct view_map maps[GPT_MAX_PROJECTIONS];
	struct hidden_part hidden[GPT_MAX_HIDDEN];
	/* Seal bookkeeping: what this view made read-only, so teardown can undo
	 * exactly that and nothing else. */
	bool seal;
	struct gendisk **sealed_disks;
	unsigned int sealed_disk_count, sealed_disk_cap;
	struct block_device **sealed_parts;
	unsigned int sealed_part_count, sealed_part_cap;
	atomic_t inflight;
	struct completion drained;
#ifdef CONFIG_BLK_INLINE_ENCRYPTION
	struct blk_crypto_profile crypto;
	bool crypto_ready;
#endif
};
static DEFINE_MUTEX(control_lock);
static struct gpt_view *active;
static bool applied;
static bool ready;
static bool sealed;

/* Sealing is defined below `validate_apply`, but teardown restores it. */
static void unseal_view(struct gpt_view *v);
static int seal_view(struct gpt_view *v);
static bool disk_is_writable_backend(const struct gpt_view *v,
				     const struct gendisk *disk);
static bool part_is_writable_backend(const struct gpt_view *v,
				     const struct block_device *bdev);
static int seal_record_disk(struct gpt_view *v, struct gendisk *disk);
static int seal_record_part(struct gpt_view *v, struct block_device *bdev);
#ifndef ESPINIT_GENERATION
#error "ESPINIT_GENERATION must be defined by modules/gpt/Makefile"
#endif
static char generation[] = ESPINIT_GENERATION;
static_assert(sizeof(generation) >= 2);
static_assert(sizeof(generation) <= 64);
module_param_string(generation, generation, sizeof(generation), 0444);
module_param(ready, bool, 0444);
module_param(sealed, bool, 0444);
MODULE_PARM_DESC(generation, "Payload generation (must match userspace)");
MODULE_PARM_DESC(ready,
		 "Y only after the complete GPT view and hiding are active");
MODULE_PARM_DESC(sealed,
		 "Y while the sealed physical endpoints are enforced");

static u32 gpt_crc(const void *data, size_t size)
{
	return crc32_le(~0U, data, size) ^ ~0U;
}

/* GPT fields are explicitly byte-encoded: no host alignment or endianness. */
static void le32_at(u8 *p, u32 value)
{
	__le32 v = cpu_to_le32(value);
	memcpy(p, &v, sizeof(v));
}
static void le64_at(u8 *p, u64 value)
{
	__le64 v = cpu_to_le64(value);
	memcpy(p, &v, sizeof(v));
}
static void new_guid(u8 *p)
{
	get_random_bytes(p, 16);
	p[7] = (p[7] & 0x0f) | 0x40; /* GPT stores UUID fields little-endian */
	p[8] = (p[8] & 0x3f) | 0x80;
}
static void make_header(u8 *h, u64 here, u64 other, u64 last_usable,
			u64 entries, unsigned int first_usable, const u8 *guid,
			u32 entries_crc)
{
	memcpy(h, "EFI PART", 8);
	le32_at(h + 8, 0x00010000);
	le32_at(h + 12, 92);
	le64_at(h + 24, here);
	le64_at(h + 32, other);
	le64_at(h + 40, first_usable);
	le64_at(h + 48, last_usable);
	memcpy(h + 56, guid, 16);
	le64_at(h + 72, entries);
	le32_at(h + 80, GPT_ENTRIES);
	le32_at(h + 84, GPT_ENTRY_SIZE);
	le32_at(h + 88, entries_crc);
	le32_at(h + 16, gpt_crc(h, 92));
}
static int make_metadata(struct gpt_view *v, const struct gpt_apply *a)
{
	/* Linux filesystem data GUID, in GPT's mixed-endian wire format. */
	static const u8 type_guid[16] = { 0xaf, 0x3d, 0xc6, 0x0f, 0x83, 0x84,
					  0x72, 0x47, 0x8e, 0x79, 0x3d, 0x69,
					  0xd8, 0x47, 0x7d, 0xe4 };
	u8 guid[16], *entries;
	u64 blocks = v->sectors / (v->block_size >> 9);
	unsigned int entry_blocks =
		GPT_ENTRIES * GPT_ENTRY_SIZE / v->block_size;
	unsigned int i, j;
	u32 crc;

	/* Partition scanning uses page-cache reads; include the final metadata
	 * page's zero padding rather than treating part of that page as a gap. */
	v->primary_bytes =
		round_up((2 + entry_blocks) * v->block_size, PAGE_SIZE);
	v->backup_bytes = (1 + entry_blocks) * v->block_size;
	v->backup_start = v->sectors - (v->backup_bytes >> 9);
	v->primary = kzalloc(v->primary_bytes, GFP_KERNEL);
	v->backup = kzalloc(v->backup_bytes, GFP_KERNEL);
	if (!v->primary || !v->backup)
		return -ENOMEM;
	/* Protective MBR covers the entire logical disk (saturating at 2 TiB). */
	v->primary[446 + 1] = 0;
	v->primary[446 + 2] = 2;
	v->primary[446 + 4] = 0xee;
	memset(v->primary + 446 + 5, 0xff, 3);
	le32_at(v->primary + 446 + 8, 1);
	le32_at(v->primary + 446 + 12, min_t(u64, v->sectors - 1, U32_MAX));
	v->primary[510] = 0x55;
	v->primary[511] = 0xaa;
	entries = v->primary + 2 * v->block_size;
	for (i = 0; i < v->count; i++) {
		struct view_map *m = &v->maps[i];
		u8 *e = entries + i * GPT_ENTRY_SIZE;
		memcpy(e, type_guid, 16);
		new_guid(e + 16);
		le64_at(e + 32, m->start / (v->block_size >> 9));
		le64_at(e + 40,
			(m->start + m->length) / (v->block_size >> 9) - 1);
		/* UEFI read-only attribute; per-bdev and I/O policy also enforced. */
		if (m->ro)
			le64_at(e + 48, BIT_ULL(60));
		for (j = 0; a->projections[i].name[j]; j++)
			e[56 + 2 * j] = a->projections[i].name[j];
	}
	crc = gpt_crc(entries, GPT_ENTRIES * GPT_ENTRY_SIZE);
	memcpy(v->backup, entries, GPT_ENTRIES * GPT_ENTRY_SIZE);
	new_guid(guid);
	make_header(v->primary + v->block_size, 1, blocks - 1,
		    blocks - entry_blocks - 2, 2, entry_blocks + 2, guid, crc);
	make_header(v->backup + entry_blocks * v->block_size, blocks - 1, 1,
		    blocks - entry_blocks - 2, blocks - entry_blocks - 1,
		    entry_blocks + 2, guid, crc);
	return 0;
}

static void clone_done(struct bio *clone)
{
	struct bio *original = clone->bi_private;
	struct gpt_view *v = original->bi_bdev->bd_disk->private_data;
	if (clone->bi_status)
		WRITE_ONCE(original->bi_status, clone->bi_status);
	bio_put(clone);
	bio_endio(original);
	if (atomic_dec_and_test(&v->inflight))
		complete(&v->drained);
}

static void view_submit(struct bio *bio)
{
	struct gpt_view *v = bio->bi_bdev->bd_disk->private_data;
	sector_t sector = bio->bi_iter.bi_sector;
	sector_t n = bio_sectors(bio);
	unsigned int i;
	const u8 *source = NULL;
	struct bio_vec bv;
	struct bvec_iter iter;
	size_t offset = 0;

	if (sector > v->sectors || n > v->sectors - sector)
		goto fail;
	/* Empty flushes arrive as REQ_OP_WRITE | REQ_PREFLUSH with sector zero.
	 * Fan them out before extent routing, preserving Linux's flush encoding. */
	if ((bio->bi_opf & REQ_PREFLUSH) && !n) {
		for (i = 0; i < v->count; i++) {
			struct bio *flush;

			if (v->maps[i].ro)
				continue;
			flush = bio_alloc_bioset(v->maps[i].lower, 0,
						 REQ_OP_WRITE | REQ_PREFLUSH,
						 GFP_NOIO, &v->clones);
			if (!flush) {
				bio->bi_status = BLK_STS_RESOURCE;
				break;
			}
			bio_inc_remaining(bio);
			flush->bi_private = bio;
			flush->bi_end_io = clone_done;
			atomic_inc(&v->inflight);
			submit_bio(flush);
		}
		bio_endio(bio);
		return;
	}
	for (i = 0; i < v->count; i++) {
		struct view_map *m = &v->maps[i];
		struct bio *clone;
		if (sector < m->start || sector >= m->start + m->length ||
		    n > m->start + m->length - sector)
			continue;
		if (m->ro && bio_op(bio) != REQ_OP_READ)
			goto fail;
		/* Clone the existing vectors AND crypto/integrity context. Only the
		 * sector changes: the filesystem's DUN must never be recomputed from
		 * our virtual GPT offset. Partition backends bypass their remapper,
		 * targeting a held whole bdev plus the original bd_start_sect. */
		clone = bio_alloc_clone(m->lower, bio, GFP_NOIO, &v->clones);
		if (!clone)
			goto fail;
		clone->bi_iter.bi_sector = sector - m->start + m->offset;
		clone->bi_private = bio;
		clone->bi_end_io = clone_done;
		atomic_inc(&v->inflight);
		submit_bio(clone);
		return;
	}
	if (bio_op(bio) != REQ_OP_READ || bio_has_crypt_ctx(bio))
		goto fail;
	if (sector + n <= (v->primary_bytes >> 9))
		source = v->primary + (sector << 9);
	else if (sector >= v->backup_start)
		source = v->backup + ((sector - v->backup_start) << 9);
	if (!source)
		goto fail; /* gaps and all metadata writes are deliberately rejected */
	bio_for_each_segment(bv, bio, iter) {
		void *p = kmap_local_page(bv.bv_page);
		memcpy(p + bv.bv_offset, source + offset, bv.bv_len);
		kunmap_local(p);
		flush_dcache_page(bv.bv_page);
		offset += bv.bv_len;
	}
	bio_endio(bio);
	return;
fail:
	bio_io_error(bio);
}
static const struct block_device_operations view_ops = {
	.owner = THIS_MODULE,
	.submit_bio = view_submit,
};

#ifdef CONFIG_BLK_INLINE_ENCRYPTION
static int derive_sw_secret(struct blk_crypto_profile *profile,
			    const u8 *eph_key, size_t eph_key_size,
			    u8 sw_secret[BLK_CRYPTO_SW_SECRET_SIZE])
{
	struct gpt_view *v = container_of(profile, struct gpt_view, crypto);
	unsigned int i;
	int err = -EOPNOTSUPP;

	for (i = 0; i < v->count; i++) {
		err = blk_crypto_derive_sw_secret(v->maps[i].lower, eph_key,
						  eph_key_size, sw_secret);
		if (!err)
			break;
	}
	return err;
}

static int evict_key(struct blk_crypto_profile *profile,
		     const struct blk_crypto_key *key, unsigned int slot)
{
	struct gpt_view *v = container_of(profile, struct gpt_view, crypto);
	unsigned int i;

	for (i = 0; i < v->count; i++)
		blk_crypto_evict_key(v->maps[i].lower, key);
	return 0;
}
static int setup_crypto(struct gpt_view *v)
{
	unsigned int i;
	int err = blk_crypto_profile_init(&v->crypto, 0);
	if (err)
		return err;
	v->crypto_ready = true;
	v->crypto.ll_ops.keyslot_evict = evict_key;
	v->crypto.ll_ops.derive_sw_secret = derive_sw_secret;
	v->crypto.max_dun_bytes_supported = UINT_MAX;
	memset(v->crypto.modes_supported, 0xff,
	       sizeof(v->crypto.modes_supported));
	v->crypto.key_types_supported = ~0;
	for (i = 0; i < v->count; i++)
		blk_crypto_intersect_capabilities(
			&v->crypto, v->maps[i].lower->bd_queue->crypto_profile);
	/* A slotless stacking profile keeps crypto attached until the lower
	 * queue, instead of prematurely applying software fallback here. */
	v->disk->queue->crypto_profile = &v->crypto;
	return 0;
}
#else
static int setup_crypto(struct gpt_view *v)
{
	return 0;
}
#endif

static void destroy_view(struct gpt_view *v)
{
	unsigned int i;
	if (!v)
		return;
	/* The seal was applied last, so it is restored first, while every bdev
	 * that referenced the sealed disks is still held below. */
	unseal_view(v);
	/* Restore physical PARTNAME endpoints before withdrawing replacements. */
	for (i = v->hide_count; i > 0; i--) {
		struct hidden_part *h = &v->hidden[i - 1];
		if (h->changed) {
			mutex_lock(&h->bdev->bd_disk->open_mutex);
			if (h->bdev->bd_meta_info)
				memcpy(h->bdev->bd_meta_info->volname,
				       h->volname, sizeof(h->volname));
			mutex_unlock(&h->bdev->bd_disk->open_mutex);
		}
	}
	if (v->added)
		del_gendisk(v->disk);
	if (atomic_dec_and_test(&v->inflight))
		complete(&v->drained);
	wait_for_completion(&v->drained);
	if (v->disk)
		put_disk(v->disk);
#ifdef CONFIG_BLK_INLINE_ENCRYPTION
	if (v->crypto_ready)
		blk_crypto_profile_destroy(&v->crypto);
#endif
	if (v->pool_ready)
		bioset_exit(&v->clones);
	for (i = v->hide_count; i > 0; i--)
		if (v->hidden[i - 1].file)
			fput(v->hidden[i - 1].file);
	for (i = v->count; i > 0; i--) {
		if (v->maps[i - 1].whole)
			fput(v->maps[i - 1].whole);
		if (v->maps[i - 1].backend)
			fput(v->maps[i - 1].backend);
	}
	kfree(v->backup);
	kfree(v->primary);
	kfree(v);
}

static int checked_dev(u32 major, u32 minor, dev_t *dev)
{
	*dev = MKDEV(major, minor);
	return MAJOR(*dev) == major && MINOR(*dev) == minor && *dev ? 0 :
								      -EINVAL;
}
static int validate_apply(const struct gpt_apply *a)
{
	unsigned int i, j;
	dev_t dev;
	if (a->version != GPT_ABI_VERSION || !a->count ||
	    a->count > GPT_MAX_PROJECTIONS || a->hide_count > GPT_MAX_HIDDEN ||
	    (a->flags & ~GPT_APPLY_FLAG_SEAL))
		return -EINVAL;
	for (i = 0; i < a->count; i++) {
		const struct gpt_projection *p = &a->projections[i];
		size_t len = strnlen(p->name, sizeof(p->name));
		if (checked_dev(p->major, p->minor, &dev) || p->read_only > 1 ||
		    memchr_inv(p->reserved, 0, sizeof(p->reserved)) ||
		    memchr_inv(p->reserved2, 0, sizeof(p->reserved2)) || !len ||
		    len > GPT_LABEL_BYTES ||
		    memchr_inv(p->name + len, 0, sizeof(p->name) - len))
			return -EINVAL;
		for (j = 0; j < len; j++)
			if (p->name[j] < 0x20 || p->name[j] > 0x7e)
				return -EINVAL;
		for (j = 0; j < i; j++)
			if (!strcmp(p->name, a->projections[j].name) ||
			    (p->major == a->projections[j].major &&
			     p->minor == a->projections[j].minor))
				return -EINVAL;
	}
	if (memchr_inv(a->projections + a->count, 0,
		       (GPT_MAX_PROJECTIONS - a->count) *
			       sizeof(a->projections[0])) ||
	    memchr_inv(a->hide + a->hide_count, 0,
		       (GPT_MAX_HIDDEN - a->hide_count) * sizeof(a->hide[0])))
		return -EINVAL;
	for (i = 0; i < a->hide_count; i++) {
		if (checked_dev(a->hide[i].major, a->hide[i].minor, &dev))
			return -EINVAL;
		for (j = 0; j < i; j++)
			if (a->hide[i].major == a->hide[j].major &&
			    a->hide[i].minor == a->hide[j].minor)
				return -EINVAL;
	}
	return 0;
}

/* Seal the physical storage this managed view must not write.
 *
 * The candidate set is exactly the storage the view already references: every
 * projection backend (a partition backend contributes its whole disk) and every
 * hidden physical partition. Enumeration stays inside those references because
 * the phone kernel does not export the block class this module would need to
 * walk every gendisk; a managed ROM must reference every physical partition it
 * hides, so the referenced set covers every logical unit it opens.
 *
 * A candidate disk that is not the lower disk of a writable projection becomes
 * read-only at the gendisk (GD_READ_ONLY covers the disk and all its
 * partitions). A candidate disk that stays writable keeps its partitions
 * writable except for those that are neither a writable backend nor owned by
 * another subsystem: a partition with an exclusive holder is skipped, which is
 * what keeps the LVM physical volume (held by dm) usable under the thin pool.
 * Every applied change is recorded so teardown restores exactly it.
 *
 * GD_READ_ONLY is not clearable by BLKROSET, so a sealed disk cannot be opened
 * for writing by an internal caller again; BD_READ_ONLY on a partition is
 * clearable by root with BLKROSET and is therefore only a friction layer. This
 * is not a firewall: bio_check_ro only warns, so a client that opened a device
 * before the seal keeps writing through that open. */
static int seal_view(struct gpt_view *v)
{
	struct gendisk **disks;
	unsigned int disk_count = 0, seek, i;
	int err = 0;

	if (!v->seal)
		return 0;
	disks = kcalloc(v->count + v->hide_count, sizeof(*disks), GFP_KERNEL);
	if (!disks)
		return -ENOMEM;
	for (i = 0; i < v->count; i++) {
		struct gendisk *disk = v->maps[i].lower->bd_disk;

		for (seek = 0; seek < disk_count; seek++)
			if (disks[seek] == disk)
				break;
		if (seek == disk_count)
			disks[disk_count++] = disk;
	}
	for (i = 0; i < v->hide_count; i++) {
		struct gendisk *disk = v->hidden[i].bdev->bd_disk;

		for (seek = 0; seek < disk_count; seek++)
			if (disks[seek] == disk)
				break;
		if (seek == disk_count)
			disks[disk_count++] = disk;
	}
	for (i = 0; i < disk_count; i++) {
		struct gendisk *disk = disks[i];

		if (!disk_is_writable_backend(v, disk)) {
			err = seal_record_disk(v, disk);
			if (err)
				goto out;
			set_disk_ro(disk, true);
			continue;
		}
		mutex_lock(&disk->open_mutex);
		{
			unsigned long index;
			struct block_device *bdev;

			xa_for_each(&disk->part_tbl, index, bdev) {
				if (!index || bdev->bd_holder ||
				    part_is_writable_backend(v, bdev) ||
				    bdev_read_only(bdev))
					continue;
				err = seal_record_part(v, bdev);
				if (err)
					break;
				bdev_set_flag(bdev, BD_READ_ONLY);
			}
		}
		mutex_unlock(&disk->open_mutex);
		if (err)
			goto out;
	}
	WRITE_ONCE(sealed, true);
out:
	kfree(disks);
	return err;
}

static bool disk_is_writable_backend(const struct gpt_view *v,
				     const struct gendisk *disk)
{
	unsigned int i;

	for (i = 0; i < v->count; i++)
		if (!v->maps[i].ro && v->maps[i].lower->bd_disk == disk)
			return true;
	return false;
}

static bool part_is_writable_backend(const struct gpt_view *v,
				     const struct block_device *bdev)
{
	unsigned int i;

	for (i = 0; i < v->count; i++)
		if (!v->maps[i].ro && file_bdev(v->maps[i].backend) == bdev)
			return true;
	return false;
}

static int seal_record_disk(struct gpt_view *v, struct gendisk *disk)
{
	if (v->sealed_disk_count == v->sealed_disk_cap) {
		unsigned int next = v->sealed_disk_cap ?
					   v->sealed_disk_cap * 2 :
					   8;
		struct gendisk **grown =
			krealloc(v->sealed_disks, next * sizeof(*grown),
				 GFP_KERNEL);
		if (!grown)
			return -ENOMEM;
		v->sealed_disks = grown;
		v->sealed_disk_cap = next;
	}
	v->sealed_disks[v->sealed_disk_count++] = disk;
	return 0;
}

static int seal_record_part(struct gpt_view *v, struct block_device *bdev)
{
	if (v->sealed_part_count == v->sealed_part_cap) {
		unsigned int next = v->sealed_part_cap ?
					   v->sealed_part_cap * 2 :
					   8;
		struct block_device **grown =
			krealloc(v->sealed_parts, next * sizeof(*grown),
				 GFP_KERNEL);
		if (!grown)
			return -ENOMEM;
		v->sealed_parts = grown;
		v->sealed_part_cap = next;
	}
	v->sealed_parts[v->sealed_part_count++] = bdev;
	return 0;
}

/* Undo the seal in reverse order: the per-partition flags first, then the whole
 * disks they belong to. */
static void unseal_view(struct gpt_view *v)
{
	unsigned int i;

	for (i = v->sealed_part_count; i > 0; i--)
		bdev_clear_flag(v->sealed_parts[i - 1], BD_READ_ONLY);
	for (i = v->sealed_disk_count; i > 0; i--)
		set_disk_ro(v->sealed_disks[i - 1], false);
	kfree(v->sealed_parts);
	kfree(v->sealed_disks);
	v->sealed_parts = NULL;
	v->sealed_disks = NULL;
	v->sealed_part_count = v->sealed_part_cap = 0;
	v->sealed_disk_count = v->sealed_disk_cap = 0;
	if (v->seal)
		WRITE_ONCE(sealed, false);
}

static int apply_view(const struct gpt_apply *a)
{
	struct gpt_view *v;
	struct device *parent = NULL;
	struct queue_limits limits;
	sector_t end = GPT_ALIGNMENT;
	unsigned int i;
	int err = validate_apply(a);
	if (err)
		return err;
	blk_set_stacking_limits(&limits);
	v = kzalloc(sizeof(*v), GFP_KERNEL);
	if (!v)
		return -ENOMEM;
	init_completion(&v->drained);
	atomic_set(&v->inflight, 1);
	v->count = a->count;
	v->hide_count = a->hide_count;
	v->seal = !!(a->flags & GPT_APPLY_FLAG_SEAL);
	for (i = 0; i < v->count; i++) {
		const struct gpt_projection *p = &a->projections[i];
		struct view_map *m = &v->maps[i];
		struct block_device *b;
		dev_t dev;
		checked_dev(p->major, p->minor, &dev);
		m->ro = p->read_only;
		m->backend = bdev_file_open_by_dev(
			dev, BLK_OPEN_READ | (m->ro ? 0 : BLK_OPEN_WRITE), NULL,
			NULL);
		if (IS_ERR(m->backend)) {
			err = PTR_ERR(m->backend);
			m->backend = NULL;
			goto fail;
		}
		b = file_bdev(m->backend);
		m->lower = b;
		m->length = bdev_nr_sectors(b);
		m->offset = b->bd_start_sect;
		if (!m->length || (!m->ro && bdev_read_only(b))) {
			err = -EINVAL;
			goto fail;
		}
		if (bdev_is_partition(b)) {
			m->whole = bdev_file_open_by_dev(
				bdev_whole(b)->bd_dev,
				BLK_OPEN_READ | (m->ro ? 0 : BLK_OPEN_WRITE),
				NULL, NULL);
			if (IS_ERR(m->whole)) {
				err = PTR_ERR(m->whole);
				m->whole = NULL;
				goto fail;
			}
			m->lower = file_bdev(m->whole);
			if (!parent)
				parent = disk_to_dev(b->bd_disk);
		}
		if (m->offset > bdev_nr_sectors(m->lower) ||
		    m->length > bdev_nr_sectors(m->lower) - m->offset) {
			err = -EINVAL;
			goto fail;
		}
		queue_limits_stack_bdev(&limits, m->lower, m->offset,
					"espinit-gpt");
	}
	v->block_size = limits.logical_block_size;
	if (v->block_size > PAGE_SIZE || v->block_size > 4096) {
		err = -EINVAL;
		goto fail;
	}
	for (i = 0; i < v->count; i++) {
		struct view_map *m = &v->maps[i];
		if ((m->length | m->offset) & ((v->block_size >> 9) - 1)) {
			err = -EINVAL;
			goto fail;
		}
		m->start = end;
		if (check_add_overflow(end, m->length, &end) ||
		    check_add_overflow(end, (sector_t)GPT_ALIGNMENT - 1,
				       &end)) {
			err = -EOVERFLOW;
			goto fail;
		}
		end &= ~((sector_t)GPT_ALIGNMENT - 1);
	}
	if (check_add_overflow(end,
			       (sector_t)(GPT_ENTRIES * GPT_ENTRY_SIZE / 512 +
					  (v->block_size >> 9)),
			       &v->sectors) ||
	    v->sectors > (S64_MAX >> 9)) {
		err = -EOVERFLOW;
		goto fail;
	}
	for (i = 0; i < v->hide_count; i++) {
		struct hidden_part *h = &v->hidden[i];
		dev_t dev;
		checked_dev(a->hide[i].major, a->hide[i].minor, &dev);
		h->file = bdev_file_open_by_dev(dev, BLK_OPEN_READ, NULL, NULL);
		if (IS_ERR(h->file)) {
			err = PTR_ERR(h->file);
			h->file = NULL;
			goto fail;
		}
		h->bdev = file_bdev(h->file);
		if (!bdev_is_partition(h->bdev)) {
			err = -EINVAL;
			goto fail;
		}
		if (!parent)
			parent = disk_to_dev(h->bdev->bd_disk);
	}
	err = make_metadata(v, a);
	if (err)
		goto fail;
	err = bioset_init(&v->clones, 64, 0,
			  BIOSET_NEED_BVECS | BIOSET_NEED_RESCUER);
	if (err)
		goto fail;
	v->pool_ready = true;
	v->disk = blk_alloc_disk(&limits, NUMA_NO_NODE);
	if (IS_ERR(v->disk)) {
		err = PTR_ERR(v->disk);
		v->disk = NULL;
		goto fail;
	}
	v->disk->fops = &view_ops;
	v->disk->private_data = v;
	strscpy(v->disk->disk_name, "espinit-gpt", DISK_NAME_LEN);
	set_capacity(v->disk, v->sectors);
	err = setup_crypto(v);
	if (err)
		goto fail;
	/* add_disk's initial partition scan consumes our in-memory GPT. No
	 * post-publication rescan or non-exported partition helper is needed. */
	err = device_add_disk(parent, v->disk, NULL);
	if (err)
		goto fail;
	v->added = true;
	mutex_lock(&v->disk->open_mutex);
	for (i = 0; i < v->count; i++) {
		struct block_device *b = xa_load(&v->disk->part_tbl, i + 1);
		if (!b || b->bd_start_sect != v->maps[i].start ||
		    bdev_nr_sectors(b) != v->maps[i].length ||
		    !b->bd_meta_info ||
		    strcmp((const char *)b->bd_meta_info->volname,
			   a->projections[i].name)) {
			err = -EIO;
			break;
		}
		if (v->maps[i].ro)
			bdev_set_flag(b, BD_READ_ONLY);
	}
	mutex_unlock(&v->disk->open_mutex);
	if (err)
		goto fail;
	/* Hide physical PARTNAME endpoints, not raw parent logical units. Hidden
	 * partitions remain writable because a projected DM/LVM backend can resolve
	 * through one of them; BD_READ_ONLY would reject projected writes too. This
	 * is namespace isolation, not a storage firewall. */
	for (i = 0; i < v->hide_count; i++) {
		struct hidden_part *h = &v->hidden[i];
		mutex_lock(&h->bdev->bd_disk->open_mutex);
		if (h->bdev->bd_meta_info) {
			memcpy(h->volname, h->bdev->bd_meta_info->volname,
			       sizeof(h->volname));
			memset(h->bdev->bd_meta_info->volname, 0,
			       sizeof(h->volname));
		}
		h->changed = true;
		mutex_unlock(&h->bdev->bd_disk->open_mutex);
	}
	/* The seal runs last, when the whole view is live and every PARTNAME is
	 * hidden, and fails the whole APPLY if it cannot be applied completely. */
	err = seal_view(v);
	if (err)
		goto fail;
	active = v;
	applied = true;
	WRITE_ONCE(ready, true);
	return 0;
fail:
	destroy_view(v);
	return err;
}

static long control_ioctl(struct file *file, unsigned int cmd,
			  unsigned long arg)
{
	void __user *user = (void __user *)arg;
	struct gpt_apply *a;
	struct gpt_query q;
	long err;
	if (!capable(CAP_SYS_ADMIN))
		return -EPERM;
	if (cmd == GPT_IOCTL_QUERY) {
		mutex_lock(&control_lock);
		q = (struct gpt_query){ .version = GPT_ABI_VERSION,
					.active = !!active,
					.count = active ? active->count : 0 };
		mutex_unlock(&control_lock);
		return copy_to_user(user, &q, sizeof(q)) ? -EFAULT : 0;
	}
	if (cmd != GPT_IOCTL_APPLY)
		return -ENOTTY;
	a = memdup_user(user, sizeof(*a));
	if (IS_ERR(a))
		return PTR_ERR(a);
	mutex_lock(&control_lock);
	err = applied ? -EBUSY : apply_view(a);
	mutex_unlock(&control_lock);
	kfree(a);
	return err;
}
#ifdef CONFIG_COMPAT
static long control_compat_ioctl(struct file *file, unsigned int cmd,
				 unsigned long arg)
{
	return control_ioctl(file, cmd, (unsigned long)compat_ptr(arg));
}
#endif
static const struct file_operations control_ops = {
	.owner = THIS_MODULE,
	.unlocked_ioctl = control_ioctl,
#ifdef CONFIG_COMPAT
	.compat_ioctl = control_compat_ioctl,
#endif
};
static struct miscdevice control = {
	.minor = MISC_DYNAMIC_MINOR,
	.name = "gptctl",
	.fops = &control_ops,
	.mode = 0600,
};
static int __init gpt_init(void)
{
	/* ready and sealed are output-only, including when a caller passes them. */
	ready = false;
	sealed = false;
	/* Parameters are read-only after load; also reject load-time spoofing. */
	if (strcmp(generation, ESPINIT_GENERATION))
		return -EINVAL;
	return misc_register(&control);
}
static void __exit gpt_exit(void)
{
	misc_deregister(&control);
	mutex_lock(&control_lock);
	WRITE_ONCE(ready, false);
	destroy_view(active);
	active = NULL;
	mutex_unlock(&control_lock);
}
module_init(gpt_init);
module_exit(gpt_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("espinit in-memory GPT projection, ABI v2");
