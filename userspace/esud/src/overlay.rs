//! Read-only ESP partition overlays, relabelled on an executable tmpfs.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const PARTITIONS: [&str; 5] = ["system", "vendor", "product", "system_ext", "odm"];
const STAGING: &str = "/dev/esu";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attrs {
    mode: u32,
    uid: u32,
    gid: u32,
    context: String,
}

pub fn parse_attrs(text: &str) -> Result<BTreeMap<String, Attrs>> {
    let mut attrs = BTreeMap::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        ensure!(
            fields.len() == 5,
            "attrs line {}: expected path mode uid gid context",
            number + 1
        );
        let path = fields[0];
        let parts: Vec<_> = path
            .strip_prefix('/')
            .context("attrs path must be absolute")?
            .split('/')
            .collect();
        ensure!(
            PARTITIONS.contains(&parts[0])
                && parts.iter().all(|p| !p.is_empty()
                    && !matches!(*p, "." | "..")
                    && !p.contains([':', '\\', '\0'])),
            "invalid attrs path: {path}"
        );
        ensure!(
            !fields[1].is_empty() && fields[1].bytes().all(|b| (b'0'..=b'7').contains(&b)),
            "invalid attrs mode"
        );
        let mode = u32::from_str_radix(fields[1], 8)?;
        ensure!(mode <= 0o7777, "attrs mode outside permission bits");
        for field in &fields[2..4] {
            ensure!(
                field.bytes().all(|b| b.is_ascii_digit()),
                "invalid attrs uid/gid"
            );
        }
        let uid = fields[2].parse::<u32>()?;
        let gid = fields[3].parse::<u32>()?;
        ensure!(
            uid != u32::MAX && gid != u32::MAX,
            "invalid attrs uid/gid sentinel"
        );
        let context = fields[4];
        let labels: Vec<_> = context.split(':').collect();
        ensure!(
            labels.len() >= 4
                && labels.iter().all(|s| !s.is_empty())
                && labels[..3]
                    .iter()
                    .all(|s| s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
                && labels[3]
                    .strip_prefix('s')
                    .is_some_and(|level| level.as_bytes().first().is_some_and(u8::is_ascii_digit))
                && context
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_:,.-".contains(&b)),
            "invalid attrs SELinux context"
        );
        ensure!(
            attrs
                .insert(
                    path.to_owned(),
                    Attrs {
                        mode,
                        uid,
                        gid,
                        context: context.to_owned()
                    }
                )
                .is_none(),
            "duplicate attrs path: {path}"
        );
    }
    Ok(attrs)
}

