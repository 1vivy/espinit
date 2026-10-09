//! Keep the stock boot HAL from running beside ours.
//!
//! The module runs `esu-bootctl --stop-stock` as an `on post-fs` exec in the `esu` domain,
//! before `class_start early_hal` (`on late-fs`). It stops every init service whose command
//! carries the AOSP stock boot-HAL exec label; a not-yet-started service is disabled, so it
//! never starts. Selection is by SELinux label only: no service names or paths are stored.
//! Reading every partition's init scripts and sending `ctl.stop` are platform work, so the
//! confined `esu_bootctl` domain only serves: under an enforcing `esu_bootctl` the scan's
//! reads were refused by dontaudited rules and found nothing. The serving process never
//! waits on `ctl.stop`: at `late-fs` init's main thread can sit in `mount_all` while vold
//! waits on IBootControl, so a stop issued there deadlocks the boot. The module's
//! `sepolicy.rule` still denies the stock domain the registration as a backstop.

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

/// APEX services are imported before post-fs, alongside partition init scripts.
#[cfg(any(target_os = "android", test))]
fn rc_directories(apex: &Path) -> Vec<String> {
    let mut directories: Vec<String> = RC_DIRECTORIES.iter().map(|path| (*path).into()).collect();
    if let Ok(entries) = std::fs::read_dir(apex) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            for suffix in ["etc", "etc/init"] {
                if let Some(directory) = path.join(suffix).to_str() {
                    directories.push(directory.to_owned());
                }
            }
        }
    }
    directories
}

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
                if label_of(Path::new(command)).as_deref() == Some(label)
                    && !names.iter().any(|n| n == name)
                {
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
            // One write is one kmsg record; `writeln!` would split prefix and message.
            let _ = kmsg.write_all(format!("esu boot-hal: {line}\n").as_bytes());
        }
    }
}

/// Stop every stock boot-HAL service; logs each decision to the kernel log.
#[cfg(target_os = "android")]
pub fn stop_stock() {
    let directories = rc_directories(Path::new("/apex"));
    let borrowed: Vec<&str> = directories.iter().map(String::as_str).collect();
    let names = select(&borrowed, STOCK_LABEL, android::label_of);
    if names.is_empty() {
        android::log("no stock boot HAL service found");
    }
    for name in names {
        let stopped = android::stop(&name);
        android::log(&format!(
            "stop stock boot HAL service {name}: {}",
            if stopped { "ok" } else { "failed" }
        ));
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
        std::fs::write(
            root.join("boot.txt"),
            "service ignored /vendor/bin/hw/boot\n",
        )
        .unwrap();
        let directory = root.to_str().unwrap();
        let names = select(&[directory, "/nonexistent"], STOCK_LABEL, |path| {
            (path == Path::new("/vendor/bin/hw/boot")).then(|| STOCK_LABEL.to_vec())
        });
        assert_eq!(names, ["vendor.boot-qti"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn discovers_apex_rc_services_without_selecting_by_name() {
        let root = std::env::temp_dir().join(format!("esu-stock-apex-{}", std::process::id()));
        let directory = root.join("com.example.boot/etc");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("service.rc"),
            "service arbitrary.name /apex/com.example.boot/bin/hal\n\
             service misleading.boot /apex/com.example.boot/bin/other\n",
        )
        .unwrap();
        let directories = rc_directories(&root);
        let borrowed: Vec<&str> = directories.iter().map(String::as_str).collect();
        assert_eq!(
            select(&borrowed, STOCK_LABEL, |path| {
                (path == Path::new("/apex/com.example.boot/bin/hal")).then(|| STOCK_LABEL.to_vec())
            }),
            ["arbitrary.name"]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
