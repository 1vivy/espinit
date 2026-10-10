/* SPDX-License-Identifier: GPL-3.0-only */
/* Host boundary test: cc -Wall -Wextra init_rc_prefix_test.c -o init_rc_prefix_test
 * Executes the production callbacks with deterministic usercopy faults and
 * vectored destinations; no kernel/device dependencies or source assertions.
 */
/* glibc exposes the Linux loff_t host stub only with misc extensions enabled,
 * including when this regression is compiled in strict C11 mode.
 */
#ifndef _DEFAULT_SOURCE
#define _DEFAULT_SOURCE 1
#endif
#include <assert.h>
#include <errno.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>

#define __user
#define DEFINE_MUTEX(name) int name
#define min(a, b) ((a) < (b) ? (a) : (b))
#define kvfree free
static void mutex_lock(int *lock) { assert(!*lock); *lock = 1; }
static void mutex_unlock(int *lock) { assert(*lock); *lock = 0; }
struct inode { int unused; };
struct file;
struct file_operations { int (*release)(struct inode *, struct file *); };
struct file { const struct file_operations *f_op; };
struct kiocb { struct file *ki_filp; loff_t ki_pos; };
struct iov_iter { char *base[2]; size_t len[2]; size_t index, offset; };
static size_t copy_budget = (size_t)-1;
static size_t copy_to_user(char *to, const char *from, size_t count)
{
    size_t copied = min(count, copy_budget);
    memcpy(to, from, copied);
    copy_budget -= copied;
    return count - copied;
}
static size_t iov_iter_count(const struct iov_iter *to)
{
    size_t count = 0, i;
    for (i = to->index; i < 2; i++)
        count += to->len[i] - (i == to->index ? to->offset : 0);
    return count;
}
static size_t copy_to_iter(const char *from, size_t count, struct iov_iter *to)
{
    size_t done = 0;
    while (to->index < 2 && done < count && copy_budget) {
        size_t n = min(count - done, to->len[to->index] - to->offset);
        n = min(n, copy_budget);
        if (n) memcpy(to->base[to->index] + to->offset, from + done, n);
        to->offset += n;
        done += n;
        copy_budget -= n;
        if (to->offset == to->len[to->index]) {
            to->index++;
            to->offset = 0;
        }
    }
    return done;
}
#define EGYSK_INIT_RC_TEST
#include "../runtime/init_integration.c"

