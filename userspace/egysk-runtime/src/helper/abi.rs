//! Hand-maintained mirror of kernel/include/uapi/{supercall,selinux}.h.
//! Branding does not change transport magic, ioctl encodings or policy numbers.

pub(super) const UAPI_VERSION: u32 = 4;
pub(super) const INSTALL_MAGIC1: u32 = 0x45535049;
pub(super) const INSTALL_MAGIC2: u32 = 0x4e495446;
pub(super) const CONTROL_NAME: &str = "[egysk]";
pub(super) const STATE_READY: u32 = 1 << 0;
pub(super) const GET_INFO: u32 = 0x80104502;
pub(super) const SET_MODULE_RC: u32 = 0x40104515;
pub(super) const SET_SEPOLICY: u32 = 0xc0004504;
pub(super) const GET_SEPOLICY: u32 = 0xc0104516;
pub(super) const MODULE_RC_MAX_SIZE: usize = 65536;
pub(super) const LIVE_POLICY_MAX_SIZE: usize = 64 * 1024 * 1024;

/// egysk_get_info_cmd in supercall.h.
#[repr(C)]
#[derive(Default, Debug)]
pub struct Info {
    pub version: u32,
    pub flags: u32,
    pub uapi_version: u32,
    pub state: u32,
}
/// egysk_module_rc_cmd in supercall.h.
#[repr(C, align(8))]
pub(super) struct Rc {
    pub ptr: u64,
    pub len: u32,
    pub reserved: u32,
}
/// ksu_set_sepolicy_cmd in supercall.h (retained upstream wire contract).
#[repr(C, align(8))]
pub(super) struct Policy {
    pub len: u64,
    pub ptr: u64,
}
/// egysk_get_sepolicy_cmd in supercall.h.
#[repr(C, align(8))]
pub(super) struct LivePolicy {
    pub ptr: u64,
    pub len: u64,
}
const _: () = {
    use std::mem::{align_of, offset_of, size_of};
    assert!(size_of::<Info>() == 16 && align_of::<Info>() == 4);
    assert!(offset_of!(Info, flags) == 4 && offset_of!(Info, uapi_version) == 8);
    assert!(offset_of!(Info, state) == 12);
    assert!(size_of::<Rc>() == 16 && align_of::<Rc>() == 8);
    assert!(offset_of!(Rc, len) == 8 && offset_of!(Rc, reserved) == 12);
    assert!(size_of::<Policy>() == 16 && align_of::<Policy>() == 8);
    assert!(offset_of!(Policy, ptr) == 8);
    assert!(size_of::<LivePolicy>() == 16 && align_of::<LivePolicy>() == 8);
    assert!(offset_of!(LivePolicy, len) == 8);
};

// KSU_SEPOLICY_CMD_* in selinux.h.
pub(super) const NORMAL_PERM: u32 = 1;
pub(super) const XPERM: u32 = 2;
pub(super) const TYPE_STATE: u32 = 3;
pub(super) const TYPE: u32 = 4;
pub(super) const TYPE_ATTR: u32 = 5;
pub(super) const ATTR: u32 = 6;
pub(super) const TYPE_TRANSITION: u32 = 7;
pub(super) const TYPE_CHANGE: u32 = 8;
pub(super) const GENFSCON: u32 = 9;
// KSU_SEPOLICY_SUBCMD_* values are scoped to their command, not interchangeable.
pub(super) const NORMAL_ALLOW: u32 = 1;
pub(super) const NORMAL_DENY: u32 = 2;
pub(super) const NORMAL_AUDITALLOW: u32 = 3;
pub(super) const NORMAL_DONTAUDIT: u32 = 4;
pub(super) const XPERM_ALLOW: u32 = 1;
pub(super) const XPERM_AUDITALLOW: u32 = 2;
pub(super) const XPERM_DONTAUDIT: u32 = 3;
pub(super) const STATE_PERMISSIVE: u32 = 1;
pub(super) const STATE_ENFORCE: u32 = 2;
pub(super) const CHANGE_CHANGE: u32 = 1;
pub(super) const CHANGE_MEMBER: u32 = 2;
pub(super) const NO_SUBCOMMAND: u32 = 0;

/// Policy command/subcommand/arity from the two UAPI headers. The textual
/// aliases are parser spellings for the same command, not product identities.
pub(super) fn policy_operation(name: &str) -> Option<(u32, u32, usize)> {
    Some(match name {
        "allow" => (NORMAL_PERM, NORMAL_ALLOW, 4),
        "deny" => (NORMAL_PERM, NORMAL_DENY, 4),
        "auditallow" => (NORMAL_PERM, NORMAL_AUDITALLOW, 4),
        "dontaudit" => (NORMAL_PERM, NORMAL_DONTAUDIT, 4),
        "allowxperm" => (XPERM, XPERM_ALLOW, 5),
        "auditallowxperm" => (XPERM, XPERM_AUDITALLOW, 5),
        "dontauditxperm" => (XPERM, XPERM_DONTAUDIT, 5),
        "permissive" => (TYPE_STATE, STATE_PERMISSIVE, 1),
        "enforce" => (TYPE_STATE, STATE_ENFORCE, 1),
        "type" => (TYPE, NO_SUBCOMMAND, 2),
        "typeattribute" | "attradd" => (TYPE_ATTR, NO_SUBCOMMAND, 2),
        "attribute" => (ATTR, NO_SUBCOMMAND, 1),
        "type_transition" | "name_transition" => (TYPE_TRANSITION, NO_SUBCOMMAND, 5),
        "type_change" => (TYPE_CHANGE, CHANGE_CHANGE, 4),
        "type_member" => (TYPE_CHANGE, CHANGE_MEMBER, 4),
        "genfscon" => (GENFSCON, NO_SUBCOMMAND, 3),
        _ => return None,
    })
}
