// SPDX-License-Identifier: GPL-3.0-only
use crate::{Contents, Plan, open_file, open_root};
use anyhow::{Context, Result, ensure};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

const STAGING: &str = ".esu-staging";
const RETIRED: &str = ".esu-retired";
const COMPLETE: &str = ".esu-complete";
const COMPLETE_BYTES: &[u8] = b"esu-runtime-v1\n";
const MOUNT: &str = "/esu-metadata";
const NODE: &str = "/dev/esu-metadata";

pub fn label(file: &File, context: &str) -> Result<()> {
    let context = CString::new(context)?;
    // SAFETY: both buffers and the inode descriptor are live for the call.
    let result = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            c"security.selinux".as_ptr(),
            context.as_ptr().cast(),
            context.as_bytes_with_nul().len(),
            0,
        )
    };
    ensure!(
        result == 0,
        "inode label failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn directory(path: &Path, labels: bool) -> Result<()> {
    fs::create_dir(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    if labels {
        label(&File::open(path)?, "u:object_r:metadata_file:s0")?;
    }
    Ok(())
}

// Runtime data is not an executable payload and is not generation-replaced.
// Preserve only the existing state conventions, never old modules, rc or bins.
fn retain_state(source: &Path, destination: &Path, labels: bool) -> Result<()> {
    let info = fs::symlink_metadata(source)?;
    if info.is_dir() {
        directory(destination, labels)?;
        fs::set_permissions(destination, info.permissions())?;
        let mut entries: Vec<_> = fs::read_dir(source)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            retain_state(&entry.path(), &destination.join(entry.file_name()), labels)?;
        }
        File::open(destination)?.sync_all()?;
    } else {
        ensure!(info.is_file(), "non-regular persistent state inode");
        // Same filesystem: retain data without copying potentially large logs.
        fs::hard_link(source, destination)?;
        File::open(destination)?.sync_all()?;
    }
    Ok(())
}

fn require_complete(path: &Path) -> Result<()> {
    let root = open_root(path)?;
    let mut marker = open_file(&root, COMPLETE)?;
    ensure!(
        marker.metadata()?.len() == COMPLETE_BYTES.len() as u64,
        "invalid completion marker"
    );
    let mut bytes = [0; COMPLETE_BYTES.len()];
    marker.read_exact(&mut bytes)?;
    ensure!(
        bytes.as_slice() == COMPLETE_BYTES,
        "invalid completion marker"
    );
    Ok(())
}

fn cleanup_retired(metadata: &Path) -> Result<()> {
    let retired = metadata.join(RETIRED);
    match fs::symlink_metadata(&retired) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(info) => ensure!(info.is_dir(), "retired snapshot is not a directory"),
    }
    // Never delete the only remaining complete snapshot.
    require_complete(&metadata.join("esu"))?;
    let mut entries = fs::read_dir(&retired)?.peekable();
    if entries.peek().is_some() {
        require_complete(&retired)?;
        for entry in entries {
            let entry = entry?;
            if entry.file_name() == COMPLETE {
                continue;
            }
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                // remove_file unlinks a symlink rather than following it.
                fs::remove_file(entry.path())?;
            }
        }
        File::open(&retired)?.sync_all()?;
        fs::remove_file(retired.join(COMPLETE))?;
        File::open(&retired)?.sync_all()?;
    }
    // Empty means cleanup reached marker removal before the previous crash.
    fs::remove_dir(&retired)?;
    File::open(metadata)?.sync_all()?;
    Ok(())
}

