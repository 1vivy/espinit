//! Exact userspace mirror of `modules/gpt/gpt_uapi.h`.
//!
//! Field order, types, array bounds and total sizes are a hard ABI contract
//! with the kernel module. Every request number is derived here from
//! `size_of::<..>()` exactly as the C `_IOW`/`_IOR` macros derive their size
//! field, and both the sizes and the resulting request numbers are asserted at
//! compile time, so the Rust view cannot silently drift from the header.

/// ABI version carried by both APPLY and QUERY.
pub const GPT_ABI_VERSION: u32 = 2;

/// Maximum projections carried by one APPLY.
pub const GPT_MAX_PROJECTIONS: usize = 128;

/// Maximum hidden physical partitions carried by one APPLY.
pub const GPT_MAX_HIDDEN: usize = 256;

/// Projected label bytes without the terminating NUL.
pub const GPT_LABEL_BYTES: usize = 36;

/// APPLY flag: seal physical storage this view does not project.
pub const GPT_APPLY_FLAG_SEAL: u32 = 0x1;

/// One projected device: backend `major:minor`, explicit access mode and label.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GptProjection {
    pub major: u32,
    pub minor: u32,
    pub read_only: u8,
    pub reserved: [u8; 7],
    /// NUL-terminated ASCII label; trailing bytes must stay zero.
    pub name: [u8; GPT_LABEL_BYTES + 1],
    pub reserved2: [u8; 3],
}

impl Default for GptProjection {
    fn default() -> Self {
        Self {
            major: 0,
            minor: 0,
            read_only: 0,
            reserved: [0; 7],
            name: [0; GPT_LABEL_BYTES + 1],
            reserved2: [0; 3],
        }
    }
}

/// One hidden physical partition device number.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct GptDevice {
    pub major: u32,
    pub minor: u32,
}

/// APPLY payload: header, fixed projection array, fixed hidden array.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GptApply {
    pub version: u32,
    pub count: u32,
    pub hide_count: u32,
    pub flags: u32,
    pub projections: [GptProjection; GPT_MAX_PROJECTIONS],
    pub hide: [GptDevice; GPT_MAX_HIDDEN],
}

impl Default for GptApply {
    fn default() -> Self {
        Self {
            version: 0,
            count: 0,
            hide_count: 0,
            flags: 0,
            projections: [GptProjection::default(); GPT_MAX_PROJECTIONS],
            hide: [GptDevice::default(); GPT_MAX_HIDDEN],
        }
    }
}

/// QUERY reply: ABI version, active flag and applied projection count.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GptQuery {
    pub version: u32,
    pub active: u32,
    pub count: u32,
    pub reserved: u32,
}

const _: () = assert!(std::mem::size_of::<GptProjection>() == GPT_LABEL_BYTES + 20);
const _: () = assert!(std::mem::size_of::<GptDevice>() == 8);
const _: () = assert!(
    std::mem::size_of::<GptApply>()
        == 16 + GPT_MAX_PROJECTIONS * (GPT_LABEL_BYTES + 20) + GPT_MAX_HIDDEN * 8
);
const _: () = assert!(std::mem::size_of::<GptQuery>() == 16);

/// Encode one ioctl request number the way `_IOC` does.
const fn ioc(direction: u32, number: u32, size: usize) -> u32 {
    (direction << 30) | (((size as u32) & 0x3fff) << 16) | ((b'G' as u32) << 8) | (number & 0xff)
}

/// `GPT_IOCTL_APPLY` (`_IOW('G', 1, struct gpt_apply)`).
pub const GPT_IOCTL_APPLY: u32 = ioc(1, 1, std::mem::size_of::<GptApply>());

/// `GPT_IOCTL_QUERY` (`_IOR('G', 2, struct gpt_query)`).
pub const GPT_IOCTL_QUERY: u32 = ioc(2, 2, std::mem::size_of::<GptQuery>());

const _: () = assert!(GPT_IOCTL_APPLY == 0x6410_4701);
const _: () = assert!(GPT_IOCTL_QUERY == 0x8010_4702);

