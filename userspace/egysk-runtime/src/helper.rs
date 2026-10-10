//! Root-only helper ABI v4. No root-provider discovery, credentials or boot reports.
use anyhow::{Context, Result, ensure};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

mod abi;
pub use abi::Info;
use abi::*;
fn control() -> Result<OwnedFd> {
    crate::fsutil::root_only()?;
    let mut fd: libc::c_int = -1;
    // SAFETY: helper's retained reboot hook writes one int to this live pointer.
    unsafe {
        libc::syscall(libc::SYS_reboot, INSTALL_MAGIC1, INSTALL_MAGIC2, 0, &mut fd);
    }
    ensure!(fd >= 0, "egysk control FD unavailable");
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let target = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
    ensure!(
        target.to_string_lossy().contains(CONTROL_NAME),
        "foreign helper control FD"
    );
    Ok(fd)
}
pub fn info() -> Result<Info> {
    let fd = control()?;
    let mut info = Info::default();
    ensure!(
        unsafe { libc::ioctl(fd.as_raw_fd(), GET_INFO as _, &mut info) } == 0,
        "helper get-info: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        info.uapi_version == UAPI_VERSION && info.state & STATE_READY != 0,
        "helper ABI/readiness mismatch"
    );
    Ok(info)
}
pub fn set_module_rc(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= MODULE_RC_MAX_SIZE,
        "module RC exceeds 64 KiB"
    );
    let command = Rc {
        ptr: bytes.as_ptr() as u64,
        len: bytes.len() as u32,
        reserved: 0,
    };
    let fd = control()?;
    ensure!(
        unsafe { libc::ioctl(fd.as_raw_fd(), SET_MODULE_RC as _, &command) } == 0,
        "helper RC supply: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

/// Export the current policy, including its live Android netlink configuration.
///
/// The first ioctl queries a bounded capacity; only the second ioctl captures
/// policy bytes, coherently under the kernel policy mutex. Concurrent growth
/// beyond that capacity is an error, never a retry or an original-policy fallback.
pub fn live_policy() -> Result<Vec<u8>> {
    let fd = control()?;
    let mut command = LivePolicy { ptr: 0, len: 0 };
    ensure!(
        unsafe { libc::ioctl(fd.as_raw_fd(), GET_SEPOLICY as _, &mut command) } == 0,
        "helper live policy capacity: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        command.ptr == 0 && command.len > 0 && command.len <= LIVE_POLICY_MAX_SIZE as u64,
        "invalid helper live policy capacity"
    );
    let capacity = command.len as usize;
    let mut bytes = Vec::<u8>::new();
    bytes.try_reserve_exact(capacity)?;
    command.ptr = bytes.as_mut_ptr() as u64;
    ensure!(
        unsafe { libc::ioctl(fd.as_raw_fd(), GET_SEPOLICY as _, &mut command) } == 0,
        "helper live policy export: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        command.ptr == bytes.as_mut_ptr() as u64
            && command.len > 0
            && command.len <= capacity as u64,
        "invalid helper live policy length"
    );
    // SAFETY: successful GET_SEPOLICY initialized exactly the returned number
    // of bytes in this allocation; the validated length is within its capacity.
    unsafe { bytes.set_len(command.len as usize) };
    Ok(bytes)
}

/// Port of the existing sepolicy serializer: native-endian header followed by
/// [u32 byte length][bytes][NUL] per argument. Expansion is bounded *before*
/// allocating a Cartesian product, unlike the legacy unbounded flatten pass.
pub fn compile_policy(text: &str) -> Result<(Vec<u8>, usize)> {
    ensure!(
        text.len() <= 65536 && !text.contains('\0'),
        "module policy exceeds input bound"
    );
    let mut payload = Vec::new();
    let mut count = 0;
    for line in text.lines() {
        for statement in line.split('#').next().unwrap_or("").split(';') {
            let normalized = statement.replace('{', " { ").replace('}', " } ");
            let mut tokens = normalized.split_whitespace();
            let Some(op) = tokens.next() else { continue };
            let (cmd, subcmd, argc) =
                policy_operation(op).with_context(|| format!("unknown policy operation {op}"))?;
            let mut args: Vec<Vec<&str>> = Vec::new();
            while let Some(token) = tokens.next() {
                if token == "{" {
                    let mut values = Vec::new();
                    let mut closed = false;
                    for token in tokens.by_ref() {
                        if token == "}" {
                            closed = true;
                            break;
                        }
                        ensure!(token != "{", "nested policy set");
                        values.push(token);
                    }
                    ensure!(closed && !values.is_empty(), "unclosed/empty policy set");
                    args.push(values);
                } else {
                    ensure!(token != "}", "unmatched policy brace");
                    args.push(vec![token]);
                }
            }
            if cmd == TYPE && args.len() == 1 {
                args.push(vec!["domain"]);
            }
            if cmd == TYPE_TRANSITION && args.len() == 4 {
                args.push(vec![""]);
            }
            ensure!(args.len() == argc, "wrong argument count for {op}");
            let mut combinations = 1usize;
            for values in &args {
                combinations = combinations
                    .checked_mul(values.len())
                    .ok_or_else(|| anyhow::anyhow!("policy expansion overflow"))?;
                ensure!(
                    combinations <= 4096 && count + combinations <= 16384,
                    "policy expansion exceeds bound"
                );
                for value in values {
                    ensure!(
                        value.len() <= 255 && !value.contains(['{', '}', '\0']),
                        "invalid policy object"
                    );
                }
            }
            for index in 0..combinations {
                payload.extend_from_slice(&cmd.to_ne_bytes());
                payload.extend_from_slice(&subcmd.to_ne_bytes());
                let mut cursor = index;
                for values in &args {
                    let value = values[cursor % values.len()];
                    cursor /= values.len();
                    let value = if value == "*" { "" } else { value };
                    payload.extend_from_slice(&(value.len() as u32).to_ne_bytes());
                    payload.extend_from_slice(value.as_bytes());
                    payload.push(0);
                }
                ensure!(
                    payload.len() <= 1024 * 1024,
                    "serialized module policy exceeds bound"
                );
                count += 1;
            }
        }
    }
    Ok((payload, count))
}
pub fn apply_policy(text: &str) -> Result<()> {
    let (payload, count) = compile_policy(text)?;
    if count == 0 {
        return Ok(());
    }
    let command = Policy {
        len: payload.len() as u64,
        ptr: payload.as_ptr() as u64,
    };
    let fd = control()?;
    let applied = unsafe { libc::ioctl(fd.as_raw_fd(), SET_SEPOLICY as _, &command) };
    ensure!(
        applied >= 0,
        "helper policy application: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        applied as usize == count,
        "partial policy application: {applied}/{count}"
    );
    Ok(())
}
