/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef SHIM_H
#define SHIM_H
#define _GNU_SOURCE
#include <stdint.h>
#include <stdbool.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
typedef uint8_t u8; typedef uint16_t u16; typedef uint32_t u32; typedef uint64_t u64;
typedef u16 efi_char16_t; typedef struct { u8 b[16]; } efi_guid_t;
typedef unsigned long efi_status_t;
#define EFI_SUCCESS 0UL
#define EFI_INVALID_PARAMETER 2UL
#define EFI_BUFFER_TOO_SMALL 5UL
#define EFI_DEVICE_ERROR 7UL
#define EFI_OUT_OF_RESOURCES 9UL
#define EFI_NOT_FOUND 14UL
#define __init
#define __exit
#define GFP_KERNEL 0
#define DEFINE_MUTEX(n) int n
#define mutex_lock(p) ((void)(p))
#define mutex_unlock(p) ((void)(p))
#define module_param(a,b,c)
#define MODULE_PARM_DESC(a,b)
#define module_init(fn) static void *const use_init __attribute__((unused)) = (void *)&fn
#define module_exit(fn) static void *const use_exit __attribute__((unused)) = (void *)&fn
#define BLK_OPEN_READ 1
#define BLK_OPEN_WRITE 2
#define MKDEV(a,b) makedev(a,b)
#define MAJOR(d) major(d)
#define MINOR(d) minor(d)
#define min_t(t,a,b) ((t)(a) < (t)(b) ? (t)(a) : (t)(b))
#define kmalloc(n,f) malloc(n)
#define kvmalloc(n,f) malloc(n)
#define kfree(p) free(p)
#define kvfree(p) free(p)
#define IS_ERR(p) ((intptr_t)(p) < 0)
#define PTR_ERR(p) ((int)(intptr_t)(p))
static u16 get_unaligned_le16(const void *p) { const u8 *b=p; return b[0] | (u16)b[1]<<8; }
static u32 get_unaligned_le32(const void *p) { const u8 *b=p; return get_unaligned_le16(b) | (u32)get_unaligned_le16(b+2)<<16; }
static u64 get_unaligned_le64(const void *p) { const u8 *b=p; return get_unaligned_le32(b) | (u64)get_unaligned_le32(b+4)<<32; }
static void put_unaligned_le16(u16 v, void *p) { u8 *b=p; b[0]=v; b[1]=v>>8; }
static void put_unaligned_le32(u32 v, void *p) { u8 *b=p; put_unaligned_le16(v,b); put_unaligned_le16(v>>16,b+2); }
struct file { int fd; u64 size; };
static const char *host_path;
static unsigned int phase;
static bool corrupt_readback;
static struct file *bdev_file_open_by_dev(dev_t d, int mode, void *holder, void *ops) {
 struct stat st; struct file *f; (void)d; (void)mode; (void)holder; (void)ops;
 int fd=open(host_path,O_RDWR); if(fd<0) return (void *)(intptr_t)-errno;
 if(fstat(fd,&st)) { close(fd); return (void *)(intptr_t)-errno; }
 f=malloc(sizeof(*f)); f->fd=fd; f->size=st.st_size; return f;
}
#define file_bdev(f) (f)
#define bdev_nr_bytes(f) ((f)->size)
static ssize_t kernel_read(struct file *f, void *p, size_t n, loff_t *off) { ssize_t r=pread(f->fd,p,n,*off); if(r>0) { *off+=r; if(corrupt_readback && phase) { ((u8 *)p)[0]^=1; corrupt_readback=false; } } return r; }
static ssize_t kernel_write(struct file *f,const void *p,size_t n,loff_t *off) { ssize_t r=pwrite(f->fd,p,n,*off); fprintf(stderr,"write %lld %zu",(long long)*off,n); if(n==1) fprintf(stderr," %02x",*(const u8 *)p); fputc('\n',stderr); if(r>0)*off+=r; return r; }
static int vfs_fsync(struct file *f,int datasync) { (void)datasync; fprintf(stderr,"flush %u\n",++phase); return fsync(f->fd); }
static void fput(struct file *f) { close(f->fd); free(f); }
struct efivars { int unused; };
struct efivar_operations {
 efi_status_t (*get_variable)(efi_char16_t *,efi_guid_t *,u32 *,unsigned long *,void *);
 efi_status_t (*get_next_variable)(unsigned long *,efi_char16_t *,efi_guid_t *);
 efi_status_t (*set_variable)(efi_char16_t *,efi_guid_t *,u32,unsigned long,void *);
 void *set_variable_nonblocking;
 efi_status_t (*query_variable_info)(u32,u64 *,u64 *,u64 *);
};
static const struct efivar_operations *captured;
static int efivars_register(struct efivars *v,const struct efivar_operations *ops) { (void)v; captured=ops; return 0; }
static int efivars_unregister(struct efivars *v) { (void)v; captured=NULL; return 0; }
#endif