/// Replace a complete runtime snapshot in one rename/exchange. A leftover
/// staging tree is deliberately rejected, never guessed complete or resumed.
/// No existing path is followed while installing the newly planned snapshot.
pub fn publish(metadata: &Path, mut plan: Plan, labels: bool) -> Result<()> {
    let parent = open_root(metadata)?;
    let stage = metadata.join(STAGING);
    let target = metadata.join("esu");
    let existed = match fs::symlink_metadata(&target) {
        Ok(info) => {
            ensure!(info.is_dir(), "existing esu root is not a directory");
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    match fs::symlink_metadata(&stage) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(_) => anyhow::bail!("unresolved esu staging transaction"),
    }
    if existed {
        require_complete(&target)?;
    }
    cleanup_retired(metadata)?;
    // create_dir rejects stale partial publications and symbolic links alike.
    directory(&stage, labels).context("unresolved esu staging transaction")?;
    let result = (|| {
        let mut directories = std::collections::BTreeSet::new();
        for name in ["bin", "modules", "initrc"] {
            let path = stage.join(name);
            directory(&path, labels)?;
            directories.insert(path);
        }
        if existed {
            for name in ["log", "receipts", "module_configs", ".feature_config"] {
                let source = target.join(name);
                match fs::symlink_metadata(&source) {
                    Ok(_) => retain_state(&source, &stage.join(name), labels)?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                    Err(error) => return Err(error.into()),
                }
            }
        }
        for entry in &mut plan.files {
            crate::relative(&entry.destination)?;
            let path = stage.join(&entry.destination);
            let relative_parent = Path::new(&entry.destination).parent().unwrap();
            let mut current = stage.clone();
            for part in relative_parent.components() {
                current.push(part);
                if directories.insert(current.clone()) {
                    directory(&current, labels)?;
                }
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .mode(entry.mode)
                .open(&path)?;
            // Do not let the inherited PID-1 umask change the package mode.
            file.set_permissions(fs::Permissions::from_mode(entry.mode))?;
            match &mut entry.contents {
                Contents::Source(source) => {
                    source.seek(SeekFrom::Start(0))?;
                    let expected = source.metadata()?.len();
                    ensure!(
                        std::io::copy(source, &mut file)? == expected,
                        "short package copy"
                    );
                }
                Contents::Generated(data) => file.write_all(data)?,
            }
            if labels {
                label(
                    &file,
                    if entry.destination == "esud" {
                        "u:object_r:esu_file:s0"
                    } else {
                        "u:object_r:metadata_file:s0"
                    },
                )?;
            }
            file.sync_all()?;
        }
        // Deepest directories first, then the staging root, before publication.
        let mut directories: Vec<_> = directories.into_iter().collect();
        directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for path in directories {
            File::open(path)?.sync_all()?;
        }
        let marker = stage.join(COMPLETE);
        let mut completion = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(marker)?;
        completion.write_all(COMPLETE_BYTES)?;
        if labels {
            label(&completion, "u:object_r:metadata_file:s0")?;
        }
        completion.sync_all()?;
        File::open(&stage)?.sync_all()?;
        let from = CString::new(STAGING)?;
        let to = c"esu";
        // SAFETY: both names are single components under a held directory fd.
        // NOREPLACE for first install, EXCHANGE for an existing complete tree.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                parent.as_raw_fd(),
                from.as_ptr(),
                parent.as_raw_fd(),
                to.as_ptr(),
                if existed {
                    libc::RENAME_EXCHANGE
                } else {
                    libc::RENAME_NOREPLACE
                },
            )
        };
        ensure!(
            result == 0,
            "atomic publication failed: {}",
            std::io::Error::last_os_error()
        );
        if existed {
            // Name the old complete snapshot as cleanup-only before the parent
            // fsync. A real .esu-staging tree always remains fatal.
            let retired = CString::new(RETIRED)?;
            // SAFETY: live parent fd and terminated sibling names, with no replacement.
            let result = unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    parent.as_raw_fd(),
                    from.as_ptr(),
                    parent.as_raw_fd(),
                    retired.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            ensure!(
                result == 0,
                "retiring old snapshot failed: {}",
                std::io::Error::last_os_error()
            );
        }
        parent.sync_all()?;
        cleanup_retired(metadata)?;
        Ok(())
    })();
    // Failed writes leave the incomplete tree for diagnosis. No next boot may
    // publish it or silently use the previous generation; PID1 stops on error.
    result
}

/// Mount the verified metadata dev_t: projected for managed boot, the exact
/// native metadata partition for unmanaged boot. Always unmount before handoff.
pub fn mount_and_publish(device: libc::dev_t, filesystem: &str, plan: Plan) -> Result<()> {
    ensure!(
        matches!(filesystem, "ext4" | "f2fs"),
        "unsupported metadata filesystem"
    );
    fs::create_dir(MOUNT)?;
    let node = CString::new(NODE)?;
    // SAFETY: fixed pathname and the exact sysfs-validated metadata dev_t.
    if unsafe { libc::mknod(node.as_ptr(), libc::S_IFBLK | 0o600, device) } != 0 {
        let error = std::io::Error::last_os_error();
        fs::remove_dir(MOUNT)?;
        return Err(error.into());
    }
    let target = CString::new(MOUNT)?;
    let filesystem = CString::new(filesystem)?;
    let mut mounted = false;
    let result = (|| {
        // SAFETY: live strings; no filesystem-specific mount data is supplied.
        let result = unsafe {
            libc::mount(
                node.as_ptr(),
                target.as_ptr(),
                filesystem.as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                std::ptr::null(),
            )
        };
        ensure!(
            result == 0,
            "projected metadata mount: {}",
            std::io::Error::last_os_error()
        );
        mounted = true;
        // SAFETY: convert this mount only to private propagation.
        let result = unsafe {
            libc::mount(
                std::ptr::null(),
                target.as_ptr(),
                std::ptr::null(),
                libc::MS_PRIVATE,
                std::ptr::null(),
            )
        };
        ensure!(
            result == 0,
            "metadata private mount: {}",
            std::io::Error::last_os_error()
        );
        publish(Path::new(MOUNT), plan, true)
    })();
    if mounted {
        // SAFETY: unmount the one owned mount, without MNT_DETACH.
        ensure!(
            unsafe { libc::umount2(target.as_ptr(), 0) } == 0,
            "metadata unmount failed: {}",
            std::io::Error::last_os_error()
        );
    }
    fs::remove_file(NODE)?;
    fs::remove_dir(MOUNT)?;
    result
}
