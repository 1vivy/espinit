use crate::context::*;
use crate::fsutil as fsu;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn stage_tools(config: &BootstrapConfig, rdinit: bool) -> Result<()> {
    let mut tools = BTreeSet::from(BOOTSTRAP_TOOLS);
    tools.insert(config.backing.helper.as_str());
    tools.extend(config.tools.iter().map(String::as_str));
    for name in tools {
        identifier(name)?;
        let independent = Path::new(INDEPENDENT_BIN).join(name);
        let source = if rdinit && name == LOADER {
            PathBuf::from("/proc/self/exe")
        } else if rdinit {
            match fs::symlink_metadata(&independent) {
                Ok(_) => independent,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Path::new(PACKAGE).join("bin").join(name)
                }
                Err(error) => {
                    return Err(error).context("stat independently staged bootstrap tool");
                }
            }
        } else {
            Path::new(PACKAGE).join("bin").join(name)
        };
        if source != Path::new("/proc/self/exe") {
            fsu::regular(&source)?;
        }
        let target = Path::new(BIN).join(name);
        // Android reconstruction is executing the already copied egyskinit.
        if !rdinit && name == LOADER {
            continue;
        }
        ensure!(!target.exists(), "bootstrap tool collision {name}");
        fs::copy(&source, &target).with_context(|| format!("stage tool {}", source.display()))?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
    }
    let resetprop = Path::new(BIN).join("resetprop");
    ensure!(!resetprop.exists(), "resetprop applet collision");
    std::os::unix::fs::symlink(DAEMON, resetprop)?;
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BackingResult {
    owned_mounts: Vec<PathBuf>,
}
/// Helper CLI (all arguments literal, no shell):
/// `gobbl-runtime prepare-backing --kind block|filesystem
/// (--source ABS | --partition NAME) --fs-type TYPE --mount-at ABS
/// --store /dev/egysk/store [args...]`.
/// stdout: JSON `{ "owned_mounts": ["/actual/owned/mount", "/dev/egysk/store"] }`.
/// Paths are creation order, only newly owned mounts, never Android/vendor mounts.
/// The helper coordinates an existing Android metadata mount and never formats.
pub fn prepare_backing(config: &BootstrapConfig) -> Result<Vec<PathBuf>> {
    let backing = &config.backing;
    let output_path = Path::new(ROOT).join("backing-result");
    let output = fs::File::create(&output_path)?;
    let mut command = Command::new(Path::new(BIN).join(&backing.helper));
    crate::scripts::child_signals(&mut command);
    command.process_group(0).arg("prepare-backing").args([
        "--kind",
        &backing.kind,
        "--fs-type",
        &backing.fs_type,
        "--mount-at",
        &backing.mount_at,
        "--store",
        STORE,
    ]);
    if let Some(source) = &backing.source {
        ensure!(
            Path::new(source).exists(),
            "explicit backing source is unavailable: {source}"
        );
        command.args(["--source", source]);
    } else {
        command.args([
            "--partition",
            backing
                .partition
                .as_deref()
                .context("missing backing selector")?,
        ]);
    }
    let mut child = command
        .args(&backing.args)
        .stdin(Stdio::null())
        .stdout(output)
        .spawn()
        .context("start physical metadata bootstrap")?;
    crate::scripts::wait_until(&mut child, Instant::now() + Duration::from_secs(35), false)
        .with_context(|| {
            format!(
                "physical metadata bootstrap {} prepare-backing",
                backing.helper
            )
        })?;
    let result: BackingResult = serde_json::from_str(&fsu::text(&output_path, 4096)?)?;
    ensure!(
        fsu::mounted(Path::new(STORE))?,
        "helper did not expose metadata store bind"
    );
    let mut seen = BTreeSet::new();
    for path in &result.owned_mounts {
        absolute(path.to_str().context("non UTF-8 helper mount")?)?;
        ensure!(
            path == Path::new(STORE) || path == Path::new(&backing.mount_at),
            "helper returned unrelated mount {}",
            path.display()
        );
        ensure!(
            seen.insert(path) && fsu::mounted(path)?,
            "invalid helper mount ownership"
        );
    }
    Ok(result.owned_mounts)
}

