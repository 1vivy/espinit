// SPDX-License-Identifier: GPL-3.0-only
/*
 * thin-activate - espinit Cuttlefish thin-pool activator.
 *
 * This is the Cuttlefish integration lane's boot helper and nothing more: it
 * defines no general LVM abstraction, never runs a shell, never calls dmsetup
 * and never interprets a caller-supplied string as code. The ESP script
 * `modules/thin/early.sh` only executes this binary from PATH, and every value
 * comes from the tuple
 *
 *   androidboot.espinit.thin=<PARTUUID>:<metadata-sectors>:<data-sectors>:<thin-id>:<volume-sectors>
 *
 * read from /proc/cmdline or /proc/bootconfig. The backing GPT partition is
 * resolved by its exact PARTUUID from /sys/class/block, and the whole stack is
 * built through the device-mapper ioctl ABI:
 *
 *   userdata_thin_meta   linear    <part>                       0 .. metadata
 *   userdata_thin_data   linear    <part>   metadata  .. metadata + data
 *   userdata_thin_pool   thin-pool <meta> <data> 128 128 1 skip_block_zeroing
 *   userdata_lp          thin      <pool> <thin-id>             volume sectors
 *
 * `skip_block_zeroing` is mandatory: the forked dm-thin in modules/thin refuses
 * to create a pool that would zero newly provisioned blocks, because projected
 * userdata carries encrypted content. `userdata_lp` is the device-mapper name
 * the espinit `gpt` backend resolver looks up in /sys/class/block/dm-N/dm/name.
 *
 * An existing thin id is not an error: dm-thin reopens the internal device, so
 * the helper is idempotent across a reboot of an unchanged pool. Any other
 * pre-existing target is accepted only when its type, length and parameter
 * string match this exact stack; anything else fails the boot instead of being
 * trusted.
 *
 * Every failure prints one concrete reason on stderr and exits non-zero. Every
 * device this invocation created is removed again before it exits, so a failed
 * boot never leaves a half-built mapping for the next one.
 */

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

/*
 * Last on purpose: with glibc headers the UAPI header's fixed-width types are
 * only visible after the C library's own type headers, and <sys/types.h> above
 * provides them. The NDK sysroot needs no such ordering but is unaffected.
 */
#include <linux/dm-ioctl.h>

/*
 * The device-mapper ioctl ABI is frozen: the kernel only ever appends
 * commands. Fail at compile time rather than at boot if the toolchain's header
 * ever disagrees with the layout this helper was written against.
 */
_Static_assert(sizeof(struct dm_ioctl) == 312, "dm_ioctl layout");
_Static_assert(offsetof(struct dm_ioctl, name) == 48, "dm_ioctl name offset");
_Static_assert(offsetof(struct dm_ioctl, uuid) == 176, "dm_ioctl uuid offset");
_Static_assert(sizeof(struct dm_target_spec) == 40, "dm_target_spec layout");
_Static_assert(sizeof(struct dm_target_versions) == 16,
	       "dm_target_versions layout");
_Static_assert(sizeof(struct dm_target_msg) == 8, "dm_target_msg layout");

#define DM_BUF_SIZE (16 * 1024)
#define DM_IFACE_MAJOR 4
#define DM_IFACE_MINOR 0
#define DM_IFACE_PATCH 0

#define KEY_TUPLE "androidboot.espinit.thin"

#define NAME_META "userdata_thin_meta"
#define NAME_DATA "userdata_thin_data"
#define NAME_POOL "userdata_thin_pool"
#define NAME_OUTPUT "userdata_lp"

/* Proven pool geometry: 64 KiB data blocks, low water mark at 128 blocks. */
#define DATA_BLOCK_SECTORS 128
#define LOW_WATER_BLOCKS 128

/* dm-thin rejects a device id above MAX_DEV_ID (modules/thin/dm-thin.c). */
#define MAX_DEV_ID ((uint64_t)((1U << 24) - 1))

#define TEXT_LIMIT 512
#define NAME_LIMIT 64
#define PATH_LIMIT 512
#define ENUM_TIMEOUT_MS 10000U
#define ENUM_INTERVAL_MS 50U

static const char *const device_names[] = {
	NAME_META,
	NAME_DATA,
	NAME_POOL,
	NAME_OUTPUT,
};

enum {
	DEVICE_META,
	DEVICE_DATA,
	DEVICE_POOL,
	DEVICE_OUTPUT,
	DEVICE_COUNT,
};

struct dmdev {
	unsigned major;
	unsigned minor;
	uint64_t sectors;
};

struct table {
	char type[DM_MAX_TYPE_NAME];
	char params[TEXT_LIMIT];
	uint64_t length;
};

struct dm {
	int control;
	char *buffer;
	size_t size;
};

struct tuple {
	char partuuid[37];
	uint64_t metadata_sectors;
	uint64_t data_sectors;
	uint64_t thin_id;
	uint64_t volume_sectors;
};

struct partition {
	char devname[NAME_LIMIT];
	unsigned major;
	unsigned minor;
	uint64_t sectors;
};

/* One bounded diagnostic, reported once by main(). */
static char error_text[TEXT_LIMIT];

static bool device_created[DEVICE_COUNT];
static struct dmdev device_geometry[DEVICE_COUNT];