/// Copy one validated projection label into the fixed-size ABI field, leaving
/// every trailing byte zero. Labels use the product contract `[A-Za-z0-9_-]`.
pub fn label_bytes(label: &str) -> Option<[u8; GPT_LABEL_BYTES + 1]> {
    if label.is_empty() || label.len() > GPT_LABEL_BYTES {
        return None;
    }

    if !label
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }

    let mut name = [0u8; GPT_LABEL_BYTES + 1];
    name[..label.len()].copy_from_slice(label.as_bytes());
    Some(name)
}

/// Read back a product-contract label from one ABI projection field, or `None`
/// when its encoding, NUL termination, or zero tail is invalid.
pub fn projection_label(projection: &GptProjection) -> Option<String> {
    let end = projection.name.iter().position(|byte| *byte == 0)?;

    if projection.name[end..].iter().any(|byte| *byte != 0) {
        return None;
    }

    let bytes = &projection.name[..end];

    if bytes.is_empty()
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'-'))
    {
        return None;
    }

    Some(bytes.iter().map(|byte| char::from(*byte)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_sizes_and_offsets_match_the_kernel_header() {
        assert_eq!(std::mem::size_of::<GptProjection>(), 56);
        assert_eq!(std::mem::size_of::<GptDevice>(), 8);
        assert_eq!(std::mem::size_of::<GptApply>(), 9232);
        assert_eq!(std::mem::size_of::<GptQuery>(), 16);
        assert_eq!(std::mem::align_of::<GptApply>(), 4);

        // Field order is the ABI; check the packed layout the header defines.
        let apply = GptApply::default();
        let base = std::ptr::addr_of!(apply) as usize;
        assert_eq!(std::ptr::addr_of!(apply.count) as usize - base, 4);
        assert_eq!(std::ptr::addr_of!(apply.hide_count) as usize - base, 8);
        assert_eq!(std::ptr::addr_of!(apply.flags) as usize - base, 12);
        assert_eq!(std::ptr::addr_of!(apply.projections) as usize - base, 16);
        assert_eq!(
            std::ptr::addr_of!(apply.hide) as usize - base,
            16 + GPT_MAX_PROJECTIONS * 56
        );
        assert_eq!(
            std::ptr::addr_of!(apply.hide) as usize - base + GPT_MAX_HIDDEN * 8,
            9232
        );

        let projection = GptProjection::default();
        let projection_base = std::ptr::addr_of!(projection) as usize;
        assert_eq!(
            std::ptr::addr_of!(projection.read_only) as usize - projection_base,
            8
        );
        assert_eq!(
            std::ptr::addr_of!(projection.name) as usize - projection_base,
            16
        );
        assert_eq!(
            std::ptr::addr_of!(projection.reserved2) as usize - projection_base,
            53
        );
    }

    #[test]
    fn request_numbers_match_the_c_macros() {
        // _IOW('G', 1, struct gpt_apply) and _IOR('G', 2, struct gpt_query).
        assert_eq!(GPT_IOCTL_APPLY, 0x6410_4701);
        assert_eq!(GPT_IOCTL_QUERY, 0x8010_4702);
    }

    #[test]
    fn labels_require_bounded_printable_ascii_with_zero_tail() {
        let name = label_bytes("super").unwrap();
        assert_eq!(&name[..5], b"super");
        assert!(name[5..].iter().all(|byte| *byte == 0));

        let longest = label_bytes(&"p".repeat(GPT_LABEL_BYTES)).unwrap();
        assert_eq!(longest[GPT_LABEL_BYTES], 0);

        for label in ["", " ", "\u{e9}"] {
            assert!(label_bytes(label).is_none(), "{label:?}");
        }
        assert!(label_bytes(&"p".repeat(GPT_LABEL_BYTES + 1)).is_none());

        let projection = GptProjection {
            name,
            ..GptProjection::default()
        };
        assert_eq!(projection_label(&projection).as_deref(), Some("super"));

        // A non-zero byte after the NUL terminator must not be accepted.
        let mut dirty = projection;
        dirty.name[GPT_LABEL_BYTES] = b'x';
        assert!(projection_label(&dirty).is_none());

        // An empty or unterminated label is not a valid active projection.
        assert!(projection_label(&GptProjection::default()).is_none());
    }
}
