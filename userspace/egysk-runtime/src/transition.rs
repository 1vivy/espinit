//! Explicit, offline product-root transition. Never called by managed boot.
//!
//! The installer must stop all consumers and expose the physical backing in its
//! maintenance namespace before invoking this API. A namespace-local mount check
//! cannot prove other namespaces are idle; `--offline` is an operator assertion,
//! not an automatic quiescing mechanism. Credentials are siblings, not migration
//! inputs. No recursive copy, merge, relabelling or credential inspection occurs.
use anyhow::{Context, Result, ensure};
use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const LEGACY_SUBTREE: &str = "kernelsu-esp";
const PRODUCT_SUBTREE: &str = "egysk";

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Transitioned,
    AlreadyTransitioned,
}

/// Rename the existing product directory in place without replacing any target.
/// The caller explicitly guarantees stopped consumers across all namespaces.
/// No invocation during boot or automatic fallback to a fresh store is allowed.
pub fn offline_product_state(backing_root: &Path) -> Result<Outcome> {
    crate::fsutil::root_only()?;
    ensure!(
        // SAFETY: getpid takes no pointers and only reads the calling process ID.
        unsafe { libc::getpid() } != 1,
        "transition is not a PID1 operation"
    );
    for root in [crate::context::ROOT, "/dev/kernelsu-esp"] {
        ensure!(
            fs::symlink_metadata(root).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "product runtime exists; stop consumers in an offline maintenance environment"
        );
    }
    check_mounts(&fs::read_to_string("/proc/self/mountinfo")?)?;
    transition_at(backing_root)
}

fn check_mounts(mountinfo: &str) -> Result<()> {
    for line in mountinfo.lines() {
        // Includes bind roots and OverlayFS lowerdir/upperdir/workdir references,
        // not just mountpoints. Refuse both identities rather than detach them.
        let referenced = line.split([' ', ',', ':', '=']).any(|field| {
            Path::new(field).components().any(|component| {
                component.as_os_str() == LEGACY_SUBTREE || component.as_os_str() == PRODUCT_SUBTREE
            })
        });
        ensure!(
            !referenced,
            "product storage has live mount references; refusing transition"
        );
    }
    Ok(())
}

fn directory_if_present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "product subtree is not a physical directory: {}",
                path.display()
            );
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("inspect product subtree"),
    }
}

fn transition_at(backing_root: &Path) -> Result<Outcome> {
    ensure!(
        backing_root.is_absolute() && fs::canonicalize(backing_root)? == backing_root,
        "backing root must be an absolute physical path without symlinks"
    );
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(backing_root)?;
    ensure!(
        // SAFETY: root owns this open directory FD for the entire call; flock
        // takes no pointers, and LOCK_EX | LOCK_NB is a valid operation.
        unsafe { libc::flock(root.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "another product transition owns the backing root: {}",
        std::io::Error::last_os_error()
    );
    let old = directory_if_present(&backing_root.join(LEGACY_SUBTREE))?;
    let new = directory_if_present(&backing_root.join(PRODUCT_SUBTREE))?;
    ensure!(
        !(old && new),
        "both product subtrees exist; refusing to merge or discard either"
    );
    ensure!(old || new, "no existing product state to transition");
    if new {
        root.sync_all()?;
        return Ok(Outcome::AlreadyTransitioned);
    }
    // One same-filesystem atomic rename preserves inodes, xattrs, generations,
    // journals, module edits and state. RENAME_NOREPLACE also closes the target
    // collision race; unsupported filesystems fail closed, without a copy fallback.
    // SAFETY: both directory FDs are borrowed from the live root File; both
    // relative names are static NUL-terminated C strings. The argument order
    // matches Linux renameat2, and RENAME_NOREPLACE is a supported ABI flag.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            root.as_raw_fd(),
            c"kernelsu-esp".as_ptr(),
            root.as_raw_fd(),
            c"egysk".as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("rename offline product subtree");
    }
    root.sync_all().context("persist product-root transition")?;
    Ok(Outcome::Transitioned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn transition_preserves_inodes_modules_journals_and_credential_siblings() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join(LEGACY_SUBTREE);
        fs::create_dir_all(old.join("modules/example")).unwrap();
        fs::create_dir_all(old.join("activations")).unwrap();
        fs::create_dir_all(old.join("state/example")).unwrap();
        fs::create_dir_all(root.path().join("password_slots")).unwrap();
        fs::write(root.path().join("password_slots/slot_map"), b"1=gsi2\n").unwrap();
        fs::write(old.join("modules/example/.esp-generation"), "ab".repeat(32)).unwrap();
        fs::write(old.join("activations/example.json"), b"pending").unwrap();
        fs::write(old.join("state/example/settings"), b"retained").unwrap();
        fs::set_permissions(
            old.join("state/example/settings"),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        std::os::unix::fs::symlink("settings", old.join("state/example/link")).unwrap();
        let inode = fs::metadata(old.join("state/example/settings"))
            .unwrap()
            .ino();
        assert_eq!(transition_at(root.path()).unwrap(), Outcome::Transitioned);
        let new = root.path().join(PRODUCT_SUBTREE);
        assert!(!old.exists());
        assert_eq!(
            fs::metadata(new.join("state/example/settings"))
                .unwrap()
                .ino(),
            inode
        );
        assert_eq!(
            fs::metadata(new.join("state/example/settings"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(
            fs::read(new.join("state/example/settings")).unwrap(),
            b"retained"
        );
        assert_eq!(
            fs::read_link(new.join("state/example/link")).unwrap(),
            Path::new("settings")
        );
        assert_eq!(
            fs::read(new.join("activations/example.json")).unwrap(),
            b"pending"
        );
        assert_eq!(
            fs::read_to_string(new.join("modules/example/.esp-generation")).unwrap(),
            "ab".repeat(32)
        );
        assert_eq!(
            fs::read(root.path().join("password_slots/slot_map")).unwrap(),
            b"1=gsi2\n"
        );
        assert_eq!(
            transition_at(root.path()).unwrap(),
            Outcome::AlreadyTransitioned
        );
    }

    #[test]
    fn missing_collision_and_symlink_targets_fail_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        assert!(transition_at(root.path()).is_err());
        let old = root.path().join(LEGACY_SUBTREE);
        let new = root.path().join(PRODUCT_SUBTREE);
        fs::create_dir(&old).unwrap();
        fs::write(old.join("state"), b"old").unwrap();
        fs::create_dir(&new).unwrap();
        fs::write(new.join("state"), b"new").unwrap();
        assert!(transition_at(root.path()).is_err());
        assert_eq!(fs::read(old.join("state")).unwrap(), b"old");
        assert_eq!(fs::read(new.join("state")).unwrap(), b"new");
        fs::remove_file(new.join("state")).unwrap();
        fs::remove_dir(&new).unwrap();
        std::os::unix::fs::symlink("missing", &new).unwrap();
        assert!(transition_at(root.path()).is_err());
        assert!(old.exists());
    }

    #[test]
    fn mounted_bind_roots_and_overlay_references_are_refused() {
        for line in [
            "1 0 0:1 /kernelsu-esp /store rw - f2fs /dev/block/metadata rw",
            "1 0 0:1 / /modules/example rw - overlay overlay rw,upperdir=/offline/egysk/overlays/example/upper",
            "1 0 0:1 / /dev/egysk/store rw - f2fs /dev/block/metadata rw",
        ] {
            assert!(check_mounts(line).is_err());
        }
        assert!(check_mounts("1 0 0:1 / /offline rw - f2fs /dev/block/metadata rw").is_ok());
    }
}
