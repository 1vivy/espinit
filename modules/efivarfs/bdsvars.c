// SPDX-License-Identifier: GPL-2.0-only
/* Block-backed edk2 authenticated variable store. No runtime firmware calls. */
#include <linux/blkdev.h>
#include <linux/efi.h>
#include <linux/err.h>
#include <linux/file.h>
#include <linux/fs.h>
#include <linux/init.h>
#include <linux/kdev_t.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/slab.h>
#include <linux/unaligned.h>
#include <linux/vmalloc.h>

#define HEADER 60
#define LIMIT (8 * 1024 * 1024)
static char *dev;
module_param(dev, charp, 0400);
MODULE_PARM_DESC(dev, "required bdsvars block device major:minor");
static DEFINE_MUTEX(store_lock);
static struct file *backing;
static u8 *image, *verify_buf;
static size_t image_size, first, end, append;
static size_t dirty_start = LIMIT, dirty_end;
static bool ready;
static struct efivars esu_efivars;
int esu_fs_init(void);
void esu_fs_exit(void);

struct record { size_t pos, next; u32 ns, ds, attr; u8 state; };
static size_t aligned(size_t n) { return (n + 3) & ~(size_t)3; }
static bool erased(size_t a, size_t b)
{
	while (a < b) if (image[a++] != 0xff) return false;
	return true;
}
static bool valid_name(const u8 *s, size_t bytes)
{
	size_t i;
	if (bytes < 4 || bytes & 1 || get_unaligned_le16(s + bytes - 2)) return false;
	for (i = 0; i < bytes - 2; i += 2) {
		u16 c = get_unaligned_le16(s + i);
		if (!c || (c >= 0xdc00 && c <= 0xdfff)) return false;
		if (c >= 0xd800 && c <= 0xdbff) {
			if (i + 4 >= bytes) return false;
			i += 2; c = get_unaligned_le16(s + i);
			if (c < 0xdc00 || c > 0xdfff) return false;
		}
	}
	return true;
}
static int record_at(size_t p, struct record *r)
{
	size_t n;
	if (end - p < HEADER || get_unaligned_le16(image + p) != 0x55aa) return -EINVAL;
	r->pos = p; r->state = image[p + 2];
	if (r->state != 0xff && r->state != 0x7f && r->state != 0x3f &&
	    r->state != 0x3e && r->state != 0x3d && r->state != 0x3c) return -EINVAL;
	r->ns = get_unaligned_le32(image + p + 36);
	r->ds = get_unaligned_le32(image + p + 40);
	r->attr = get_unaligned_le32(image + p + 4);
	if (r->ns < 4 || r->ns & 1 || r->ns > end - p - HEADER ||
	    r->ds > end - p - HEADER - r->ns) return -EINVAL;
	n = aligned(HEADER + (size_t)r->ns + r->ds);
	if (n > end - p) return -EINVAL;
	r->next = p + n;
	if (r->state != 0xff && r->state != 0x7f && !valid_name(image + p + HEADER, r->ns)) return -EINVAL;
	return 0;
}
static int parse(void)
{
	static const u8 nv[] = {0x8d,0x2b,0xf1,0xff,0x96,0x76,0x8b,0x4c,0xa9,0x85,0x27,0x47,7,0x5b,0x4f,0x50};
	static const u8 auth[] = {0x78,0x2c,0xf3,0xaa,0x7b,0x94,0x9a,0x43,0xa1,0x80,0x2e,0x14,0x4e,0xc3,0x77,0x92};
	size_t h, p, map_end, ext; u32 sz; u16 sum = 0; u64 total = 0;
	struct record r;
	if (image_size < 100 || memcmp(image + 40, "_FVH", 4) || memcmp(image + 16, nv, 16) ||
	    get_unaligned_le64(image + 32) != image_size) return -EINVAL;
	h = get_unaligned_le16(image + 48);
	if (h < 72 || h & 3 || h > image_size - 28 || image[54] || image[55] != 2 ||
	    !(get_unaligned_le32(image + 44) & 0x800)) return -EINVAL;
	for (p = 0; p < h; p += 2) sum += get_unaligned_le16(image + p);
	if (sum) return -EINVAL;
	ext = get_unaligned_le16(image + 52); map_end = ext ? ext : h;
	if (map_end > h) return -EINVAL;
	for (p = 56; ; p += 8) {
		u32 count, block;
		if (p + 8 > map_end) return -EINVAL;
		count = get_unaligned_le32(image + p); block = get_unaligned_le32(image + p + 4);
		if (!count && !block) break;
		if (!count || !block || (u64)count * block > image_size - total) return -EINVAL;
		total += (u64)count * block;
	}
	if (total != image_size) return -EINVAL;
	if (ext && (ext < p + 8 || h - ext < 20 || get_unaligned_le32(image + ext + 16) < 20 ||
	    get_unaligned_le32(image + ext + 16) > h - ext)) return -EINVAL;
	sz = get_unaligned_le32(image + h + 16);
	if (memcmp(image + h, auth, 16) || sz < 28 || sz & 3 || sz > image_size - h ||
	    image[h + 20] != 0x5a || image[h + 21] != 0xfe) return -EINVAL;
	first = h + 28; end = h + sz; append = first;
	while (append < end && !erased(append, end)) {
		if (record_at(append, &r)) return -EINVAL;
		append = r.next;
	}
	return 0;
}
static int read_image(void)
{
	loff_t off = 0;
	ready = false;
	dirty_start = LIMIT; dirty_end = 0;
	if (kernel_read(backing, image, image_size, &off) != (ssize_t)image_size) return -EIO;
	if (parse()) return -EINVAL;
	ready = true; return 0;
}
static bool live(const struct record *r) { return r->state == 0x3f || r->state == 0x3e; }
static bool key(const struct record *r, efi_char16_t *name, efi_guid_t *guid, size_t ns)
{
	return r->ns == ns && !memcmp(image + r->pos + 44, guid, 16) &&
	       !memcmp(image + r->pos + HEADER, name, ns);
}
static size_t name_size(efi_char16_t *name)
{
	size_t n;
	for (n = 0; n < LIMIT / 2; n++) if (!name[n]) return (n + 1) * 2;
	return 0;
}
static size_t resolve(efi_char16_t *name, efi_guid_t *guid, size_t ns)
{
	size_t p, found = end; struct record r;
	for (p = first; p < append; p = r.next) {
		record_at(p, &r);
		if (!live(&r) || !key(&r, name, guid, ns)) continue;
		if (r.state == 0x3f) return p;
		found = p;
	}
	return found;
}
static efi_status_t esu_get(efi_char16_t *name, efi_guid_t *guid, u32 *attr,
			  unsigned long *size, void *data)
{
	efi_status_t status = EFI_NOT_FOUND; size_t p; struct record r;
	mutex_lock(&store_lock);
	if (!ready) { status = EFI_DEVICE_ERROR; goto out; }
	p = resolve(name, guid, name_size(name));
	if (p == end) goto out;
	record_at(p, &r);
	if (attr) *attr = r.attr;
	if (*size < r.ds) { *size = r.ds; status = EFI_BUFFER_TOO_SMALL; goto out; }
	*size = r.ds;
	if (r.ds && !data) { status = EFI_INVALID_PARAMETER; goto out; }
	memcpy(data, image + p + HEADER + r.ns, r.ds); status = EFI_SUCCESS;
out:
	mutex_unlock(&store_lock); return status;
}
static efi_status_t esu_next(unsigned long *size, efi_char16_t *name, efi_guid_t *guid)
{
	efi_status_t status = EFI_NOT_FOUND; size_t p, previous; struct record r;
	mutex_lock(&store_lock);
	if (!ready) { status = EFI_DEVICE_ERROR; goto out; }
	previous = name[0] ? resolve(name, guid, name_size(name)) : end;
	if (name[0] && previous == end) goto out;
	p = first;
	if (previous != end) {
		record_at(previous, &r);
		p = r.next;
	}
	for (; p < append; p = r.next) {
		record_at(p, &r);
		if (!live(&r) || resolve((efi_char16_t *)(image + p + HEADER), (efi_guid_t *)(image + p + 44), r.ns) != p) continue;
		if (*size < r.ns) { *size = r.ns; status = EFI_BUFFER_TOO_SMALL; goto out; }
		*size = r.ns; memcpy(name, image + p + HEADER, r.ns); memcpy(guid, image + p + 44, 16);
		status = EFI_SUCCESS; break;
	}
out:
	mutex_unlock(&store_lock); return status;
}
/* Verify each phase's touched span, then the whole image after the commit. */
static int write_range(size_t p, size_t n)
{
	loff_t off = p;
	if (p < dirty_start) dirty_start = p;
	if (p + n > dirty_end) dirty_end = p + n;
	return kernel_write(backing, image + p, n, &off) == (ssize_t)n ? 0 : -EIO;
}
static int verify_range(size_t p, size_t limit)
{
	while (p < limit) {
		size_t n = min_t(size_t, 4096, limit - p); loff_t off = p;
		if (kernel_read(backing, verify_buf, n, &off) != (ssize_t)n || memcmp(verify_buf, image + p, n)) return -EIO;
		p += n;
	}
	return 0;
}
static int flush(void)
{
	if (vfs_fsync(backing, 0) || verify_range(dirty_start, dirty_end)) return -EIO;
	dirty_start = LIMIT; dirty_end = 0;
	return 0;
}
static int retire(efi_char16_t *name, efi_guid_t *guid, size_t ns, size_t limit, u8 mask)
{
	size_t p; struct record r;
	for (p = first; p < limit; p = r.next) {
		record_at(p, &r);
		if (live(&r) && key(&r, name, guid, ns)) {
			image[p + 2] &= mask;
			if (write_range(p + 2, 1)) return -EIO;
		}
	}
	return flush();
}
static efi_status_t esu_set(efi_char16_t *name, efi_guid_t *guid, u32 attr, unsigned long size, void *data)
{
	efi_status_t status = EFI_INVALID_PARAMETER; size_t ns, p, length, old; struct record r;
	bool deleting = !size;
	mutex_lock(&store_lock);
	if (!ready) { status = EFI_DEVICE_ERROR; goto out; }
	ns = name_size(name);
	if (!ns || !valid_name((u8 *)name, ns) || (attr != 7 && !(deleting && !attr)) || (size && !data)) goto out;
	for (p = first; p < append; p = r.next) {
		record_at(p, &r);
		if (live(&r) && key(&r, name, guid, ns) && (r.attr & 0xb0)) goto out;
	}
	old = resolve(name, guid, ns);
	if (deleting) {
		if (old == end) { status = EFI_SUCCESS; goto out; }
		if (retire(name, guid, ns, append, 0xfd) || verify_range(0, image_size)) goto io_error;
		status = EFI_SUCCESS; goto out;
	}
	if (old != end) {
		record_at(old, &r);
		if (r.attr == attr && r.ds == size && !memcmp(image + old + HEADER + ns, data, size)) { status = EFI_SUCCESS; goto out; }
	}
	if (ns > end - append || size > end - append || HEADER + ns + size > end - append) { status = EFI_OUT_OF_RESOURCES; goto out; }
	length = aligned(HEADER + ns + size);
	if (length > end - append) { status = EFI_OUT_OF_RESOURCES; goto out; }
	p = append;
	if (retire(name, guid, ns, p, 0xfe)) goto io_error;
	put_unaligned_le16(0x55aa, image + p); image[p + 3] = 0;
	put_unaligned_le32(attr, image + p + 4); memset(image + p + 8, 0, 28);
	put_unaligned_le32(ns, image + p + 36); put_unaligned_le32(size, image + p + 40);
	memcpy(image + p + 44, guid, 16);
	if (write_range(p, HEADER) || flush()) goto io_error;
	image[p + 2] = 0x7f;
	if (write_range(p + 2, 1) || flush()) goto io_error;
	memcpy(image + p + HEADER, name, ns); memcpy(image + p + HEADER + ns, data, size);
	if (write_range(p + HEADER, length - HEADER) || flush()) goto io_error;
	image[p + 2] = 0x3f;
	if (write_range(p + 2, 1) || flush()) goto io_error;
	if (retire(name, guid, ns, p, 0xfd) || verify_range(0, image_size)) goto io_error;
	append += length; status = EFI_SUCCESS; goto out;
io_error:
	read_image(); status = EFI_DEVICE_ERROR;
out:
	mutex_unlock(&store_lock); return status;
}
static efi_status_t esu_query(u32 attr, u64 *capacity, u64 *remaining, u64 *maximum)
{
	efi_status_t status = EFI_SUCCESS;
	mutex_lock(&store_lock);
	if (!ready) status = EFI_DEVICE_ERROR;
	else if (attr != 7) status = EFI_INVALID_PARAMETER;
	else { *capacity = end - first; *remaining = end - append; *maximum = end - first > HEADER + 4 ? end - first - HEADER - 4 : 0; }
	mutex_unlock(&store_lock); return status;
}
static const struct efivar_operations esu_ops = {
	.get_variable = esu_get, .get_next_variable = esu_next, .set_variable = esu_set,
	.set_variable_nonblocking = NULL, .query_variable_info = esu_query,
};
static int __init backend_init(void)
{
	unsigned int major, minor; char extra; u64 length; loff_t off = 0; int ret;
	if (!dev || sscanf(dev, "%u:%u%c", &major, &minor, &extra) != 2 ||
	    MAJOR(MKDEV(major, minor)) != major || MINOR(MKDEV(major, minor)) != minor) return -EINVAL;
	backing = bdev_file_open_by_dev(MKDEV(major, minor), BLK_OPEN_READ | BLK_OPEN_WRITE, NULL, NULL);
	if (IS_ERR(backing)) return PTR_ERR(backing);
	verify_buf = kmalloc(4096, GFP_KERNEL);
	if (!verify_buf) { ret = -ENOMEM; goto close; }
	ret = -EINVAL;
	if (kernel_read(backing, verify_buf, 4096, &off) != 4096) goto free;
	length = get_unaligned_le64(verify_buf + 32);
	if (length < 72 || length > LIMIT || length % 4096 || length > bdev_nr_bytes(file_bdev(backing))) goto free;
	image_size = length; image = kvmalloc(image_size, GFP_KERNEL);
	if (!image) { ret = -ENOMEM; goto free; }
	ret = read_image();
	if (!ret) ret = efivars_register(&esu_efivars, &esu_ops);
	if (!ret) return 0;
	kvfree(image); image = NULL; ready = false;
free:
	kfree(verify_buf); verify_buf = NULL;
close:
	fput(backing); backing = NULL; return ret;
}
static void __exit backend_exit(void)
{
	efivars_unregister(&esu_efivars); ready = false;
	kvfree(image); kfree(verify_buf); fput(backing);
}
static int __init esu_init(void)
{
	int ret = esu_fs_init();
	if (ret) return ret;
	ret = backend_init();
	if (ret) esu_fs_exit();
	return ret;
}
static void __exit esu_exit(void) { backend_exit(); esu_fs_exit(); }
module_init(esu_init);
module_exit(esu_exit);
