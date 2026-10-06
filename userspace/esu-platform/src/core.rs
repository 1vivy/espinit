use anyhow::{Context, Result, ensure};
use std::fs;
use std::os::fd::RawFd;

const DRIVER_FD_NAME: &str = "anon_inode:[esu]";
const IOCTL_GET_INFO: u32 = 0x8058_4502;

/// Exact v3 `ksu_get_info_cmd` layout from `uapi/supercall.h`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CoreInfo {
    pub version: u32,
    pub flags: u32,
    pub features: u32,
    pub uapi_version: u32,
    pub state: u32,
    pub generation: [u8; 64],
    pub boot_mode: u32,
}

impl Default for CoreInfo {
    fn default() -> Self {
        Self {
            version: 0,
            flags: 0,
            features: 0,
            uapi_version: 0,
            state: 0,
            generation: [0; 64],
            boot_mode: 0,
        }
    }
}

const _: () = assert!(std::mem::size_of::<CoreInfo>() == 88);

/// Query the control descriptor inherited from esud.
///
/// The helper cannot install a replacement descriptor: PID 1 selects the mode,
/// esud validates the core, and this child independently reads that same
/// kernel-owned state before changing labels or mounts.
pub fn query_inherited_core_info() -> Result<CoreInfo> {
    let fd = inherited_driver_fd()?;
    let mut info = CoreInfo::default();
    // SAFETY: `fd` names the inherited esu anon inode and `info` exactly
    // matches the v3 writable ioctl payload.
    let result = unsafe { libc::ioctl(fd, IOCTL_GET_INFO as _, std::ptr::addr_of_mut!(info)) };
    if result < 0 {
        return Err(std::io::Error::last_os_error()).context("esu get-info ioctl");
    }
    Ok(info)
}

fn inherited_driver_fd() -> Result<RawFd> {
    let mut found = None;
    for entry in fs::read_dir("/proc/self/fd").context("read inherited descriptors")? {
        let entry = entry?;
        let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        let target = fs::read_link(entry.path())?;
        if target.to_string_lossy() != DRIVER_FD_NAME {
            continue;
        }
        ensure!(
            found.replace(fd).is_none(),
            "multiple esu control descriptors inherited"
        );
    }
    found.ok_or_else(|| anyhow::anyhow!("esu control descriptor was not inherited"))
}