static int failf(const char *format, ...)
{
	va_list args;

	va_start(args, format);
	vsnprintf(error_text, sizeof(error_text), format, args);
	va_end(args);

	return -1;
}

static void sleep_ms(unsigned milliseconds)
{
	struct timespec delay;

	delay.tv_sec = (time_t)(milliseconds / 1000U);
	delay.tv_nsec = (long)(milliseconds % 1000U) * 1000000L;

	do {
		if (nanosleep(&delay, &delay) == 0)
			break;
	} while (errno == EINTR);
}

/* Read one small text file into a NUL-terminated buffer; -1 on any error. */
static long read_text(const char *path, char *buffer, size_t size)
{
	long total = 0;
	int descriptor;

	descriptor = open(path, O_RDONLY | O_CLOEXEC);
	if (descriptor < 0)
		return -1;

	for (;;) {
		ssize_t got;
		int saved;

		if ((size_t)total + 1 >= size) {
			close(descriptor);
			errno = EOVERFLOW;
			return -1;
		}

		got = read(descriptor, buffer + total, size - 1 - (size_t)total);
		if (got < 0) {
			saved = errno;
			if (saved == EINTR)
				continue;
			close(descriptor);
			errno = saved;
			return -1;
		}
		if (got == 0)
			break;
		total += got;
	}

	close(descriptor);
	buffer[total] = '\0';

	return total;
}

static char *skip_space(char *text)
{
	while (*text == ' ' || *text == '\t' || *text == '\r' || *text == '\n')
		text++;

	return text;
}

static bool is_space(char character)
{
	return character == ' ' || character == '\t' || character == '\r' ||
	       character == '\n';
}

static void trim_tail(char *text)
{
	size_t length = strlen(text);

	while (length > 0 && is_space(text[length - 1]))
		text[--length] = '\0';
}

static void unquote(char *text)
{
	size_t length = strlen(text);

	if (length >= 2 &&
	    ((text[0] == '"' && text[length - 1] == '"') ||
	     (text[0] == '\'' && text[length - 1] == '\''))) {
		memmove(text, text + 1, length - 2);
		text[length - 2] = '\0';
	}
}

/* Copy after an explicit length check, so no copy is ever truncated. */
static int copy_text(char *destination, size_t size, const char *source,
		     const char *what)
{
	size_t length = strlen(source);

	if (length >= size)
		return failf("%s is %zu bytes, which does not fit %zu", what,
			     length, size - 1);

	memcpy(destination, source, length + 1);

	return 0;
}

static bool join_path(char *destination, size_t size, const char *prefix,
		      const char *stem, const char *suffix)
{
	size_t prefix_length = strlen(prefix);
	size_t stem_length = strlen(stem);
	size_t suffix_length = strlen(suffix);

	if (prefix_length + stem_length + suffix_length + 1 > size)
		return false;

	memcpy(destination, prefix, prefix_length);
	memcpy(destination + prefix_length, stem, stem_length);
	memcpy(destination + prefix_length + stem_length, suffix,
	       suffix_length + 1);

	return true;
}

/*
 * `/proc/bootconfig` prints one flattened `key.subkey = "value"` line per key
 * (fs/proc/bootconfig.c composes dotted keys), so the tuple is a plain line.
 * Lines starting with '#' are the optional trailing bootloader comment.
 */
static int lookup_bootconfig(const char *text, const char *key, char *value,
			     size_t size)
{
	const char *line = text;

	while (*line != '\0') {
		char work[TEXT_LIMIT];
		const char *end = strchr(line, '\n');
		size_t length = end != NULL ? (size_t)(end - line) : strlen(line);

		if (length < sizeof(work)) {
			char *cursor;
			char *equals;

			memcpy(work, line, length);
			work[length] = '\0';

			cursor = skip_space(work);
			trim_tail(cursor);

			if (*cursor != '\0' && *cursor != '#') {
				equals = strchr(cursor, '=');
				if (equals != NULL) {
					char *name = cursor;
					char *found;

					*equals = '\0';
					trim_tail(name);
					found = skip_space(equals + 1);
					trim_tail(found);
					unquote(found);

					if (strcmp(name, key) == 0)
						return copy_text(value, size, found,
								 key) < 0 ? -1 : 1;
				}
			}
		}

		if (end == NULL)
			break;
		line = end + 1;
	}

	return 0;
}

/* The same tuple may instead arrive as a plain kernel command line token. */
static int lookup_cmdline(const char *text, const char *key, char *value,
			  size_t size)
{
	size_t key_length = strlen(key);
	const char *cursor = text;

	while (*cursor != '\0') {
		const char *start;
		size_t length;

		while (is_space(*cursor))
			cursor++;

		start = cursor;
		while (*cursor != '\0' && !is_space(*cursor))
			cursor++;

		length = (size_t)(cursor - start);

		if (length > key_length && start[key_length] == '=' &&
		    strncmp(start, key, key_length) == 0) {
			char found[TEXT_LIMIT];
			size_t value_length = length - key_length - 1;

			if (value_length >= sizeof(found))
				return failf("%s on the kernel command line is %zu bytes, which does not fit %zu",
					     key, value_length, sizeof(found) - 1);

			memcpy(found, start + key_length + 1, value_length);
			found[value_length] = '\0';
			unquote(found);

			return copy_text(value, size, found, key) < 0 ? -1 : 1;
		}

		if (*cursor == '\0')
			break;
	}

	return 0;
}

