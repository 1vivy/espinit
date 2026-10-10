use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

pub fn regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "not a regular file: {}",
        path.display()
    );
    Ok(file)
}
pub fn text(path: &Path, max: usize) -> Result<String> {
    let mut value = String::new();
    regular(path)?
        .take(max as u64 + 1)
        .read_to_string(&mut value)?;
    ensure!(
        value.len() <= max && !value.contains('\0'),
        "invalid/oversized {}",
        path.display()
    );
    Ok(value)
}
pub fn optional_text(path: &Path, max: usize) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(_) => text(path, max).map(Some),
    }
}
pub fn directory(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        ensure!(
            matches!(component, Component::Normal(_) | Component::RootDir),
            "unsafe directory path"
        );
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) => ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "unsafe directory {}",
                current.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
pub fn children(path: &Path) -> Result<Vec<PathBuf>> {
    let mut entries = fs::read_dir(path)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    ensure!(entries.len() <= 16384, "too many directory entries");
    Ok(entries)
}
pub fn sync(path: &Path) -> Result<()> {
    File::open(path)?
        .sync_all()
        .with_context(|| format!("fsync {}", path.display()))
}
pub fn sync_tree(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        for child in children(path)? {
            sync_tree(&child)?;
        }
    }
    sync(path)
}
pub fn atomic(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().context("missing parent")?;
    directory(parent)?;
    let temp = path.with_extension("esp-tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temp)?;
    file.write_all(data)?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    sync(parent)
}
pub fn rename(from: &Path, to: &Path) -> Result<()> {
    directory(to.parent().context("missing destination parent")?)?;
    fs::rename(from, to)
        .with_context(|| format!("rename {} -> {}", from.display(), to.display()))?;
    sync(from.parent().context("missing source parent")?)?;
    sync(to.parent().context("missing destination parent")?)
}
fn safe_link(relative: &Path, target: &Path) -> Result<()> {
    if target.is_absolute() {
        ensure!(
            [
                "/system",
                "/vendor",
                "/product",
                "/system_ext",
                "/odm",
                "/oem"
            ]
            .iter()
            .any(|root| target.starts_with(root))
                && target
                    .components()
                    .all(|part| matches!(part, Component::RootDir | Component::Normal(_))),
            "unsafe absolute package symlink: {}",
            relative.display()
        );
        return Ok(());
    }
    let mut depth = relative.parent().map_or(0, |p| p.components().count());
    for part in target.components() {
        match part {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => depth -= 1,
            _ => bail!("package symlink escapes root: {}", relative.display()),
        }
    }
    Ok(())
}
/// Hash names, Unix metadata, object kind and bytes; never follow package links.
pub fn generation(root: &Path) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    fn metadata(hash: &mut Sha256, meta: &fs::Metadata) {
        hash.update((meta.permissions().mode() & 0o7777).to_le_bytes());
        hash.update(meta.uid().to_le_bytes());
        hash.update(meta.gid().to_le_bytes());
    }
    fn visit(root: &Path, path: &Path, hash: &mut Sha256, depth: usize) -> Result<()> {
        ensure!(depth < 128, "package nesting exceeds bound");
        for path in children(path)? {
            let relative = path.strip_prefix(root)?;
            if relative == Path::new(".esp-generation") {
                continue;
            }
            let name = relative.to_str().context("non UTF-8 package path")?;
            hash.update((name.len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
            let meta = fs::symlink_metadata(&path)?;
            metadata(hash, &meta);
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&path)?;
                safe_link(relative, &target)?;
                hash.update(b"l");
                hash.update(target.as_os_str().as_encoded_bytes());
            } else if meta.is_dir() {
                hash.update(b"d");
                visit(root, &path, hash, depth + 1)?;
            } else if meta.is_file() {
                hash.update(b"f");
                hash.update(meta.len().to_le_bytes());
                let mut file = regular(&path)?;
                let mut buffer = [0u8; 65536];
                loop {
                    let n = file.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    hash.update(&buffer[..n]);
                }
            } else {
                bail!("special file in package: {}", path.display());
            }
        }
        Ok(())
    }
    let root_metadata = fs::symlink_metadata(root)?;
    ensure!(
        root_metadata.is_dir() && !root_metadata.file_type().is_symlink(),
        "unsafe package root"
    );
    let mut hash = Sha256::new();
    metadata(&mut hash, &root_metadata);
    visit(root, root, &mut hash, 0)?;
    let mut generation = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in hash.finalize() {
        generation.push(HEX[(byte >> 4) as usize] as char);
        generation.push(HEX[(byte & 15) as usize] as char);
    }
    Ok(generation)
}
/// Copy into an empty private destination; symlinks are validated, never traversed.
pub fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    generation(from)?;
    fn metadata(source: &Path, target: &Path) -> Result<()> {
        let meta = fs::symlink_metadata(source)?;
        let target_c = CString::new(target.as_os_str().as_encoded_bytes())?;
        ensure!(
            unsafe { libc::lchown(target_c.as_ptr(), meta.uid(), meta.gid()) } == 0,
            "preserve installer ownership {}: {}",
            target.display(),
            std::io::Error::last_os_error()
        );
        if !meta.file_type().is_symlink() {
            fs::set_permissions(
                target,
                fs::Permissions::from_mode(meta.permissions().mode() & 0o7777),
            )?;
            sync(target)?;
        }
        Ok(())
    }
    fn copy(from: &Path, to: &Path) -> Result<()> {
        directory(to)?;
        for src in children(from)? {
            let dst = to.join(src.file_name().context("missing name")?);
            let meta = fs::symlink_metadata(&src)?;
            if meta.file_type().is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(&src)?, &dst)?;
                metadata(&src, &dst)?;
            } else if meta.is_dir() {
                copy(&src, &dst)?;
            } else {
                fs::copy(&src, &dst)?;
                metadata(&src, &dst)?;
            }
        }
        metadata(from, to)
    }
    ensure!(!to.exists(), "copy destination already exists");
    copy(from, to)
}
pub fn mount(
    source: &str,
    target: &Path,
    kind: &str,
    flags: libc::c_ulong,
    options: &str,
) -> Result<()> {
    directory(target)?;
    let source = CString::new(source)?;
    let target_c = CString::new(target.as_os_str().as_encoded_bytes())?;
    let kind = CString::new(kind)?;
    let options = CString::new(options)?;
    // SAFETY: all pointers are live NUL-terminated strings for this syscall.
    let rc = unsafe {
        libc::mount(
            source.as_ptr(),
            target_c.as_ptr(),
            kind.as_ptr(),
            flags,
            options.as_ptr().cast(),
        )
    };
    ensure!(
        rc == 0,
        "mount {}: {}",
        target.display(),
        std::io::Error::last_os_error()
    );
    Ok(())
}
pub fn unmount(path: &Path) -> Result<()> {
    let value = CString::new(path.as_os_str().as_encoded_bytes())?;
    // No MNT_DETACH: upper/work must have no live overlay references at handoff.
    if unsafe { libc::umount2(value.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("unmount {}", path.display()));
    }
    Ok(())
}
pub fn mounted(path: &Path) -> Result<bool> {
    let expected = path.to_str().context("invalid mount path")?;
    Ok(fs::read_to_string("/proc/self/mountinfo")?
        .lines()
        .any(|line| line.split_whitespace().nth(4) == Some(expected)))
}
pub fn root_only() -> Result<()> {
    ensure!(unsafe { libc::geteuid() } == 0, "operation requires root");
    Ok(())
}
pub struct StoreLock(File);
impl StoreLock {
    pub fn acquire(root: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root.join("lifecycle.lock"))?;
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
            "lock module lifecycle: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(file))
    }
}
impl Drop for StoreLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
