//! Keep the stock boot HAL from running beside ours.
//!
//! The module runs `esu-bootctl --stop-stock` as an `on post-fs` exec, before
//! `class_start early_hal` (`on late-fs`). It stops every init service whose command carries
//! the AOSP stock boot-HAL exec label; a not-yet-started service is disabled, so it never
//! starts. Selection is by SELinux label only: no service names or paths are stored. The
//! serving process never waits on `ctl.stop`: at `late-fs` init's main thread can sit in
//! `mount_all` while vold waits on IBootControl, so a stop issued there deadlocks the boot.
//! The module's `sepolicy.rule` still denies the stock domain the registration as a backstop.

use std::path::Path;

/// AOSP's exec label for the default boot-control HAL domain (`hal_bootctl_default`).
pub const STOCK_LABEL: &[u8] = b"u:object_r:hal_bootctl_default_exec:s0";

/// Directories init reads service definitions from (`LoadBootScripts`), plus `hw/`.
pub const RC_DIRECTORIES: [&str; 7] = [
    "/system/etc/init",
    "/system/etc/init/hw",
    "/system_ext/etc/init",
    "/vendor/etc/init",
    "/vendor/etc/init/hw",
    "/odm/etc/init",
    "/product/etc/init",
];

/// `(name, command)` of every `service <name> <command> ...` line in one rc file.
pub fn services(rc: &str) -> impl Iterator<Item = (&str, &str)> {
    rc.lines().filter_map(|line| {
        let mut words = line.split_whitespace();
        (words.next()? == "service").then_some(())?;
        Some((words.next()?, words.next()?))
    })
}

/// Names of the services in `directories` whose command `label_of` reports as `label`.
pub fn select(
    directories: &[&str],
    label: &[u8],
    label_of: impl Fn(&Path) -> Option<Vec<u8>>,
) -> Vec<String> {
    let mut names = Vec::new();
    for directory in directories {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "rc") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (name, command) in services(&text) {
                if label_of(Path::new(command)).as_deref() == Some(label) && !names.iter().any(|n| n == name) {
                    names.push(name.to_owned());
                }
            }
        }
    }
    names
}

#[cfg(target_os = "android")]
mod android {
    use std::ffi::{CString, c_char};
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    unsafe extern "C" {
        fn __system_property_set(name: *const c_char, value: *const c_char) -> i32;
    }

    /// The file's `security.selinux` value without the trailing NUL, if readable.
    pub fn label_of(path: &Path) -> Option<Vec<u8>> {
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut value = [0u8; 256];
        // SAFETY: both names are NUL-terminated; the kernel writes at most `value.len()` bytes.
        let length = unsafe {
            libc::getxattr(
                path.as_ptr(),
                c"security.selinux".as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        let length = usize::try_from(length).ok()?;
        let value = &value[..length];
        Some(value.strip_suffix(b"\0").unwrap_or(value).to_vec())
    }

    /// Ask init to stop one service; a service that has not started yet is disabled.
    pub fn stop(name: &str) -> bool {
        let Ok(name) = CString::new(name) else {
            return false;
        };
        // SAFETY: both arguments are NUL-terminated and live across the call.
        unsafe { __system_property_set(c"ctl.stop".as_ptr(), name.as_ptr()) == 0 }
    }

    pub fn log(line: &str) {
        if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
            let _ = writeln!(kmsg, "esu boot-hal: {line}");
        }
    }
}

/// Stop every stock boot-HAL service; logs each decision to the kernel log.
#[cfg(target_os = "android")]
pub fn stop_stock() {
    let names = select(&RC_DIRECTORIES, STOCK_LABEL, android::label_of);
    if names.is_empty() {
        android::log("no stock boot HAL service found");
    }
    for name in names {
        let stopped = android::stop(&name);
        android::log(&format!("stop stock boot HAL service {name}: {}", if stopped { "ok" } else { "failed" }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_only_services_whose_command_carries_the_label() {
        let root = std::env::temp_dir().join(format!("esu-stock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("boot.rc"),
            "# comment service vendor.fake /x\nservice vendor.boot-qti /vendor/bin/hw/boot\n    class early_hal\n\
             service other /vendor/bin/other\nservice\nservice lonely\n",
        )
        .unwrap();
        std::fs::write(root.join("boot.txt"), "service ignored /vendor/bin/hw/boot\n").unwrap();
        let directory = root.to_str().unwrap();
        let names = select(&[directory, "/nonexistent"], STOCK_LABEL, |path| {
            (path == Path::new("/vendor/bin/hw/boot")).then(|| STOCK_LABEL.to_vec())
        });
        assert_eq!(names, ["vendor.boot-qti"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