/*
 * Exactly one source must provide the tuple. Disagreement is fatal so a stale
 * bootconfig can never silently win over a freshly built command line.
 */
static int load_tuple(char *value, size_t size)
{
	static char cmdline_text[8192];
	static char bootconfig_text[65536];
	char from_cmdline[TEXT_LIMIT];
	char from_bootconfig[TEXT_LIMIT];
	bool have_cmdline_file;
	bool have_bootconfig_file;
	int on_cmdline = 0;
	int in_bootconfig = 0;

	have_cmdline_file = read_text("/proc/cmdline", cmdline_text,
				      sizeof(cmdline_text)) >= 0;
	have_bootconfig_file = read_text("/proc/bootconfig", bootconfig_text,
					 sizeof(bootconfig_text)) >= 0;

	if (!have_cmdline_file && !have_bootconfig_file)
		return failf("cannot read /proc/cmdline or /proc/bootconfig: %s",
			     strerror(errno));

	if (have_cmdline_file)
		on_cmdline = lookup_cmdline(cmdline_text, KEY_TUPLE,
					    from_cmdline, sizeof(from_cmdline));
	if (have_bootconfig_file)
		in_bootconfig = lookup_bootconfig(bootconfig_text, KEY_TUPLE,
						  from_bootconfig,
						  sizeof(from_bootconfig));

	if (on_cmdline < 0 || in_bootconfig < 0)
		return -1;

	if (on_cmdline == 0 && in_bootconfig == 0)
		return failf("%s is set neither on the kernel command line nor in /proc/bootconfig",
			     KEY_TUPLE);

	if (on_cmdline != 0 && in_bootconfig != 0 &&
	    strcmp(from_cmdline, from_bootconfig) != 0)
		return failf("%s disagrees between the kernel command line (%s) and /proc/bootconfig (%s)",
			     KEY_TUPLE, from_cmdline, from_bootconfig);

	return copy_text(value, size,
			 on_cmdline != 0 ? from_cmdline : from_bootconfig,
			 KEY_TUPLE);
}

static bool is_uuid(const char *text, size_t length)
{
	size_t index;

	if (length != 36)
		return false;

	for (index = 0; index < length; index++) {
		bool dash = index == 8 || index == 13 || index == 18 ||
			    index == 23;

		if (dash) {
			if (text[index] != '-')
				return false;
			continue;
		}

		if (!((text[index] >= '0' && text[index] <= '9') ||
		      (text[index] >= 'a' && text[index] <= 'f') ||
		      (text[index] >= 'A' && text[index] <= 'F')))
			return false;
	}

	return true;
}

/* Decimal only: no sign, no space, no base prefix and no overflow. */
static bool parse_count(const char *text, size_t length, uint64_t *result)
{
	uint64_t value = 0;
	size_t index;

	if (length == 0)
		return false;

	for (index = 0; index < length; index++) {
		unsigned digit;

		if (text[index] < '0' || text[index] > '9')
			return false;

		digit = (unsigned)(text[index] - '0');
		if (value > (UINT64_MAX - digit) / 10)
			return false;
		value = value * 10 + digit;
	}

	*result = value;

	return true;
}

/*
 * Exactly five nonempty colon-separated fields. Every count is bounded, and
 * the metadata and data runs must fit inside the volume the tuple asks for, so
 * a swapped, truncated or padded tuple can never build a different stack.
 */
static int parse_tuple(const char *text, struct tuple *tuple)
{
	const char *fields[5];
	size_t lengths[5];
	size_t count = 0;
	const char *cursor = text;
	uint64_t total;

	if (*text == '\0')
		return failf("the %s value is empty", KEY_TUPLE);

	for (;;) {
		const char *colon = strchr(cursor, ':');
		size_t length = colon != NULL ? (size_t)(colon - cursor)
					      : strlen(cursor);

		if (count == 5 || length == 0)
			return failf("%s is not <PARTUUID>:<metadata-sectors>:<data-sectors>:<thin-id>:<volume-sectors>: %s",
				     KEY_TUPLE, text);

		fields[count] = cursor;
		lengths[count] = length;
		count++;

		if (colon == NULL)
			break;
		cursor = colon + 1;
	}

	if (count != 5)
		return failf("%s has %zu fields, expected exactly 5: %s", KEY_TUPLE,
			     count, text);

	if (!is_uuid(fields[0], lengths[0]))
		return failf("tuple field 1 is not a 36-character GPT partition UUID: %.*s",
			     (int)lengths[0], fields[0]);

	memcpy(tuple->partuuid, fields[0], 36);
	tuple->partuuid[36] = '\0';

	if (!parse_count(fields[1], lengths[1], &tuple->metadata_sectors))
		return failf("tuple field 2 (metadata sectors) is not an unsigned decimal count: %.*s",
			     (int)lengths[1], fields[1]);
	if (!parse_count(fields[2], lengths[2], &tuple->data_sectors))
		return failf("tuple field 3 (data sectors) is not an unsigned decimal count: %.*s",
			     (int)lengths[2], fields[2]);
	if (!parse_count(fields[3], lengths[3], &tuple->thin_id))
		return failf("tuple field 4 (thin id) is not an unsigned decimal count: %.*s",
			     (int)lengths[3], fields[3]);
	if (!parse_count(fields[4], lengths[4], &tuple->volume_sectors))
		return failf("tuple field 5 (volume sectors) is not an unsigned decimal count: %.*s",
			     (int)lengths[4], fields[4]);

	if (tuple->metadata_sectors == 0)
		return failf("tuple metadata size is zero");

	if (tuple->data_sectors < DATA_BLOCK_SECTORS)
		return failf("tuple data size %llu sectors is smaller than one %d-sector data block",
			     (unsigned long long)tuple->data_sectors,
			     DATA_BLOCK_SECTORS);

	if (tuple->thin_id > MAX_DEV_ID)
		return failf("tuple thin id %llu is above the dm-thin maximum %llu",
			     (unsigned long long)tuple->thin_id,
			     (unsigned long long)MAX_DEV_ID);

	if (tuple->volume_sectors == 0)
		return failf("tuple volume size is zero");

	if (tuple->metadata_sectors > UINT64_MAX - tuple->data_sectors)
		return failf("tuple metadata and data sizes overflow");

	total = tuple->metadata_sectors + tuple->data_sectors;
	if (total > tuple->volume_sectors)
		return failf("tuple metadata and data (%llu sectors) do not fit the requested %llu-sector volume",
			     (unsigned long long)total,
			     (unsigned long long)tuple->volume_sectors);

	return 0;
}

