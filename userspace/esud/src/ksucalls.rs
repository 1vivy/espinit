#![allow(clippy::unreadable_literal)]
use anyhow::{Result, bail};

use crate::ksu_uapi;
use std::cell::Cell;
use std::fs;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::LazyLock;

// sigsys handler
std::thread_local! {
    #[allow(clippy::missing_const_for_thread_local)]
    static SVC_IN_FLIGHT: Cell<bool> = const { Cell::new(false) };
    #[allow(clippy::missing_const_for_thread_local)]
    static SIGSYS_OCCURRED: Cell<bool> = const { Cell::new(false) };
}

const SYS_SECCOMP: libc::c_int = 1;

fn with_svc_call<F, R>(call: F) -> R
where
    F: FnOnce() -> R,
{
    SVC_IN_FLIGHT.with(|in_flight| in_flight.set(true));
    let result = call();
    SVC_IN_FLIGHT.with(|in_flight| in_flight.set(false));
    result
}

fn take_sigsys_occurred() -> bool {
    SIGSYS_OCCURRED.with(|occurred| occurred.replace(false))
}

extern "C" fn sigsys_handler(
    _sig: libc::c_int,
    info: *mut libc::siginfo_t,
    ctx: *mut libc::c_void,
) {
    unsafe {
        if info.is_null() || ctx.is_null() || (*info).si_code != SYS_SECCOMP {
            return;
        }
        if SVC_IN_FLIGHT.with(Cell::get) {
            SIGSYS_OCCURRED.with(|occurred| occurred.set(true));
        }

        #[cfg(not(target_arch = "riscv64"))]
        let ucontext = ctx.cast::<libc::ucontext_t>();
        #[cfg(target_arch = "aarch64")]
        {
            (*ucontext).uc_mcontext.regs[0] = (-libc::EPERM) as u64;
        }
        #[cfg(target_arch = "x86_64")]
        {
            let rax = libc::REG_RAX as usize;
            (*ucontext).uc_mcontext.gregs[rax] = i64::from(-libc::EPERM);
        }
        #[cfg(target_arch = "riscv64")]
        {
            let ucontext = ctx.cast::<ksu_uapi::ucontext_t>();
            (*ucontext).uc_mcontext.__gregs[ksu_uapi::REG_A0 as usize] =
                (-libc::EPERM) as libc::c_ulong;
        }
    }
}

pub fn setup_sigsys_handler() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_flags = libc::SA_SIGINFO;
        sa.sa_sigaction = sigsys_handler as *const () as usize;
        libc::sigemptyset(std::ptr::addr_of_mut!(sa.sa_mask));
        if libc::sigaction(libc::SIGSYS, std::ptr::addr_of!(sa), std::ptr::null_mut()) != 0 {
            let error = std::io::Error::last_os_error();
            log::warn!("Failed to set SIGSYS handler: {error}");
        }
    }
}

const DRIVER_FD_NAME: &str = "anon_inode:[esu]";

// Cached driver state. Both initializers are fixed and idempotent, so they
// live with the statics and run on first use: the control fd is installed once
// through the reboot hook, and the core identity is queried once through it.
static DRIVER_FD: LazyLock<RawFd> = LazyLock::new(|| init_driver_fd().unwrap_or(-1));
static INFO_CACHE: LazyLock<ksu_uapi::ksu_get_info_cmd> = LazyLock::new(query_info);

/// The daemon and the core module exchange exactly the v3 `ksu_get_info_cmd`
/// from `uapi/supercall.h`. Drift in either the 88-byte field layout or the
/// encoded ioctl number (`0x80584502`) fails this build instead of silently
/// reading a different structure, mirroring the PID-1 checks.
const _: () = assert!(std::mem::size_of::<ksu_uapi::ksu_get_info_cmd>() == 88);
const _: () = assert!(ksu_uapi::KSU_IOCTL_GET_INFO == 0x8058_4502);

fn scan_driver_fd() -> io::Result<Option<RawFd>> {
    let fd_dir = fs::read_dir("/proc/self/fd")?;
    let mut driver_fd = None;

    for entry in fd_dir.flatten() {
        if let Ok(fd_num) = entry.file_name().to_string_lossy().parse::<i32>() {
            let link_path = format!("/proc/self/fd/{fd_num}");
            if let Ok(target) = fs::read_link(&link_path) {
                let target_str = target.to_string_lossy();
                if target_str == DRIVER_FD_NAME {
                    driver_fd = Some(fd_num);
                }
            }
        }
    }

    Ok(driver_fd)
}