pub fn initialize_store() -> Result<()> {
    for name in [
        "modules",
        "modules_update",
        "modules_previous",
        "state",
        "overlays",
        "preferences",
        "log",
        "post-fs-data.d",
        "service.d",
        "config",
    ] {
        fsu::directory(&Path::new(STORE).join(name))?;
    }
    fsu::directory(Path::new(MODULES))?;
    fsu::directory(&Path::new(ROOT).join("state"))
}
fn label(path: &Path, context: &str) -> Result<()> {
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    let context = std::ffi::CString::new(context)?;
    // lsetxattr never follows a module's symlink. With metacopy=off this also
    // forces the owned overlay's inode/data copy-up before Android exports it.
    ensure!(
        unsafe {
            libc::lsetxattr(
                name.as_ptr(),
                c"security.selinux".as_ptr(),
                context.as_ptr().cast(),
                context.as_bytes_with_nul().len(),
                0,
            )
        } == 0,
        "label {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    Ok(())
}
fn label_tree(root: &Path, context: &str) -> Result<()> {
    label(root, context)?;
    if fs::symlink_metadata(root)?.is_dir() {
        for path in fsu::children(root)? {
            label_tree(&path, context)?;
        }
    }
    Ok(())
}
pub fn label_module(module: &Module) -> Result<()> {
    let root = Path::new(MODULES).join(&module.id);
    // FAT supplies no Unix modes. Initialize a fresh ESP upper once; preserved
    // generations (including rollback) retain the user's copied-up permissions.
    if module.owner == Owner::Esp
        && !Path::new(STORE)
            .join("prepared")
            .join(&module.id)
            .join(&module.generation)
            .exists()
    {
        fn permissions(root: &Path, path: &Path) -> Result<()> {
            let metadata = fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink() {
                return Ok(());
            }
            let relative = path.strip_prefix(root)?;
            let executable = relative
                .components()
                .any(|part| matches!(part.as_os_str().to_str(), Some("bin" | "xbin")))
                || path.extension().and_then(|value| value.to_str()) == Some("sh");
            fs::set_permissions(
                path,
                fs::Permissions::from_mode(if metadata.is_dir() || executable {
                    0o755
                } else {
                    0o644
                }),
            )
            .with_context(|| format!("initialize module permissions: {}", path.display()))?;
            if metadata.is_dir() {
                for child in fsu::children(path)? {
                    permissions(root, &child)?;
                }
            }
            Ok(())
        }
        permissions(&root, &root)?;
    }
    label_tree(&root, FILE_CONTEXT)?;
    for (directory, context) in [
        ("system", "u:object_r:system_file:s0"),
        ("system/vendor", "u:object_r:vendor_file:s0"),
    ] {
        let directory = root.join(directory);
        if directory.exists() {
            label_tree(&directory, context)?;
        }
    }
    label_tree(
        &Path::new(ROOT).join("state").join(&module.id),
        FILE_CONTEXT,
    )?;
    Ok(())
}
pub fn persist_descriptor(descriptor: &Descriptor) -> Result<()> {
    descriptor.validate()?;
    fsu::atomic(Path::new(SOURCE), &serde_json::to_vec(descriptor)?)
}
/// Reconstruct only the descriptor's exact source and generations, never run
/// discovery/activation a second time. Enter in init; create final views in egysk.
pub fn reconstruct(hex: &str) -> Result<()> {
    fsu::root_only()?;
    let descriptor = Descriptor::decode(hex)?;
    ensure!(
        fsu::mounted(Path::new(ESP))? && fsu::mounted(Path::new(BIN))?,
        "stock-tool ESP/bin bootstrap missing"
    );
    let device = fs::metadata(ESP)?.dev();
    let major =
        u32::try_from(i64::from(libc::major(device))).context("invalid reconstructed ESP major")?;
    let minor =
        u32::try_from(i64::from(libc::minor(device))).context("invalid reconstructed ESP minor")?;
    ensure!(
        major == descriptor.esp_major && minor == descriptor.esp_minor,
        "reconstructed ESP identity mismatch"
    );
    stage_tools(&descriptor.config, false)?;
    // Base helper declarations allow these labels and init-context execution;
    // the complete native policy is loaded only after its executable is usable.
    // This boot-local parent is shared with system-UID module consumers; do not
    // let the launching init's umask make their explicitly labelled files
    // unreachable. Contents retain their own narrower permissions.
    fs::set_permissions(ROOT, fs::Permissions::from_mode(0o755))?;
    label(Path::new(ROOT), FILE_CONTEXT)?;
    label_tree(Path::new(BIN), FILE_CONTEXT)?;
    let mut selected = descriptor.config.clone();
    if selected.backing.kind == "block" {
        let node = SELECTED_BACKING.to_owned();
        let path = std::ffi::CString::new(node.as_str())?;
        ensure!(
            unsafe {
                libc::mknod(
                    path.as_ptr(),
                    libc::S_IFBLK | 0o600,
                    descriptor.backing_device,
                )
            } == 0,
            "create selected physical backing node: {}",
            std::io::Error::last_os_error()
        );
        selected.backing.source = Some(node);
        selected.backing.partition = None;
    }
    prepare_backing(&selected)?;
    ensure!(
        fs::metadata(STORE)?.dev() == descriptor.backing_device,
        "reconstructed metadata identity mismatch"
    );
    initialize_store()?;
    // The kernel's sysfs export omits Android policy configuration bits. Export
    // the coherent live policy through the helper rather than replacing foreign
    // rules or deriving configuration from an original on-disk policy.
    let snapshot = Path::new(ROOT).join("bootstrap.policy");
    fsu::atomic(&snapshot, &crate::helper::live_policy()?)?;
    let output = Command::new(Path::new(BIN).join(POLICY_TOOL))
        .arg("--load")
        .arg(&snapshot)
        .args(["--live", "--magisk"])
        .output();
    let cleanup = fs::remove_file(&snapshot);
    let output = output.context("execute native base product policy")?;
    cleanup.context("remove bootstrap policy snapshot")?;
    ensure!(
        output.status.success(),
        "native base product policy failed: {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    // Persisted uppers carry Android export labels from the previous boot.
    // Relabel them in the product domain, not by expanding stock init's access.
    // OverlayFS also retains this domain's credentials for lower/upper work.
    fs::write("/proc/self/attr/current", DOMAIN.as_bytes())
        .context("enter product context before reconstructing module storage")?;
    // Only the dedicated framework subtree is labelled, never metadata's keys
    // or password_slots siblings. Do this before attaching any overlay.
    label_tree(Path::new(STORE), FILE_CONTEXT)?;
    label(Path::new(MODULES), FILE_CONTEXT)?;
    crate::store::mount_view(&descriptor.modules).context("reconstruct module views")?;
    fsu::mount(
        MOUNT_SOURCE,
        Path::new(RD),
        EXECUTABLE_MOUNT.fs_type,
        EXECUTABLE_MOUNT.flags,
        EXECUTABLE_MOUNT.data,
    )
    .context("reconstruct staged module filesystem")?;
    for module in &descriptor.modules {
        stage_module_files(module).with_context(|| format!("stage module {} files", module.id))?;
    }
    label_tree(Path::new(RD), FILE_CONTEXT)?;
    label_tree(Path::new(BIN), FILE_CONTEXT)?;
    restore_efivarfs().context("restore efivarfs")?;
    persist_descriptor(&descriptor).context("persist reconstructed descriptor")?;
    label(Path::new(SOURCE), FILE_CONTEXT)?;
    fsu::atomic(&Path::new(ROOT).join("reconstructed"), b"1")?;
    label(&Path::new(ROOT).join("reconstructed"), FILE_CONTEXT)?;
    crate::property("egysk.bootstrap.ready", "1")
}
pub fn restore_efivarfs() -> Result<()> {
    // Only a loaded filesystem is restored; EFI is an independent critical LKM
    // when the platform requires it, not a generic ROM schema dependency.
    let available = fs::read_to_string("/proc/filesystems")?
        .lines()
        .any(|line| line.split_whitespace().last() == Some("efivarfs"));
    let path = Path::new("/sys/firmware/efi/efivars");
    if available && !fsu::mounted(path)? {
        fsu::mount(
            "efivarfs",
            path,
            "efivarfs",
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            &format!("context={FILE_CONTEXT}"),
        )?;
    }
    Ok(())
}

/// Stage `rd/` under the owned executable runtime, merging disjoint directories
/// but rejecting every file collision. `early_bin` lists module-relative files
/// copied by basename to bin. `rd_copy` lists `source:relative-destination`
/// entries below the owned rd tree. Neither can overwrite a bootstrap tool.
pub fn stage_module_files(module: &Module) -> Result<()> {
    fn plan(
        source: &Path,
        target: &Path,
        files: &mut Vec<(PathBuf, PathBuf, bool)>,
        seen: &mut BTreeMap<PathBuf, bool>,
        depth: usize,
    ) -> Result<()> {
        ensure!(depth < 128, "early staging nesting exceeds bound");
        let meta = fs::symlink_metadata(source)?;
        ensure!(!meta.file_type().is_symlink(), "symlink in early staging");
        if meta.is_dir() {
            match fs::symlink_metadata(target) {
                Ok(existing) => ensure!(
                    existing.is_dir() && !existing.file_type().is_symlink(),
                    "early directory collision"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("stat early staging directory"),
            }
            ensure!(
                seen.get(target) != Some(&false),
                "early directory collides with planned file"
            );
            if seen.insert(target.to_owned(), true).is_none() {
                files.push((source.to_owned(), target.to_owned(), true));
            }
            for child in fsu::children(source)? {
                plan(
                    &child,
                    &target.join(child.file_name().context("missing staged name")?),
                    files,
                    seen,
                    depth + 1,
                )?;
            }
        } else {
            ensure!(meta.is_file(), "invalid early staging file");
            match fs::symlink_metadata(target) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("stat early staging file"),
                Ok(_) => anyhow::bail!("early staging collision {}", target.display()),
            }
            ensure!(
                seen.insert(target.to_owned(), false).is_none(),
                "early staging collision {}",
                target.display()
            );
            files.push((source.to_owned(), target.to_owned(), false));
        }
        Ok(())
    }
    fn relative(root: &Path, name: &str) -> Result<PathBuf> {
        ensure!(
            !name.is_empty()
                && Path::new(name)
                    .components()
                    .all(|p| matches!(p, std::path::Component::Normal(_))),
            "unsafe early export path"
        );
        let path = root.join(name);
        let mut prefix = root.to_owned();
        for part in Path::new(name).components() {
            prefix.push(part);
            if let Ok(meta) = fs::symlink_metadata(&prefix) {
                ensure!(
                    !meta.file_type().is_symlink(),
                    "symlink in early export path"
                );
            }
        }
        Ok(path)
    }
    let root = Path::new(MODULES).join(&module.id);
    let rd = root.join("rd");
    let mut files = Vec::new();
    let mut seen = BTreeMap::new();
    match fs::symlink_metadata(&rd) {
        Ok(_) => plan(&rd, Path::new(RD), &mut files, &mut seen, 0)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("stat module rd exports"),
    }
    let prop = fsu::text(&root.join("module.prop"), 65536)?;
    for (key, values) in prop.lines().filter_map(|line| line.split_once('=')) {
        for item in values.split_whitespace() {
            match key.trim() {
                "early_bin" => {
                    let source = relative(&root, item)?;
                    plan(
                        &source,
                        &Path::new(BIN).join(source.file_name().context("invalid early_bin")?),
                        &mut files,
                        &mut seen,
                        0,
                    )?;
                }
                "rd_copy" => {
                    let (source, destination) = item
                        .split_once(':')
                        .context("rd_copy must be source:destination")?;
                    plan(
                        &relative(&root, source)?,
                        &relative(Path::new(RD), destination)?,
                        &mut files,
                        &mut seen,
                        0,
                    )?;
                }
                _ => {}
            }
        }
    }
    let mut copied = Vec::new();
    let mut directories = Vec::new();
    let result = (|| -> Result<()> {
        for (source, target, directory) in &files {
            if *directory {
                if !target.exists() {
                    fsu::directory(target)?;
                    directories.push(target);
                    fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
                }
            } else {
                fsu::directory(target.parent().context("missing staging parent")?)?;
                copied.push(target);
                fs::copy(source, target)?;
                fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        for target in copied.iter().rev() {
            if target.exists() {
                fs::remove_file(target)
                    .with_context(|| format!("undo failed staging: {error:#}"))?;
            }
        }
        for target in directories.iter().rev() {
            fs::remove_dir(target)
                .with_context(|| format!("undo failed directory staging: {error:#}"))?;
        }
        return Err(error);
    }
    Ok(())
}