static bool parse_uevent(const char *text, const char *key, char *value,
			 size_t size)
{
	size_t key_length = strlen(key);
	const char *line = text;

	while (*line != '\0') {
		const char *end = strchr(line, '\n');
		size_t length = end != NULL ? (size_t)(end - line) : strlen(line);

		if (length > key_length && line[key_length] == '=' &&
		    strncmp(line, key, key_length) == 0) {
			size_t value_length = length - key_length - 1;

			if (value_length >= size)
				return false;

			memcpy(value, line + key_length + 1, value_length);
			value[value_length] = '\0';

			return true;
		}

		if (end == NULL)
			break;
		line = end + 1;
	}

	return false;
}

static bool is_plain_name(const char *name)
{
	size_t length = strlen(name);
	size_t index;

	if (length == 0 || length >= NAME_LIMIT)
		return false;

	for (index = 0; index < length; index++) {
		char character = name[index];

		if (!((character >= 'a' && character <= 'z') ||
		      (character >= 'A' && character <= 'Z') ||
		      (character >= '0' && character <= '9') ||
		      character == '.' || character == '_' || character == '-'))
			return false;
	}

	return true;
}

/*
 * Find the single partition whose GPT PARTUUID matches, exactly like the
 * espinit by-name resolver scans sysfs. 0 means "not enumerated yet" and is
 * retried; more than one match, a whole-disk match, or an unusable device name
 * is a hard failure.
 */
static int find_partition(const char *partuuid, struct partition *found)
{
	DIR *directory;
	struct dirent *entry;
	int matches = 0;

	directory = opendir("/sys/class/block");
	if (directory == NULL)
		return failf("cannot scan /sys/class/block: %s",
			     strerror(errno));

	while ((entry = readdir(directory)) != NULL) {
		char path[PATH_LIMIT];
		char uevent[1024];
		char devtype[NAME_LIMIT];
		char devname[NAME_LIMIT];
		char value[NAME_LIMIT];

		if (entry->d_name[0] == '.')
			continue;

		devtype[0] = '\0';
		devname[0] = '\0';

		if (!join_path(path, sizeof(path), "/sys/class/block/",
			       entry->d_name, "/uevent"))
			continue;
		if (read_text(path, uevent, sizeof(uevent)) < 0)
			continue;
		if (!parse_uevent(uevent, "PARTUUID", value, sizeof(value)))
			continue;
		if (strcasecmp(value, partuuid) != 0)
			continue;

		matches++;
		if (matches > 1) {
			closedir(directory);
			return failf("multiple block devices report PARTUUID %s; the tuple must name exactly one partition",
				     partuuid);
		}

		if (!parse_uevent(uevent, "DEVTYPE", devtype, sizeof(devtype)) ||
		    strcmp(devtype, "partition") != 0) {
			closedir(directory);
			return failf("PARTUUID %s is not a partition", partuuid);
		}

		if (!parse_uevent(uevent, "DEVNAME", devname, sizeof(devname)) ||
		    !is_plain_name(devname)) {
			closedir(directory);
			return failf("PARTUUID %s has an unusable DEVNAME", partuuid);
		}

		if (copy_text(found->devname, sizeof(found->devname), devname,
			      "partition device name") < 0) {
			closedir(directory);
			return -1;
		}
	}

	closedir(directory);

	if (matches == 0)
		return 0;

	{
		char path[PATH_LIMIT];
		char buffer[NAME_LIMIT];
		unsigned major_number;
		unsigned minor_number;
		unsigned long long sectors;

		if (!join_path(path, sizeof(path), "/sys/class/block/",
			       found->devname, "/dev"))
			return failf("partition device name is too long");
		if (read_text(path, buffer, sizeof(buffer)) < 0)
			return failf("cannot read the device number of %s: %s",
				     found->devname, strerror(errno));
		if (sscanf(buffer, "%u:%u", &major_number, &minor_number) != 2)
			return failf("unexpected device number for %s: %s",
				     found->devname, buffer);

		if (!join_path(path, sizeof(path), "/sys/class/block/",
			       found->devname, "/size"))
			return failf("partition device name is too long");
		if (read_text(path, buffer, sizeof(buffer)) < 0)
			return failf("cannot read the size of %s: %s",
				     found->devname, strerror(errno));
		if (sscanf(buffer, "%llu", &sectors) != 1)
			return failf("unexpected size for %s: %s",
				     found->devname, buffer);

		found->major = major_number;
		found->minor = minor_number;
		found->sectors = sectors;
	}

	return 1;
}