// Get cached driver fd
fn init_driver_fd() -> Option<RawFd> {
    let fd = scan_driver_fd().ok().flatten();
    if fd.is_none() {
        let mut fd = -1;
        with_svc_call(|| unsafe {
            libc::syscall(
                libc::SYS_reboot,
                ksu_uapi::ESU_INSTALL_MAGIC1,
                ksu_uapi::ESU_INSTALL_MAGIC2,
                0,
                &mut fd,
            )
        });
        if take_sigsys_occurred() {
            eprintln!("esu driver install syscall was blocked by seccomp");
            log::error!("esu driver install syscall was blocked by seccomp");
        }
        if fd >= 0 { Some(fd) } else { None }
    } else {
        fd
    }
}

/// Duplicate the validated control descriptor without `O_CLOEXEC` for one
/// tightly scoped helper process. The caller keeps this owned duplicate alive
/// through `Command::status`; the original cached descriptor remains private.
pub fn duplicate_driver_fd_for_child() -> Result<OwnedFd> {
    let fd = *DRIVER_FD;
    if fd < 0 {
        bail!("could not retrieve esu driver fd");
    }
    // SAFETY: `fd` is the process-owned cached descriptor. `F_DUPFD` returns a
    // new descriptor without FD_CLOEXEC, owned by the returned `OwnedFd`.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD, 3) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: `duplicate` is a fresh descriptor returned by `fcntl`.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

// ioctl wrapper using libc
fn ksuctl<T>(request: u32, arg: *mut T) -> Result<i32> {
    use std::io;

    let fd = *DRIVER_FD;
    if fd < 0 {
        bail!("could not retrieve esu driver fd")
    }
    unsafe {
        let ret = libc::ioctl(fd as libc::c_int, request as i32, arg);
        if ret < 0 {
            bail!("ksuctl failed: {}", io::Error::last_os_error())
        }
        Ok(ret)
    }
}

// API implementations

/// Query the core module once through the v3 get-info ioctl. The buffer starts
/// fully zeroed so a legacy reply can never leave newer fields
/// (`uapi_version`, `state`, `generation`, `boot_mode`) uninitialized.
fn query_info() -> ksu_uapi::ksu_get_info_cmd {
    let mut cmd = ksu_uapi::ksu_get_info_cmd {
        version: 0,
        flags: 0,
        features: 0,
        uapi_version: 0,
        state: 0,
        generation: [0; 64],
        boot_mode: 0,
    };
    if ksuctl(ksu_uapi::KSU_IOCTL_GET_INFO, &raw mut cmd).is_err() {
        // A core predating UAPI v2 answers only the zero-size request, and its
        // reply carries neither a version nor a generation; the readiness and
        // generation checks below reject that reply.
        let _ = ksuctl(ksu_uapi::KSU_IOCTL_GET_INFO_LEGACY, &raw mut cmd);
    }
    cmd
}

/// The core module identity reported by the last get-info query.
pub fn get_info() -> ksu_uapi::ksu_get_info_cmd {
    *INFO_CACHE
}

pub fn get_version() -> i32 {
    get_info().version as i32
}

pub fn is_lkm() -> bool {
    get_info().flags & ksu_uapi::KSU_GET_INFO_FLAG_LKM != 0
}

pub const fn uapi_version() -> u32 {
    ksu_uapi::ESU_UAPI_VERSION
}

/// Build generation compiled into this daemon by build.rs, from
/// `ESU_GENERATION` or the full Git HEAD hash of the esu repository.
/// It must equal the generation of the loaded core module.
pub const BUILD_GENERATION: &str = env!("ESU_GENERATION");

/// State bits reported by the core module through the last get-info query.
pub fn core_state() -> u32 {
    get_info().state
}

/// True when the core module reported `ESU_STATE_READY`, i.e. its normal
/// initialization completed.
pub fn is_core_ready() -> bool {
    core_state() & ksu_uapi::ESU_STATE_READY != 0
}

/// Build generation reported by the loaded core module, decoded from the
/// fixed-width NUL-terminated ASCII field. `generation[63]` is always NUL, so
/// a missing terminator or non-ASCII content means the core is not speaking
/// this ABI, and the result is `None`. Any remaining weakness in this decode
/// cannot admit an invalid generation: the caller accepts it only when it is
/// byte-identical to `BUILD_GENERATION`, which build.rs validates against the
/// `[A-Za-z0-9._-]{1,63}` build charset.
pub fn kernel_generation() -> Option<String> {
    let generation = &get_info().generation;
    let end = generation.iter().position(|&byte| byte == 0)?;
    let bytes = &generation[..end];
    if !bytes.is_ascii() {
        return None;
    }
    Some(bytes.iter().map(|&byte| char::from(byte)).collect())
}

pub fn runtime_mode() -> &'static str {
    if is_lkm() { "module" } else { "built-in" }
}

