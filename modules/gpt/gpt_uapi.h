/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef ESPINIT_GPT_UAPI_H
#define ESPINIT_GPT_UAPI_H
#include <linux/types.h>
#include <linux/ioctl.h>
#define GPT_ABI_VERSION 1U
#define GPT_MAX_PROJECTIONS 64U
#define GPT_MAX_HIDDEN 256U
#define GPT_LABEL_BYTES 36U
struct gpt_projection {
	__u32 major;
	__u32 minor;
	__u8 read_only;
	__u8 reserved[7];
	char name[GPT_LABEL_BYTES + 1];
	__u8 reserved2[3];
};
struct gpt_device {
	__u32 major;
	__u32 minor;
};
struct gpt_apply {
	__u32 version, count, hide_count, flags;
	struct gpt_projection projections[GPT_MAX_PROJECTIONS];
	struct gpt_device hide[GPT_MAX_HIDDEN];
};
struct gpt_query {
	__u32 version, active, count, reserved;
};
#define GPT_IOCTL_APPLY _IOW('G', 1, struct gpt_apply)
#define GPT_IOCTL_QUERY _IOR('G', 2, struct gpt_query)
#endif