static int wait_partition(const char *partuuid, struct partition *found)
{
	unsigned elapsed = 0;

	for (;;) {
		int status = find_partition(partuuid, found);

		if (status != 0)
			return status;
		if (elapsed >= ENUM_TIMEOUT_MS)
			return failf("no block device reports PARTUUID %s after %u ms; the generated partition is missing",
				     partuuid, ENUM_TIMEOUT_MS);

		sleep_ms(ENUM_INTERVAL_MS);
		elapsed += ENUM_INTERVAL_MS;
	}
}

static void dm_prepare(struct dm *dm, const char *name)
{
	struct dm_ioctl *header;

	memset(dm->buffer, 0, dm->size);

	header = (struct dm_ioctl *)dm->buffer;
	header->version[0] = DM_IFACE_MAJOR;
	header->version[1] = DM_IFACE_MINOR;
	header->version[2] = DM_IFACE_PATCH;
	header->data_size = (uint32_t)dm->size;
	header->data_start = (uint32_t)sizeof(struct dm_ioctl);

	if (name != NULL) {
		size_t length = strlen(name);

		if (length < sizeof(header->name))
			memcpy(header->name, name, length + 1);
	}
}

/* Raw wrapper: keeps errno for callers that classify failures. */
static int dm_call(struct dm *dm, unsigned long command, int *error)
{
	struct dm_ioctl *header = (struct dm_ioctl *)dm->buffer;

	if (ioctl(dm->control, command, dm->buffer) < 0) {
		*error = errno;
		return -1;
	}

	if (header->flags & DM_BUFFER_FULL_FLAG) {
		*error = EOVERFLOW;
		return -1;
	}

	*error = 0;

	return 0;
}

static int dm_require(struct dm *dm, unsigned long command, const char *what)
{
	struct dm_ioctl *header = (struct dm_ioctl *)dm->buffer;
	int error;

	if (dm_call(dm, command, &error) == 0)
		return 0;

	return failf("%s failed: %s (kernel device-mapper ABI %u.%u.%u)", what,
		     error == EOVERFLOW ? "the 16 KiB ioctl buffer is too small"
					: strerror(error),
		     header->version[0], header->version[1], header->version[2]);
}

static int ensure_dm_control(void)
{
	FILE *misc;
	char name[NAME_LIMIT];
	unsigned device_minor;
	bool found = false;

	misc = fopen("/proc/misc", "re");
	if (misc == NULL)
		return failf("cannot read /proc/misc: %s", strerror(errno));

	while (fscanf(misc, "%u %63s", &device_minor, name) == 2) {
		if (strcmp(name, "device-mapper") == 0) {
			found = true;
			break;
		}
	}
	if (ferror(misc)) {
		int error = errno;

		fclose(misc);
		return failf("cannot parse /proc/misc: %s", strerror(error));
	}
	fclose(misc);

	if (!found)
		return 0;
	if (mkdir("/dev/mapper", 0755) < 0 && errno != EEXIST)
		return failf("cannot create /dev/mapper: %s", strerror(errno));
	if (mknod("/dev/mapper/control", S_IFCHR | 0600,
		  makedev(10, device_minor)) < 0 &&
	    errno != EEXIST)
		return failf("cannot create /dev/mapper/control: %s",
			     strerror(errno));

	return 0;
}

static int dm_open(struct dm *dm)
{
	unsigned elapsed = 0;

	dm->control = -1;
	dm->size = DM_BUF_SIZE;
	dm->buffer = calloc(1, dm->size);
	if (dm->buffer == NULL)
		return failf("cannot allocate the %d byte device-mapper buffer",
			     DM_BUF_SIZE);

	for (;;) {
		dm->control = open("/dev/mapper/control", O_RDWR | O_CLOEXEC);
		if (dm->control >= 0)
			break;
		if (errno != ENOENT)
			return failf("cannot open /dev/mapper/control: %s",
				     strerror(errno));
		if (ensure_dm_control() < 0)
			return -1;
		if (elapsed >= ENUM_TIMEOUT_MS)
			return failf("/dev/mapper/control did not appear within %u ms; device-mapper is missing",
				     ENUM_TIMEOUT_MS);

		sleep_ms(ENUM_INTERVAL_MS);
		elapsed += ENUM_INTERVAL_MS;
	}

	dm_prepare(dm, NULL);
	if (dm_require(dm, DM_VERSION, "DM_VERSION") < 0)
		return -1;

	if (((struct dm_ioctl *)dm->buffer)->version[0] != DM_IFACE_MAJOR)
		return failf("kernel device-mapper ABI %u.x is not the required %u.x interface",
			     ((struct dm_ioctl *)dm->buffer)->version[0],
			     DM_IFACE_MAJOR);

	return 0;
}