/// Verify that the loaded core module matches this daemon before any operation
/// relies on it: same UAPI, finished initialization and identical generation.
pub fn ensure_uapi_version_matched() -> anyhow::Result<()> {
    let info = get_info();
    let kernel_uapi = info.uapi_version;
    let userspace_uapi = uapi_version();
    if kernel_uapi != userspace_uapi {
        bail!(
            "UAPI version mismatch: kernel={kernel_uapi}, esud={userspace_uapi}. Please update esu!"
        );
    }

    if !is_core_ready() {
        bail!(
            "esu core is not ready: get-info reported state=0x{:x} without the READY bit. Load the \
             matching esu module to completion before the daemon runs.",
            info.state
        );
    }

    let Some(kernel_generation) = kernel_generation() else {
        bail!(
            "esu core reported an invalid build generation: the get-info field is not \
             NUL-terminated ASCII. Build and load a core module that implements UAPI v3."
        );
    };
    if kernel_generation.is_empty() {
        bail!(
            "esu core reported an empty build generation. Build the core module with \
             ESU_GENERATION or from a Git checkout and reinstall the matching payload."
        );
    }
    if kernel_generation != BUILD_GENERATION {
        bail!(
            "esu generation mismatch: kernel={kernel_generation}, esud={BUILD_GENERATION}. \
             Install a payload whose core module and daemon share one generation."
        );
    }

    Ok(())
}

fn report_event(event: u32) {
    let mut cmd = ksu_uapi::ksu_report_event_cmd { event };
    let _ = ksuctl(ksu_uapi::KSU_IOCTL_REPORT_EVENT, &raw mut cmd);
}

pub fn report_post_fs_data() {
    report_event(ksu_uapi::EVENT_POST_FS_DATA);
}

pub fn report_services() -> Result<bool> {
    let mut cmd = ksu_uapi::ksu_report_event_cmd {
        event: ksu_uapi::EVENT_SERVICES,
    };
    Ok(ksuctl(ksu_uapi::KSU_IOCTL_REPORT_EVENT, &raw mut cmd)? == 1)
}

pub fn report_boot_complete() {
    report_event(ksu_uapi::EVENT_BOOT_COMPLETED);
}

pub fn report_module_mounted() {
    report_event(ksu_uapi::EVENT_MODULE_MOUNTED);
}

pub fn check_kernel_safemode() -> bool {
    let mut cmd = ksu_uapi::ksu_check_safemode_cmd { in_safe_mode: 0 };
    let _ = ksuctl(ksu_uapi::KSU_IOCTL_CHECK_SAFEMODE, &raw mut cmd);
    cmd.in_safe_mode != 0
}

pub fn set_sepolicy(payload: *const u8, payload_len: u64) -> Result<i32> {
    let mut ioctl_cmd = crate::ksu_uapi::ksu_set_sepolicy_cmd {
        data_len: payload_len,
        data: payload as u64,
    };

    ksuctl(ksu_uapi::KSU_IOCTL_SET_SEPOLICY, &raw mut ioctl_cmd)
}

/// Get feature value and support status from kernel
/// Returns (value, supported)
pub fn get_feature(feature_id: u32) -> Result<(u64, bool)> {
    let mut cmd = ksu_uapi::ksu_get_feature_cmd {
        feature_id,
        value: 0,
        supported: 0,
    };
    ksuctl(ksu_uapi::KSU_IOCTL_GET_FEATURE, &raw mut cmd)?;
    Ok((cmd.value, cmd.supported != 0))
}