static const char stock[] = "on init\n    start stock\n";
static unsigned int backing_calls, releases;
static int backing_error;
static ssize_t backing_read(struct file *file, char *buf, size_t count, loff_t *pos)
{
    size_t n;
    (void)file;
    backing_calls++;
    if (backing_error) return backing_error;
    n = min(count, sizeof(stock) - 1 - (size_t)*pos);
    memcpy(buf, stock + *pos, n);
    *pos += n;
    return n;
}
static ssize_t backing_iter(struct kiocb *iocb, struct iov_iter *to)
{
    size_t n;
    backing_calls++;
    if (backing_error) return backing_error;
    n = copy_to_iter(stock + iocb->ki_pos,
                     sizeof(stock) - 1 - (size_t)iocb->ki_pos, to);
    iocb->ki_pos += n;
    return n;
}
static int backing_release(struct inode *inode, struct file *file)
{
    (void)inode;
    assert(file->f_op == orig_fops);
    releases++;
    return -EIO;
}
static void setup(size_t len)
{
    module_rc_buf = malloc(len ? len : 1);
    assert(module_rc_buf);
    memset(module_rc_buf, 'P', len);
    module_rc_len = len;
    module_rc_pos = 0;
    module_rc_set = module_rc_loaded = true;
    assert(module_rc_set && module_rc_loaded);
    orig_read = backing_read;
    orig_read_iter = backing_iter;
    backing_calls = 0;
    backing_error = 0;
    copy_budget = (size_t)-1;
}
int main(void)
{
    static const struct file_operations original = { .release = backing_release };
    struct file file = { .f_op = &fops_proxy };
    struct kiocb iocb = { .ki_filp = &file };
    loff_t pos = 0;
    char out[128] = {0};
    size_t total = 0, i;
    ssize_t n;
    struct iov_iter to = { .base = {out, out + 2}, .len = {2, 7} };
    orig_fops = &original;

    setup(7);
    assert(read_proxy(&file, NULL, 0, &pos) == 0);
    assert(!module_rc_pos && !backing_calls && !pos);
    copy_budget = 0;
    assert(read_proxy(&file, out, 7, &pos) == -EFAULT);
    assert(!module_rc_pos && !pos);
    copy_budget = 3;
    assert(read_proxy(&file, out, 7, &pos) == 3);
    assert(module_rc_pos == 3 && !pos && !backing_calls);
    copy_budget = (size_t)-1;
    while ((n = read_proxy(&file, out + total, 2, &pos)) > 0) total += n;
    assert(n == 0 && total == 4 + sizeof(stock) - 1);
    assert(!memcmp(out, "PPPP", 4));
    assert(!memcmp(out + 4, stock, sizeof(stock) - 1));
    assert(pos == sizeof(stock) - 1 && module_rc_len == 7);
    backing_error = -EIO;
    assert(read_proxy(&file, out, sizeof(out), &pos) == -EIO);
    assert(release_proxy(NULL, &file) == -EIO);
    assert(!module_rc_buf && !module_rc_len && releases == 1);

    setup(7);
    to.len[0] = to.len[1] = 0;
    assert(read_iter_proxy(&iocb, &to) == 0 && !module_rc_pos);
    to.len[0] = 2; to.len[1] = 7;
    copy_budget = 0;
    assert(read_iter_proxy(&iocb, &to) == -EFAULT && !module_rc_pos);
    copy_budget = 3;
    assert(read_iter_proxy(&iocb, &to) == 3 && module_rc_pos == 3);
    assert(!iocb.ki_pos && !backing_calls && iov_iter_count(&to) == 6);
    copy_budget = (size_t)-1;
    assert(read_iter_proxy(&iocb, &to) == 4);
    assert(!iocb.ki_pos && !backing_calls && iov_iter_count(&to) == 2);
    assert(read_iter_proxy(&iocb, &to) == 2 && iocb.ki_pos == 2);
    assert(!memcmp(out, "PPPPPPP", 7) && !memcmp(out + 7, stock, 2));
    total = 9;
    while (iocb.ki_pos < (loff_t)(sizeof(stock) - 1)) {
        struct iov_iter rest = {
            .base = {out + total, out + total + 1}, .len = {1, 2}
        };
        n = read_iter_proxy(&iocb, &rest);
        assert(n > 0);
        total += n;
    }
    {
        struct iov_iter eof = { .base = {out + total, NULL}, .len = {1, 0} };
        assert(read_iter_proxy(&iocb, &eof) == 0);
        backing_error = -EIO;
        assert(read_iter_proxy(&iocb, &eof) == -EIO);
    }
    assert(total == 7 + sizeof(stock) - 1);
    assert(!memcmp(out + 7, stock, sizeof(stock) - 1));
    free_module_rc();

    setup(65536);
    pos = 0;
    for (i = 0; i < 65536; i++) {
        assert(read_proxy(&file, out, 1, &pos) == 1 && out[0] == 'P');
        assert(!pos && !backing_calls);
    }
    assert(read_proxy(&file, out, sizeof(out), &pos) == sizeof(stock) - 1);
    assert(!memcmp(out, stock, sizeof(stock) - 1));
    assert(read_proxy(&file, out, sizeof(out), &pos) == 0);
    free_module_rc();
    setup(0);
    pos = 0;
    assert(read_proxy(&file, out, sizeof(out), &pos) == sizeof(stock) - 1);
    assert(!memcmp(out, stock, sizeof(stock) - 1));
    free_module_rc();
    return 0;
}