/*
 * Confirm the kernel has the target registered. DM_GET_TARGET_VERSION answers
 * per target and reports EINVAL for an unregistered one (dm-ioctl.c), which is
 * a hard failure: the table load could not succeed either. A kernel that
 * predates the command answers ENOTTY and the table load below then reports
 * the real error.
 */
static int require_target(struct dm *dm, const char *target)
{
	struct dm_ioctl *header;
	struct dm_target_versions *version;
	int error;

	dm_prepare(dm, target);

	if (dm_call(dm, DM_GET_TARGET_VERSION, &error) < 0) {
		if (error == EINVAL)
			return failf("the device-mapper target %s is not registered on this kernel; load its module first",
				     target);
		return 0;
	}

	header = (struct dm_ioctl *)dm->buffer;
	version = (struct dm_target_versions *)(dm->buffer + header->data_start);

	if (version->name[0] == '\0' || strcmp(version->name, target) != 0)
		return failf("the kernel answered DM_GET_TARGET_VERSION with an unexpected target name");

	return 0;
}

/* 1 when an active table was read, 0 when the device has none, -1 on error. */
static int table_status(struct dm *dm, const char *name, struct table *table)
{
	struct dm_ioctl *header;
	struct dm_target_spec *spec;
	const char *params;
	size_t available;
	size_t limit;
	size_t length;
	int error;

	dm_prepare(dm, name);
	((struct dm_ioctl *)dm->buffer)->flags = DM_STATUS_TABLE_FLAG;

	if (dm_call(dm, DM_TABLE_STATUS, &error) < 0) {
		if (error == ENXIO)
			return 0;
		return failf("DM_TABLE_STATUS for %s failed: %s", name,
			     strerror(error));
	}

	header = (struct dm_ioctl *)dm->buffer;
	if ((header->flags & DM_ACTIVE_PRESENT_FLAG) == 0 ||
	    header->target_count == 0)
		return 0;

	if (header->data_size <= header->data_start)
		return failf("DM_TABLE_STATUS returned no data for %s", name);

	available = header->data_size - header->data_start;
	if (available < sizeof(struct dm_target_spec) + 1)
		return failf("DM_TABLE_STATUS returned a truncated target for %s",
			     name);

	spec = (struct dm_target_spec *)(dm->buffer + header->data_start);
	if ((spec->next != 0 &&
	     ((size_t)spec->next < sizeof(struct dm_target_spec) ||
	      ((size_t)spec->next & 7) != 0 ||
	      (size_t)spec->next > available + 7)))
		return failf("DM_TABLE_STATUS returned a malformed target for %s",
			     name);

	memcpy(table->type, spec->target_type, DM_MAX_TYPE_NAME - 1);
	table->type[DM_MAX_TYPE_NAME - 1] = '\0';
	table->length = spec->length;

	params = (const char *)spec + sizeof(struct dm_target_spec);
	limit = available - sizeof(struct dm_target_spec);
	length = strnlen(params, limit);
	if (length >= limit)
		return failf("DM_TABLE_STATUS returned an unterminated parameter string for %s",
			     name);
	if (length >= sizeof(table->params))
		return failf("the active table parameters of %s exceed %zu bytes",
			     name, sizeof(table->params) - 1);

	memcpy(table->params, params, length + 1);

	return 1;
}

static bool same_params(const char *left, const char *right)
{
	size_t left_length = strlen(left);
	size_t right_length = strlen(right);

	while (left_length > 0 && left[left_length - 1] == ' ')
		left_length--;
	while (right_length > 0 && right[right_length - 1] == ' ')
		right_length--;

	return left_length == right_length &&
	       strncmp(left, right, left_length) == 0;
}

/* Resolve one device-mapper device by its exact name, as the loader does. */
static int lookup_dmdev(const char *name, struct dmdev *device)
{
	DIR *directory;
	struct dirent *entry;
	int matches = 0;

	directory = opendir("/sys/class/block");
	if (directory == NULL)
		return failf("cannot scan /sys/class/block: %s",
			     strerror(errno));

	while ((entry = readdir(directory)) != NULL) {
		char path[PATH_LIMIT];
		char mapped[NAME_LIMIT];
		char buffer[NAME_LIMIT];
		unsigned major_number;
		unsigned minor_number;
		unsigned long long sectors;

		if (strncmp(entry->d_name, "dm-", 3) != 0)
			continue;

		if (!join_path(path, sizeof(path), "/sys/class/block/",
			       entry->d_name, "/dm/name"))
			continue;
		if (read_text(path, mapped, sizeof(mapped)) < 0)
			continue;

		trim_tail(mapped);
		if (strcmp(mapped, name) != 0)
			continue;

		matches++;
		if (matches > 1) {
			closedir(directory);
			return failf("multiple device-mapper devices are named %s",
				     name);
		}

		if (!join_path(path, sizeof(path), "/sys/class/block/",
			       entry->d_name, "/dev"))
			continue;
		if (read_text(path, buffer, sizeof(buffer)) < 0)
			continue;
		if (sscanf(buffer, "%u:%u", &major_number, &minor_number) != 2) {
			closedir(directory);
			return failf("unexpected device number for %s: %s", name,
				     buffer);
		}

		if (!join_path(path, sizeof(path), "/sys/class/block/",
			       entry->d_name, "/size"))
			continue;
		if (read_text(path, buffer, sizeof(buffer)) < 0)
			continue;
		if (sscanf(buffer, "%llu", &sectors) != 1) {
			closedir(directory);
			return failf("unexpected size for %s: %s", name, buffer);
		}

		device->major = major_number;
		device->minor = minor_number;
		device->sectors = sectors;
	}

	closedir(directory);

	return matches;
}

