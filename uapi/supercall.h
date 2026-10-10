#ifndef __ESP_UAPI_SUPERCALL_H
#define __ESP_UAPI_SUPERCALL_H

#include <linux/ioctl.h>
#include <linux/types.h>

static const __u32 ESU_UAPI_VERSION = 4;
/* reboot(magic1, magic2, 0, &fd) installs an owned anonymous control FD. */
static const __u32 ESU_INSTALL_MAGIC1 = 0x45535049; /* ESPI */
static const __u32 ESU_INSTALL_MAGIC2 = 0x4e495446; /* NITF */
#define ESU_CONTROL_NAME "[kernelsu-esp]"
static const __u32 ESU_GET_INFO_FLAG_LKM = (1U << 0);
static const __u32 ESU_STATE_READY = (1U << 0);

struct esu_get_info_cmd {
    __u32 version;
    __u32 flags;
    __u32 uapi_version;
    __u32 state;
};

/* Root-only, immutable one-pass supplement; bootstrap rc precedes module rc.
 * reserved must be zero; ptr must be nonzero when len is nonzero.
 * EALREADY on a second supply, EBUSY if init consumed before supply.
 * norc suppresses delivery without changing the one-supply contract.
 */
struct esu_module_rc_cmd {
    __aligned_u64 ptr;
    __u32 len; /* 0..65536 */
    __u32 reserved;
};

struct ksu_set_sepolicy_cmd {
    __u64 data_len;
    __aligned_u64 data;
};

#define ESU_POLICY_MAX_SIZE (64U * 1024U * 1024U)
/* Root-only coherent live snapshot, never the original boot policy.
 * ptr=0,len=0 queries a bounded allocation capacity (not a snapshot).
 * Otherwise ptr must be nonzero and len is the buffer capacity, <= MAX_SIZE.
 * Success replaces len with the serialized byte count. Policy growth between
 * query and export can return ENOSPC; callers must fail rather than retry.
 * Serialization preserves live Android netlink configuration.
 */
struct esu_get_sepolicy_cmd {
    __aligned_u64 ptr;
    __u64 len;
};
struct ksu_sepolicy_cmd_hdr {
    __u32 cmd;
    __u32 subcmd;
};
/* Arguments following each header: [u32 len][len bytes][NUL].
 * len excludes NUL; zero means ALL. Arity by KSU_SEPOLICY_CMD_*:
 * NORMAL_PERM=4, XPERM=5, TYPE_STATE=1, TYPE=2, TYPE_ATTR=2, ATTR=1,
 * TYPE_TRANSITION=5, TYPE_CHANGE=4, GENFSCON=3 (selinux.h command IDs).
 */
/* arm64/x86_64 encodings: GET_INFO=0x80104502,
 * SET_MODULE_RC=0x40104515, SET_SEPOLICY=0xc0004504,
 * GET_SEPOLICY=0xc0104516.
 * SET_SEPOLICY intentionally retains its size-zero serialized-batch encoding;
 * the argument still points to the 16-byte ksu_set_sepolicy_cmd.
 */

/* All commands are root-only. Type E is distinct from KernelSU's type K. */
static const __u32 ESU_IOCTL_GET_INFO = _IOR('E', 2, struct esu_get_info_cmd);
static const __u32 ESU_IOCTL_SET_MODULE_RC = _IOW('E', 21, struct esu_module_rc_cmd);
static const __u32 ESU_IOCTL_SET_SEPOLICY = _IOC(_IOC_READ | _IOC_WRITE, 'E', 4, 0);
static const __u32 ESU_IOCTL_GET_SEPOLICY = _IOWR('E', 22, struct esu_get_sepolicy_cmd);

#endif
