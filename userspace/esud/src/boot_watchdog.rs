//! Lab-only stalled-boot watchdog, armed from bootconfig at the Early stage.
//!
//! A boot that waits forever never panics, so the lab's panic instruments
//! (`init_fatal_panic`, ramoops, APSS minidump) never fire. With
//! `androidboot.esu.boot_watchdog=<seconds>` a detached process outside
//! init's service cgroup waits for `sys.boot_completed`. On the deadline it
//! puts blocked-task stacks into the kernel log, then either panics (when
//! `androidboot.init_fatal_panic=true`, handing over to those instruments) or
//! writes a size-capped dump under `/metadata/esu/log/hang/` and reboots.
//! No product launcher sets the key.

#![cfg_attr(not(target_os = "android"), allow(dead_code))]

/// Bootconfig key that arms the watchdog with its deadline in seconds.
pub const WATCHDOG_KEY: &str = "androidboot.esu.boot_watchdog";
/// AOSP's opt-in that turns a fatal boot failure into a kernel panic.
pub const FATAL_PANIC_KEY: &str = "androidboot.init_fatal_panic";

/// The value of one `key = "value"` bootconfig line, exactly matched.
pub fn bootconfig_value<'a>(bootconfig: &'a str, key: &str) -> Option<&'a str> {
    bootconfig.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim().trim_matches('"'))
    })
}

/// The watchdog deadline, accepted only as 30..=3600 seconds.
pub fn deadline_seconds(bootconfig: &str) -> Option<u64> {
    bootconfig_value(bootconfig, WATCHDOG_KEY)?
        .parse()
        .ok()
        .filter(|seconds| (30..=3600).contains(seconds))
}

/// Keep at most `limit` trailing bytes of a capture.
pub fn tail(bytes: &[u8], limit: usize) -> &[u8] {
    &bytes[bytes.len().saturating_sub(limit)..]
}

#[cfg(target_os = "android")]
mod android {
    use super::{FATAL_PANIC_KEY, bootconfig_value, deadline_seconds, tail};
    use anyhow::{Context, Result};
    use log::{info, warn};
    use std::fs::{self, File, OpenOptions};
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn bootconfig() -> String {
        fs::read_to_string("/proc/bootconfig").unwrap_or_default()
    }

    fn sysrq(key: &[u8]) {
        if let Ok(mut trigger) = OpenOptions::new().write(true).open("/proc/sysrq-trigger") {
            let _ = trigger.write_all(key);
        }
    }

    /// Start the watchdog when this boot's bootconfig asks for it. Never fails
    /// the boot.
    pub fn arm() {
        let Some(seconds) = deadline_seconds(&bootconfig()) else {
            return;
        };
        let spawned = std::env::current_exe().and_then(|exe| {
            // SAFETY: the pre_exec closure runs between fork and exec and only
            // calls switch_cgroups, which writes this child's pid into cgroup
            // files; it touches no state shared with the parent.
            unsafe {
                Command::new(exe)
                    .arg("boot-watchdog")
                    .arg(seconds.to_string())
                    .process_group(0)
                    .pre_exec(|| {
                        crate::utils::switch_cgroups();
                        Ok(())
                    })
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
            }
        });
        match spawned {
            Ok(_) => info!("lab: boot watchdog armed for {seconds}s"),
            Err(error) => warn!("lab: cannot start boot watchdog: {error}"),
        }
    }

    fn capture(dir: &Path, name: &str, limit: usize, program: &str, args: &[&str]) {
        let bytes = match Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()
        {
            Ok(output) => [output.stdout, output.stderr].concat(),
            Err(error) => format!("cannot run {program}: {error}\n").into_bytes(),
        };
        if let Ok(mut file) = File::create(dir.join(name)) {
            let _ = file.write_all(tail(&bytes, limit));
            let _ = file.sync_all();
        }
    }

    /// Wait for boot completion; on the deadline, record the stall and end the boot.
    pub fn run(seconds: u64) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            if crate::utils::getprop("sys.boot_completed").as_deref() == Some("1") {
                return Ok(());
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        // Blocked (D-state) task stacks go into the kernel log either way.
        sysrq(b"w");
        if bootconfig_value(&bootconfig(), FATAL_PANIC_KEY) == Some("true") {
            sysrq(b"c");
            return Ok(());
        }
        // /data logs are available only after post-fs-data created the root.
        if !Path::new(crate::defs::LOG_DIR).is_dir() {
            sysrq(b"b");
            return Ok(());
        }
        let dir = Path::new(crate::defs::LOG_DIR).join("hang");
        fs::create_dir(&dir)
            .or_else(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .context("create hang dump directory")?;
        capture(
            &dir,
            "ps.txt",
            256 << 10,
            "ps",
            &["-A", "-T", "-o", "PID,TID,PPID,S,WCHAN,CMDLINE"],
        );
        capture(&dir, "getprop.txt", 256 << 10, "getprop", &[]);
        capture(&dir, "dmesg.txt", 1 << 20, "dmesg", &[]);
        capture(
            &dir,
            "logcat.txt",
            2 << 20,
            "timeout",
            &["10", "logcat", "-d", "-b", "all"],
        );
        if let Ok(handle) = File::open(&dir) {
            let _ = handle.sync_all();
        }
        sysrq(b"s");
        sysrq(b"b");
        Ok(())
    }
}

#[cfg(target_os = "android")]
pub use android::{arm, run};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_deadline_key_is_matched_exactly_and_bounded() {
        let config = "androidboot.esu.boot_watchdog_x = \"60\"\n\
            androidboot.esu.boot_watchdog = \"90\"\n";
        assert_eq!(deadline_seconds(config), Some(90));
        assert_eq!(
            deadline_seconds("androidboot.esu.boot_watchdog = \"5\""),
            None
        );
        assert_eq!(
            deadline_seconds("androidboot.esu.boot_watchdog = \"x\""),
            None
        );
        assert_eq!(deadline_seconds(""), None);
        assert_eq!(
            bootconfig_value("androidboot.init_fatal_panic = \"true\"", FATAL_PANIC_KEY),
            Some("true")
        );
    }

    #[test]
    fn captures_keep_the_tail() {
        assert_eq!(tail(b"abcdef", 3), b"def");
        assert_eq!(tail(b"ab", 3), b"ab");
    }
}