static int create_thin(struct dm *dm, uint64_t thin_id)
{
	struct dm_ioctl *header;
	struct dm_target_msg *message;
	char text[64];
	int error;
	int length;

	length = snprintf(text, sizeof(text), "create_thin %llu",
			  (unsigned long long)thin_id);
	if (length < 0 || (size_t)length >= sizeof(text))
		return failf("thin id does not fit a device-mapper message");

	dm_prepare(dm, NAME_POOL);
	header = (struct dm_ioctl *)dm->buffer;
	message = (struct dm_target_msg *)(dm->buffer + header->data_start);
	message->sector = 0;
	memcpy(message->message, text, (size_t)length + 1);

	if (dm_call(dm, DM_TARGET_MSG, &error) == 0)
		return 0;
	if (error == EEXIST)
		return 0;

	return failf("DM_TARGET_MSG create_thin %llu failed: %s",
		     (unsigned long long)thin_id, strerror(error));
}

static int load_table(struct dm *dm, const char *name, const char *type,
		      uint64_t length, const char *params)
{
	struct dm_ioctl *header;
	struct dm_target_spec *spec;
	size_t offset;
	size_t params_size;
	size_t next;

	dm_prepare(dm, name);

	header = (struct dm_ioctl *)dm->buffer;
	header->target_count = 1;

	spec = (struct dm_target_spec *)(dm->buffer + sizeof(struct dm_ioctl));
	if (strlen(type) >= sizeof(spec->target_type))
		return failf("the %s target type does not fit the device-mapper target name field",
			     type);

	spec->sector_start = 0;
	spec->length = length;
	memcpy(spec->target_type, type, strlen(type) + 1);

	offset = sizeof(struct dm_ioctl) + sizeof(struct dm_target_spec);
	params_size = strlen(params) + 1;
	if (offset + params_size + 8 > dm->size)
		return failf("the %s table for %s does not fit the %d byte device-mapper buffer",
			     type, name, DM_BUF_SIZE);

	memcpy(dm->buffer + offset, params, params_size);

	next = (params_size + sizeof(struct dm_target_spec) + 7) & ~(size_t)7;
	spec->next = (uint32_t)next;

	if (dm_require(dm, DM_TABLE_LOAD, "DM_TABLE_LOAD") < 0)
		return -1;

	dm_prepare(dm, name);
	if (dm_require(dm, DM_DEV_SUSPEND, "DM_DEV_SUSPEND (resume)") < 0)
		return -1;

	return 0;
}

/*
 * Bring one mapping up. A matching pre-existing table is reused as is, so a
 * reboot of an unchanged pool is idempotent; any other pre-existing table for
 * these four names is fatal.
 */
static int activate(struct dm *dm, int index, const char *type, uint64_t length,
		    const char *params)
{
	const char *name = device_names[index];
	struct dmdev geometry;
	struct table table;
	int have;

	have = table_status(dm, name, &table);
	if (have < 0)
		return -1;

	if (have == 1) {
		if (strcmp(table.type, type) != 0 || table.length != length ||
		    !same_params(table.params, params))
			return failf("existing device-mapper device %s is not the expected stack (found \"%s\" %llu sectors \"%s\"; expected \"%s\" %llu sectors \"%s\")",
				     name, table.type,
				     (unsigned long long)table.length, table.params,
				     type, (unsigned long long)length, params);

		have = lookup_dmdev(name, &geometry);
		if (have < 0)
			return -1;
		if (have != 1)
			return failf("verified device-mapper device %s disappeared",
				     name);
		if (geometry.sectors != length)
			return failf("existing device-mapper device %s reports %llu sectors instead of %llu",
				     name, (unsigned long long)geometry.sectors,
				     (unsigned long long)length);

		device_geometry[index] = geometry;

		return 0;
	}

	have = lookup_dmdev(name, &geometry);
	if (have < 0)
		return -1;

	if (have == 0) {
		dm_prepare(dm, name);
		if (dm_require(dm, DM_DEV_CREATE, "DM_DEV_CREATE") < 0)
			return -1;
		device_created[index] = true;
	} else {
		fprintf(stderr,
			"thin-activate: reusing the tableless device-mapper device %s\n",
			name);
	}

	if (load_table(dm, name, type, length, params) < 0)
		return -1;

	have = lookup_dmdev(name, &geometry);
	if (have < 0)
		return -1;
	if (have != 1)
		return failf("device-mapper device %s is not visible in /sys/class/block after activation",
			     name);
	if (geometry.sectors != length)
		return failf("device-mapper device %s was activated with %llu sectors instead of %llu",
			     name, (unsigned long long)geometry.sectors,
			     (unsigned long long)length);

	device_geometry[index] = geometry;

	return 0;
}