pub fn label(path: &Path, context: &str) -> Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let context = CString::new(context)?;
    let bytes = context.as_bytes_with_nul();
    let result = unsafe {
        libc::lsetxattr(
            path.as_ptr(),
            c"security.selinux".as_ptr(),
            bytes.as_ptr().cast(),
            bytes.len(),
            0,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn target_attrs(path: &Path) -> Result<Attrs> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("OverlayAttrsMissing {}", path.display()))?;
    let name = CString::new(path.as_os_str().as_bytes())?;
    let size = unsafe {
        libc::lgetxattr(
            name.as_ptr(),
            c"security.selinux".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    ensure!(
        size > 0,
        "OverlayAttrsMissing {}: no SELinux label",
        path.display()
    );
    let mut value = vec![0; usize::try_from(size)?];
    let read = unsafe {
        libc::lgetxattr(
            name.as_ptr(),
            c"security.selinux".as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    ensure!(
        read == size,
        "OverlayAttrsMissing {}: label changed/unreadable",
        path.display()
    );
    if value.last() == Some(&0) {
        value.pop();
    }
    Ok(Attrs {
        mode: metadata.mode() & 0o7777,
        uid: metadata.uid(),
        gid: metadata.gid(),
        context: String::from_utf8(value)?,
    })
}

pub fn file_context(path: &Path) -> Result<String> {
    Ok(target_attrs(path)?.context)
}

/// Copy one partition tree. Resolve attributes before creating each destination;
/// vfat permissions and its single mount label are never used as overlay attrs.
pub fn stage(
    source: &Path,
    destination: &Path,
    target: &Path,
    logical: &str,
    attrs: &BTreeMap<String, Attrs>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    ensure!(
        metadata.is_dir() || metadata.is_file(),
        "unsupported overlay inode: {}",
        source.display()
    );
    let attributes = match attrs.get(logical) {
        Some(attrs) => attrs.clone(),
        None => target_attrs(target)?,
    };
    if metadata.is_dir() {
        fs::create_dir_all(destination)?;
        let mut entries = fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_str().context("non-UTF8 overlay path")?;
            ensure!(!name.contains([':', '\\', '\0']), "invalid overlay path");
            stage(
                &entry.path(),
                &destination.join(name),
                &target.join(name),
                &format!("{logical}/{name}"),
                attrs,
            )?;
        }
    } else {
        fs::copy(source, destination)?;
    }
    std::os::unix::fs::chown(destination, Some(attributes.uid), Some(attributes.gid))?;
    fs::set_permissions(destination, fs::Permissions::from_mode(attributes.mode))?;
    label(destination, &attributes.context)?;
    Ok(())
}

pub fn mounted(target: &str, filesystem: &str) -> Result<bool> {
    Ok(fs::read_to_string("/proc/self/mountinfo")?
        .lines()
        .any(|line| {
            let Some((left, right)) = line.split_once(" - ") else {
                return false;
            };
            left.split_whitespace().nth(4) == Some(target)
                && right.split_whitespace().next() == Some(filesystem)
                && (filesystem != "overlay"
                    || right.split_whitespace().nth(2).is_some_and(|options| {
                        options
                            .split(',')
                            .any(|option| option.starts_with("lowerdir=/dev/esu/"))
                    }))
        }))
}

fn mount(
    source: &str,
    target: &str,
    filesystem: &str,
    flags: libc::c_ulong,
    data: &str,
) -> Result<()> {
    let (source, target, filesystem, data) = (
        CString::new(source)?,
        CString::new(target)?,
        CString::new(filesystem)?,
        CString::new(data)?,
    );
    let result = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            filesystem.as_ptr(),
            flags,
            data.as_ptr().cast(),
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// True when `target` is a mount point in this mount namespace.
pub fn mount_point(target: &str) -> Result<bool> {
    Ok(fs::read_to_string("/proc/self/mountinfo")?
        .lines()
        .filter_map(|line| line.split_once(" - ").map(|(left, _)| left))
        .any(|left| left.split_whitespace().nth(4) == Some(target)))
}

/// Policy must be installed before copying labels. A reload does not execute scripts.
///
/// Recovery shares the ESP stack with Android, but its ramdisk mounts a target
/// partition later than `on init` on some devices: a partition that is not a
/// mount point yet is skipped instead of being covered by an overlay that would
/// block recovery's own mount.
///
/// Policy/attribute/staging failures omit that module's layers. A failed combined
/// partition mount leaves the stock partition visible. Either failure stops
/// normal Android when the affected module/partition has a critical contributor.
pub fn apply(root: &Path, order: &[String], recovery: bool) -> Result<()> {
    let mut layers: BTreeMap<&str, Vec<(PathBuf, bool)>> = BTreeMap::new();
    for id in order {
        let module = root.join(id);
        // The shared admission markers decide which modules take part: a
        // disabled/removed module and, in recovery, a module PID 1 did not
        // admit contribute no overlay either, and an admitted module must still
        // be identifiable.
        let Some(critical) = crate::module::admission(&module, id, recovery)? else {
            continue;
        };
        match stage_module(&module, id, recovery) {
            Ok(entries) => {
                for (partition, destination) in entries {
                    layers
                        .entry(partition)
                        .or_default()
                        .push((destination, critical));
                }
            }
            Err(error) => escalate_or_skip(critical, id, recovery, error)?,
        }
    }
    for (partition, entries) in layers {
        let target = format!("/{partition}");
        let result = (|| -> Result<()> {
            if mounted(&target, "overlay")? {
                return Ok(());
            }
            let mut lowerdirs = entries
                .iter()
                .map(|(path, _)| path.to_str().context("invalid staging path"))
                .collect::<Result<Vec<_>>>()?;
            lowerdirs.push(&target);
            mount(
                "overlay",
                &target,
                "overlay",
                libc::MS_RDONLY,
                &format!("lowerdir={}", lowerdirs.join(":")),
            )
        })();
        let Err(error) = result else {
            continue;
        };
        let error = error.context(format!("{target} overlay"));
        if !recovery && entries.iter().any(|(_, critical)| *critical) {
            return Err(crate::module::CriticalModuleError::new(error).into());
        }
        log::warn!("{error:#}");
    }
    Ok(())
}

/// Apply one module's automatic policy and stage its partition trees. Any error
/// leaves the module out of the overlay stack; the caller decides whether that
/// is fatal, so the disposition never depends on where the failure happened.
fn stage_module(module: &Path, id: &str, recovery: bool) -> Result<Vec<(&'static str, PathBuf)>> {
    let policy = module.join("sepolicy.rule");
    if policy.exists() {
        crate::sepolicy::apply_strict(&std::fs::read_to_string(policy)?)
            .with_context(|| format!("module {id} sepolicy"))?;
    }
    if skip_mount(module) {
        // `skip_mount` keeps the module's scripts and policy; only its Android
        // partitions are left alone.
        return Ok(Vec::new());
    }
    let attrs_path = module.join("attrs");
    let attrs = if attrs_path.exists() {
        parse_attrs(&std::fs::read_to_string(attrs_path)?)?
    } else {
        BTreeMap::new()
    };
    let mut entries = Vec::new();
    for partition in PARTITIONS {
        let source = module.join(partition);
        if !source.exists() {
            continue;
        }
        let target = format!("/{partition}");
        if recovery && !mount_point(&target)? {
            log::warn!("recovery: {target} is not a mount point; skipping {id}'s overlay");
            continue;
        }
        let destination = Path::new(STAGING).join(id).join(partition);
        if !mounted(&target, "overlay")? {
            if !mounted(STAGING, "tmpfs")? {
                fs::create_dir_all(STAGING)?;
                mount(
                    "tmpfs",
                    STAGING,
                    "tmpfs",
                    0, // metadata_shared block node must be openable here.
                    "mode=0700,uid=0,gid=0",
                )?;
            }
            let private = Path::new(STAGING).join(id);
            fs::create_dir_all(&private)?;
            fs::set_permissions(&private, fs::Permissions::from_mode(0o700))?;
            stage(&source, &destination, Path::new(&target), &target, &attrs)?;
        }
        entries.push((partition, destination));
    }
    Ok(entries)
}

/// `skip_mount` is a mount-layer flag, not an admission filter, so it is read
/// here: only a regular marker file counts.
fn skip_mount(module: &Path) -> bool {
    fs::symlink_metadata(module.join("skip_mount")).is_ok_and(|metadata| metadata.is_file())
}

/// Report a module's overlay failure, or stop a normal Android boot when the
/// module is critical. Recovery only logs it.
fn escalate_or_skip(critical: bool, id: &str, recovery: bool, error: anyhow::Error) -> Result<()> {
    let error = error.context(format!("module {id} overlay"));
    if recovery || !critical {
        log::warn!("{error:#}");
        return Ok(());
    }
    Err(crate::module::CriticalModuleError::new(error).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attrs_contract() {
        let attrs =
            parse_attrs("/vendor/bin/hw/boot 0755 0 2000 u:object_r:hal_bootctl_default_exec:s0\n")
                .unwrap();
        assert_eq!(attrs["/vendor/bin/hw/boot"].mode, 0o755);
        assert_eq!(attrs["/vendor/bin/hw/boot"].gid, 2000);
        for line in [
            "vendor/a 0755 0 0 u:object_r:x:s0",
            "/vendor/../a 0755 0 0 u:object_r:x:s0",
            "/vendor/a 888 0 0 u:object_r:x:s0",
            "/vendor/a 10000 0 0 u:object_r:x:s0",
            "/vendor/a 755 -1 0 u:object_r:x:s0",
            "/vendor/a 755 4294967296 0 u:object_r:x:s0",
            "/vendor/a 755 0 0 invalid",
        ] {
            assert!(parse_attrs(line).is_err(), "{line}");
        }
    }
    #[test]
    fn missing_target_requires_attrs_before_copy() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::write(&source, b"payload").unwrap();
        let error = stage(
            &source,
            &temp.path().join("destination"),
            &temp.path().join("missing"),
            "/vendor/new",
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("OverlayAttrsMissing"));
        assert!(!temp.path().join("destination").exists());
    }
    fn bad_module(root: &Path, id: &str, markers: &[&str]) {
        let module = root.join(id);
        fs::create_dir(&module).unwrap();
        fs::write(module.join("module.prop"), format!("id={id}\n")).unwrap();
        fs::write(module.join("sepolicy.rule"), "allow broken").unwrap();
        for marker in markers {
            fs::write(module.join(marker), b"").unwrap();
        }
    }

    #[test]
    fn optional_policy_failure_cannot_hide_a_later_critical_failure() {
        for recovery in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            bad_module(temp.path(), "optional", &["recovery-ok"]);
            bad_module(temp.path(), "critical", &["critical", "recovery-ok"]);
            let result = apply(
                temp.path(),
                &["optional".to_owned(), "critical".to_owned()],
                recovery,
            );
            if recovery {
                assert!(result.is_ok());
            } else {
                assert!(crate::module::is_critical_failure(&result.unwrap_err()));
            }
        }
    }

    #[test]
    fn disabled_and_removed_modules_are_never_read() {
        for marker in ["disable", "remove"] {
            let temp = tempfile::tempdir().unwrap();
            bad_module(temp.path(), "bad", &["critical"]);
            fs::write(temp.path().join("bad/module.prop"), "id=other\n").unwrap();
            let error = apply(temp.path(), &["bad".to_owned()], false).unwrap_err();
            assert!(crate::module::is_critical_failure(&error));
            fs::write(temp.path().join("bad").join(marker), b"").unwrap();
            apply(temp.path(), &["bad".to_owned()], false).unwrap();
        }
    }

    #[test]
    fn an_unidentifiable_module_contributes_no_overlay() {
        for prop in ["id=other\n", ""] {
            let temp = tempfile::tempdir().unwrap();
            bad_module(temp.path(), "bad", &[]);
            fs::write(temp.path().join("bad/module.prop"), prop).unwrap();
            // Optional: reported and skipped.
            apply(temp.path(), &["bad".to_owned()], false).unwrap();
            // Critical: normal Android stops instead.
            fs::write(temp.path().join("bad/critical"), b"").unwrap();
            let error = apply(temp.path(), &["bad".to_owned()], false).unwrap_err();
            assert!(crate::module::is_critical_failure(&error), "{prop:?}");
            // Recovery only reports it.
            fs::write(temp.path().join("bad/recovery-ok"), b"").unwrap();
            apply(temp.path(), &["bad".to_owned()], true).unwrap();
        }
    }

    #[test]
    fn skip_mount_bypasses_unused_overlay_attributes() {
        let temp = tempfile::tempdir().unwrap();
        bad_module(temp.path(), "bad", &["critical"]);
        let module = temp.path().join("bad");
        fs::remove_file(module.join("sepolicy.rule")).unwrap();
        fs::write(module.join("attrs"), "invalid attributes").unwrap();
        fs::create_dir(module.join("vendor")).unwrap();
        let error = apply(temp.path(), &["bad".to_owned()], false).unwrap_err();
        assert!(crate::module::is_critical_failure(&error));
        fs::write(module.join("skip_mount"), b"").unwrap();
        apply(temp.path(), &["bad".to_owned()], false).unwrap();
    }
    #[test]
    fn mount_point_reads_this_mount_namespace() {
        // Recovery gates its overlays on this predicate: the root always is a
        // mount point, an unused path never is.
        assert!(mount_point("/").unwrap());
        assert!(!mount_point("/esu-nonexistent-mount").unwrap());
    }
}