/// Set feature value in kernel
pub fn set_feature(feature_id: u32, value: u64) -> Result<()> {
    let mut cmd = ksu_uapi::ksu_set_feature_cmd { feature_id, value };
    ksuctl(ksu_uapi::KSU_IOCTL_SET_FEATURE, &raw mut cmd)?;
    Ok(())
}

/// Get mark status for a process (pid=0 returns total marked count)
pub fn mark_get(pid: i32) -> Result<u32> {
    let mut cmd = ksu_uapi::ksu_manage_mark_cmd {
        operation: ksu_uapi::KSU_MARK_GET,
        pid,
        result: 0,
    };
    ksuctl(ksu_uapi::KSU_IOCTL_MANAGE_MARK, &raw mut cmd)?;
    Ok(cmd.result)
}

/// Mark a process (pid=0 marks all processes)
pub fn mark_set(pid: i32) -> Result<()> {
    let mut cmd = ksu_uapi::ksu_manage_mark_cmd {
        operation: ksu_uapi::KSU_MARK_MARK,
        pid,
        result: 0,
    };
    ksuctl(ksu_uapi::KSU_IOCTL_MANAGE_MARK, &raw mut cmd)?;
    Ok(())
}

/// Unmark a process (pid=0 unmarks all processes)
pub fn mark_unset(pid: i32) -> Result<()> {
    let mut cmd = ksu_uapi::ksu_manage_mark_cmd {
        operation: ksu_uapi::KSU_MARK_UNMARK,
        pid,
        result: 0,
    };
    ksuctl(ksu_uapi::KSU_IOCTL_MANAGE_MARK, &raw mut cmd)?;
    Ok(())
}

/// Refresh mark for all running processes
pub fn mark_refresh() -> Result<()> {
    let mut cmd = ksu_uapi::ksu_manage_mark_cmd {
        operation: ksu_uapi::KSU_MARK_REFRESH,
        pid: 0,
        result: 0,
    };
    ksuctl(ksu_uapi::KSU_IOCTL_MANAGE_MARK, &raw mut cmd)?;
    Ok(())
}

pub fn nuke_ext4_sysfs(mnt: &str) -> anyhow::Result<()> {
    let c_mnt = std::ffi::CString::new(mnt)?;
    let mut ioctl_cmd = ksu_uapi::ksu_nuke_ext4_sysfs_cmd {
        arg: c_mnt.as_ptr() as u64,
    };
    ksuctl(ksu_uapi::KSU_IOCTL_NUKE_EXT4_SYSFS, &raw mut ioctl_cmd)?;
    Ok(())
}

/// Wipe all entries from umount list
pub fn umount_list_wipe() -> Result<()> {
    let mut cmd = ksu_uapi::ksu_add_try_umount_cmd {
        arg: 0,
        flags: 0,
        mode: ksu_uapi::KSU_UMOUNT_WIPE,
    };
    ksuctl(ksu_uapi::KSU_IOCTL_ADD_TRY_UMOUNT, &raw mut cmd)?;
    Ok(())
}

/// Add mount point to umount list
pub fn umount_list_add(path: &str, flags: u32) -> anyhow::Result<()> {
    let c_path = std::ffi::CString::new(path)?;
    let mut cmd = ksu_uapi::ksu_add_try_umount_cmd {
        arg: c_path.as_ptr() as u64,
        flags,
        mode: ksu_uapi::KSU_UMOUNT_ADD,
    };
    ksuctl(ksu_uapi::KSU_IOCTL_ADD_TRY_UMOUNT, &raw mut cmd)?;
    Ok(())
}

/// Delete mount point from umount list
pub fn umount_list_del(path: &str) -> anyhow::Result<()> {
    let c_path = std::ffi::CString::new(path)?;
    let mut cmd = ksu_uapi::ksu_add_try_umount_cmd {
        arg: c_path.as_ptr() as u64,
        flags: 0,
        mode: ksu_uapi::KSU_UMOUNT_DEL,
    };
    ksuctl(ksu_uapi::KSU_IOCTL_ADD_TRY_UMOUNT, &raw mut cmd)?;
    Ok(())
}

/// Set current process's process group to init_group (pgid = 0)
pub fn set_init_pgrp() -> Result<()> {
    ksuctl(
        ksu_uapi::KSU_IOCTL_SET_INIT_PGRP,
        std::ptr::null_mut::<u8>(),
    )?;
    Ok(())
}