/*
 * Convenience only. The espinit resolver reads the device-mapper name from
 * sysfs and builds its own node, and after handoff ueventd publishes
 * /dev/block/mapper/<name> from the device-mapper uevent. A missing or
 * read-only /dev therefore only warns; a node that exists with the wrong
 * device number is still fatal.
 */
static int publish_node(void)
{
	const struct dmdev *device = &device_geometry[DEVICE_OUTPUT];
	char path[PATH_LIMIT];
	struct stat status;

	if (!join_path(path, sizeof(path), "/dev/mapper/", NAME_OUTPUT, ""))
		return failf("the output device path does not fit %d bytes",
			     PATH_LIMIT);

	if (mkdir("/dev/mapper", 0755) < 0 && errno != EEXIST)
		return failf("cannot create /dev/mapper: %s", strerror(errno));

	if (stat(path, &status) == 0) {
		if (!S_ISBLK(status.st_mode) ||
		    major(status.st_rdev) != device->major ||
		    minor(status.st_rdev) != device->minor)
			return failf("%s exists but is not the expected block device %u:%u",
				     path, device->major, device->minor);

		return 0;
	}

	if (errno != ENOENT)
		return failf("cannot inspect %s: %s", path, strerror(errno));

	if (mknod(path, S_IFBLK | 0600,
		  makedev(device->major, device->minor)) < 0)
		return failf("cannot create %s: %s", path, strerror(errno));

	return 0;
}

/* Remove, in reverse order, everything this invocation created. */
static void cleanup(struct dm *dm)
{
	int index;

	for (index = DEVICE_COUNT - 1; index >= 0; index--) {
		if (!device_created[index])
			continue;

		dm_prepare(dm, device_names[index]);
		if (dm_require(dm, DM_DEV_REMOVE, "DM_DEV_REMOVE") < 0) {
			fprintf(stderr,
				"thin-activate: warning: cannot remove the partially created device %s\n",
				device_names[index]);
			error_text[0] = '\0';
		} else {
			device_created[index] = false;
		}
	}
}

int main(void)
{
	static char tuple_text[TEXT_LIMIT];
	static char params[TEXT_LIMIT];
	struct partition partition;
	struct tuple tuple;
	struct dm dm;
	bool ready = false;
	uint64_t total;

	dm.control = -1;
	dm.buffer = NULL;
	dm.size = 0;

	if (load_tuple(tuple_text, sizeof(tuple_text)) < 0)
		goto failed;
	if (parse_tuple(tuple_text, &tuple) < 0)
		goto failed;
	if (wait_partition(tuple.partuuid, &partition) != 1)
		goto failed;

	total = tuple.metadata_sectors + tuple.data_sectors;
	if (total > partition.sectors) {
		failf("tuple metadata and data (%llu sectors) exceed partition %s (%llu sectors)",
		      (unsigned long long)total, partition.devname,
		      (unsigned long long)partition.sectors);
		goto failed;
	}

	if (dm_open(&dm) < 0)
		goto failed;
	ready = true;

	if (require_target(&dm, "linear") < 0)
		goto failed;
	if (require_target(&dm, "thin-pool") < 0)
		goto failed;
	if (require_target(&dm, "thin") < 0)
		goto failed;

	snprintf(params, sizeof(params), "%u:%u 0", partition.major,
		 partition.minor);
	if (activate(&dm, DEVICE_META, "linear", tuple.metadata_sectors,
		     params) < 0)
		goto failed;

	snprintf(params, sizeof(params), "%u:%u %llu", partition.major,
		 partition.minor, (unsigned long long)tuple.metadata_sectors);
	if (activate(&dm, DEVICE_DATA, "linear", tuple.data_sectors, params) < 0)
		goto failed;

	snprintf(params, sizeof(params), "%u:%u %u:%u %d %d 1 skip_block_zeroing",
		 device_geometry[DEVICE_META].major,
		 device_geometry[DEVICE_META].minor,
		 device_geometry[DEVICE_DATA].major,
		 device_geometry[DEVICE_DATA].minor, DATA_BLOCK_SECTORS,
		 LOW_WATER_BLOCKS);
	if (activate(&dm, DEVICE_POOL, "thin-pool", tuple.data_sectors,
		     params) < 0)
		goto failed;

	if (create_thin(&dm, tuple.thin_id) < 0)
		goto failed;

	snprintf(params, sizeof(params), "%u:%u %llu",
		 device_geometry[DEVICE_POOL].major,
		 device_geometry[DEVICE_POOL].minor,
		 (unsigned long long)tuple.thin_id);
	if (activate(&dm, DEVICE_OUTPUT, "thin", tuple.volume_sectors,
		     params) < 0)
		goto failed;

	if (publish_node() < 0)
		fprintf(stderr, "thin-activate: warning: %s\n", error_text);

	fprintf(stdout,
		"thin-activate: %s active over %s (%llu sectors), thin id %llu, volume %llu sectors\n",
		NAME_OUTPUT, NAME_POOL, (unsigned long long)tuple.data_sectors,
		(unsigned long long)tuple.thin_id,
		(unsigned long long)tuple.volume_sectors);

	return 0;

failed:
	if (ready)
		cleanup(&dm);
	if (dm.control >= 0)
		close(dm.control);
	free(dm.buffer);

	fprintf(stderr, "thin-activate: %s\n",
		error_text[0] != '\0' ? error_text : "failed");

	return 1;
}
