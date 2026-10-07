//! Kernel module loading.
//!
//! Standard vendor modules from the boot image are loaded with the normal
//! `finit_module` syscall. Selection and ordering follow Android's libmodprobe:
//! the versioned module directory matching the running kernel comes first, the
//! mode-specific `modules.load` list (recovery/charger) comes before the plain
//! list, and hard dependencies from `modules.dep` plus pre-softdeps from
//! `modules.softdep` are loaded first; a softdep line that cannot be parsed is
//! warned about and skipped, the way Android's libmodprobe treats that advisory
//! file, so one malformed vendor line cannot abort the load. Module options
//! come from `modules.options` and the kernel command line; nothing is
//! evaluated by a shell. Payload modules from the ESP keep using the existing kallsyms
//! relocation loader, which the kernel module lifecycle has always relied on.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};

use rustix::io::Errno;
use rustix::system::finit_module;

use crate::config::ModuleEntry;
use crate::receipt::{Failure, Stage};
use crate::scripts::{BootMode, RECOVERY_EXECUTABLE, classify_boot_mode};

const MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MODULES: usize = 16_384;
const MAX_DEPENDENCY_DEPTH: usize = 128;

fn vendor_error(detail: impl AsRef<str>) -> Failure {
    Failure::new(Stage::ModuleLoad, "VendorModuleLoad", detail)
}

/// Read bounded metadata; only genuinely absent optional files are ignored.
fn read_metadata(path: &Path, optional: bool) -> Result<Option<String>, Failure> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if optional && error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(vendor_error(format!(
                "cannot open {}: {error}",
                path.display()
            )));
        }
    };
    let mut text = String::new();
    file.take(MAX_METADATA_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|error| vendor_error(format!("cannot read {}: {error}", path.display())))?;
    if text.len() as u64 > MAX_METADATA_BYTES || text.contains('\0') {
        return Err(vendor_error(format!(
            "invalid or oversized metadata {}",
            path.display()
        )));
    }
    Ok(Some(text))
}

fn mode_list(mode: BootMode) -> &'static str {
    match mode {
        BootMode::Normal => "modules.load",
        BootMode::Recovery => "modules.load.recovery",
        BootMode::Charger => "modules.load.charger",
    }
}

fn existing_list(base: &Path, mode: BootMode) -> Result<Option<PathBuf>, Failure> {
    for name in [mode_list(mode), "modules.load"] {
        let path = base.join(name);
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => return Ok(Some(path)),
            Ok(_) => {
                return Err(vendor_error(format!(
                    "{} is not a regular file",
                    path.display()
                )));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(vendor_error(format!(
                    "cannot stat {}: {error}",
                    path.display()
                )));
            }
        }
    }
    Ok(None)
}

fn select_vendor_list(release: &str, mode: BootMode) -> Result<Option<PathBuf>, Failure> {
    let mut version = release.trim().split('.');
    let major = version.next().unwrap_or("");
    let minor = version.next().unwrap_or("");
    if major.is_empty()
        || minor.is_empty()
        || !major.bytes().all(|byte| byte.is_ascii_digit())
        || !minor.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(vendor_error("invalid kernel release"));
    }
    let prefix = format!("{major}.{minor}");
    let root = Path::new("/lib/modules");
    let mut directories = Vec::new();
    match fs::read_dir(root) {
        Ok(entries) => {
            for (index, entry) in entries.enumerate() {
                if index >= MAX_MODULES {
                    return Err(vendor_error("too many module directories"));
                }
                let entry =
                    entry.map_err(|error| vendor_error(format!("cannot list modules: {error}")))?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if name.starts_with(&prefix)
                    && !name[prefix.len()..].starts_with(|ch: char| ch.is_ascii_digit())
                {
                    let metadata = entry.metadata().map_err(|error| {
                        vendor_error(format!("cannot stat module directory: {error}"))
                    })?;
                    if metadata.is_dir() {
                        directories.push(entry.path());
                    }
                }
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(vendor_error(format!(
                "cannot list {}: {error}",
                root.display()
            )));
        }
    }
    directories.sort();
    // Prefer the exact running release when several Android module sets exist.
    if let Some(index) = directories
        .iter()
        .position(|path| path.file_name().and_then(|name| name.to_str()) == Some(release.trim()))
    {
        let exact = directories.remove(index);
        directories.insert(0, exact);
    }
    for directory in directories
        .iter()
        .map(PathBuf::as_path)
        .chain([root, Path::new("/vendor/lib/modules")])
    {
        if let Some(list) = existing_list(directory, mode)? {
            return Ok(Some(list));
        }
    }
    Ok(None)
}

fn module_name(path: &str) -> String {
    let base = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    base.trim_end_matches(".ko").replace('-', "_")
}

fn validate_module_path(path: &str) -> Result<(), Failure> {
    if path.is_empty()
        || Path::new(path)
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::RootDir))
    {
        return Err(vendor_error(format!(
            "module path escapes selected directory: {path}"
        )));
    }
    Ok(())
}

fn confined_file(base: &Path, module_path: &str) -> Result<PathBuf, Failure> {
    validate_module_path(module_path)?;
    let path = fs::canonicalize(base.join(module_path))
        .map_err(|error| vendor_error(format!("cannot resolve {module_path}: {error}")))?;
    if !path.starts_with(base) || !path.is_file() {
        return Err(vendor_error(format!(
            "not a regular file inside module directory: {module_path}"
        )));
    }
    Ok(path)
}

/// Kernel-style whitespace splitting that keeps double-quoted values together.
///
/// Quotes are preserved because the kernel's module parameter parser consumes
/// them; nothing here is evaluated by a shell. An unbalanced quote is not an
/// error: the remaining text stays one token.
fn command_line_tokens(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start = None;
    let mut quoted = false;

    for (index, ch) in text.char_indices() {
        if ch == '"' {
            quoted = !quoted;
        }

        if ch.is_whitespace() && !quoted {
            if let Some(begin) = start.take() {
                tokens.push(&text[begin..index]);
            }
        } else {
            start.get_or_insert(index);
        }
    }

    if let Some(begin) = start {
        tokens.push(&text[begin..]);
    }

    tokens
}

#[derive(Default)]
struct VendorModules {
    requested: Vec<String>,
    paths: BTreeMap<String, String>,
    dependencies: BTreeMap<String, Vec<String>>,
    pre_softdeps: BTreeMap<String, Vec<String>>,
    options: BTreeMap<String, Vec<String>>,
}

impl VendorModules {
    fn register(&mut self, path: &str) -> Result<(), Failure> {
        validate_module_path(path)?;
        let name = module_name(path);
        if let Some(previous) = self.paths.get(&name) {
            if previous != path {
                return Err(vendor_error(format!("ambiguous module name {name}")));
            }
        } else {
            if self.paths.len() >= MAX_MODULES {
                return Err(vendor_error("too many vendor modules"));
            }
            self.paths.insert(name, path.to_owned());
        }
        Ok(())
    }

    fn parse(
        list: &str,
        dep: &str,
        softdep: &str,
        options: &str,
        cmdline: &str,
    ) -> Result<Self, Failure> {
        let mut modules = Self::default();

        // `modules.dep` is authoritative for module paths, exactly as Android's
        // libmodprobe uses it; the load list only selects and orders modules.
        for line in dep
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
        {
            let (path, dependencies) = line
                .split_once(':')
                .ok_or_else(|| vendor_error("invalid modules.dep line"))?;
            let path = path.trim();
            modules.register(path)?;
            let mut ordered = Vec::new();
            for dependency in dependencies.split_whitespace() {
                validate_module_path(dependency)?;
                ordered.push(module_name(dependency));
            }
            if modules
                .dependencies
                .insert(module_name(path), ordered)
                .is_some()
            {
                return Err(vendor_error(format!("duplicate modules.dep entry {path}")));
            }
        }

        for line in list.lines() {
            let Some(token) = line
                .split('#')
                .next()
                .unwrap_or("")
                .split_whitespace()
                .next()
            else {
                continue;
            };
            let name = module_name(token);
            // A list entry that `modules.dep` already describes resolves to the
            // dependency path; otherwise it is a path of its own, as in vendor
            // module directories that ship without `modules.dep`.
            if !modules.paths.contains_key(&name) {
                modules.register(token)?;
            }
            modules.requested.push(name);
            if modules.requested.len() > MAX_MODULES {
                return Err(vendor_error("module load list is too long"));
            }
        }
        // `modules.softdep` is advisory, exactly as Android's libmodprobe treats
        // it: a line it cannot parse is warned about and skipped, and the boot
        // keeps going. Vendor sets ship malformed lines such as a glued
        // `pre:<dep>` marker, which is three tokens where Android wants at
        // least four; failing here would abort a load Android itself completes.
        for (number, line) in softdep.lines().enumerate() {
            let words: Vec<&str> = line
                .split('#')
                .next()
                .unwrap_or("")
                .split_whitespace()
                .collect();
            if words.is_empty() {
                continue;
            }
            if words.len() < 4 || words[0] != "softdep" {
                log::warn!(
                    "Ignoring malformed softdep line {} in modules.softdep: softdep lines must have at least 4 entries",
                    number + 1
                );
                continue;
            }
            let name = words[1];
            let mut pre = false;
            for word in &words[2..] {
                match *word {
                    "pre:" => pre = true,
                    "post:" => pre = false,
                    _ if pre => modules
                        .pre_softdeps
                        .entry(module_name(name))
                        .or_default()
                        .push(module_name(word)),
                    _ => {}
                }
            }
        }
        for line in options.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut words = line.splitn(3, char::is_whitespace);
            if words.next() != Some("options") {
                return Err(vendor_error("invalid modules.options directive"));
            }
            let rest = line["options".len()..].trim_start();
            let (name, params) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if name.is_empty() {
                return Err(vendor_error("missing options module"));
            }
            let params = params.trim();
            if !params.is_empty() {
                modules
                    .options
                    .entry(module_name(name))
                    .or_default()
                    .push(params.to_owned());
            }
        }
        for token in command_line_tokens(cmdline) {
            let Some((name, param)) = token.split_once('.') else {
                continue;
            };
            if param.contains('=') && modules.paths.contains_key(&module_name(name)) {
                modules
                    .options
                    .entry(module_name(name))
                    .or_default()
                    .push(param.to_owned());
            }
        }
        Ok(modules)
    }

    fn load(
        &self,
        mut load: impl FnMut(&str, &str, &str) -> Result<(), Failure>,
    ) -> Result<(), Failure> {
        let mut loaded = BTreeSet::new();
        let mut visiting = BTreeSet::new();

        for name in &self.requested {
            self.visit(name, &mut loaded, &mut visiting, &mut load)?;
        }

        Ok(())
    }

    fn visit(
        &self,
        name: &str,
        loaded: &mut BTreeSet<String>,
        visiting: &mut BTreeSet<String>,
        load: &mut impl FnMut(&str, &str, &str) -> Result<(), Failure>,
    ) -> Result<(), Failure> {
        if loaded.contains(name) {
            return Ok(());
        }

        if visiting.len() >= MAX_DEPENDENCY_DEPTH || !visiting.insert(name.to_owned()) {
            return Err(vendor_error(format!(
                "dependency cycle or excessive depth at {name}"
            )));
        }

        // Always unwind this visit, including failed hard deps, missing paths,
        // and load errors, so a failed soft hint cannot poison later requests.
        let result = (|| {
            let path = self
                .paths
                .get(name)
                .ok_or_else(|| vendor_error(format!("no module path for dependency {name}")))?;

            for dependency in self.dependencies.get(name).into_iter().flatten() {
                self.visit(dependency, loaded, visiting, load)?;
            }

            // Android treats pre-softdeps as hints, including aliases and
            // built-ins without a module path. Never load post-softdeps here.
            for dependency in self.pre_softdeps.get(name).into_iter().flatten() {
                if let Err(error) = self.visit(dependency, loaded, visiting, load) {
                    log::warn!(
                        "Cannot load pre-softdep {dependency} for {name}: {}: {}",
                        error.error,
                        error.detail
                    );
                }
            }

            let params = self
                .options
                .get(name)
                .map(|parts| parts.join(" "))
                .unwrap_or_default();
            load(name, path, &params)?;
            loaded.insert(name.to_owned());
            Ok(())
        })();
        visiting.remove(name);
        result
    }
}

/// Read one metadata file inside the selected module directory.
fn read_vendor_metadata(base: &Path, name: &str, optional: bool) -> Result<String, Failure> {
    let path = base.join(name);

    match fs::symlink_metadata(&path) {
        Err(error) if optional && error.kind() == ErrorKind::NotFound => {
            return Ok(String::new());
        }
        Err(error) => return Err(vendor_error(format!("cannot stat {name}: {error}"))),
        Ok(_) => {}
    }

    let path = confined_file(base, name)?;

    Ok(read_metadata(&path, false)?.unwrap_or_default())
}

/// The vendor module directory and load-list file name selected for this boot.
fn vendor_module_source(
    release: &str,
    mode: BootMode,
) -> Result<Option<(PathBuf, String)>, Failure> {
    let Some(list) = select_vendor_list(release, mode)? else {
        return Ok(None);
    };

    let directory = list
        .parent()
        .ok_or_else(|| vendor_error("module list has no directory"))?;
    let base = fs::canonicalize(directory).map_err(|error| {
        vendor_error(format!("cannot resolve {}: {error}", directory.display()))
    })?;

    let list_name = list
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| vendor_error("invalid module list name"))?
        .to_owned();

    Ok(Some((base, list_name)))
}

/// What one `finit_module` result means for the load sequence.
///
/// A module the kernel already has is success, not a duplicate-load failure,
/// exactly as Android's libmodprobe treats an already loaded module.
fn finit_outcome(result: Result<(), Errno>) -> Result<bool, Errno> {
    match result {
        Ok(()) => Ok(true),
        Err(Errno::EXIST) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Insert one vendor module with `finit_module`.
fn load_vendor_module(
    base: &Path,
    name: &str,
    module_path: &str,
    params: &str,
) -> Result<(), Failure> {
    let path = confined_file(base, module_path)?;
    let file = File::open(&path).map_err(|error| {
        Failure::at(
            Stage::ModuleLoad,
            Some(name),
            "VendorModuleUnreadable",
            format!("cannot open {}: {error}", path.display()),
        )
    })?;
    let params = CString::new(params).map_err(|_| {
        Failure::at(
            Stage::ModuleLoad,
            Some(name),
            "VendorParamsInvalid",
            "vendor module parameters contain NUL",
        )
    })?;

    match finit_outcome(finit_module(&file, &params, 0)) {
        Ok(true) => log::info!("Loaded vendor module {name}"),
        Ok(false) => log::info!("Vendor module {name} is already loaded"),
        Err(error) => {
            return Err(Failure::at(
                Stage::ModuleLoad,
                Some(name),
                "VendorModuleLoad",
                format!("finit_module {} failed: {error}", path.display()),
            ));
        }
    }

    Ok(())
}

/// Load vendor modules in Android list/dependency order, without shell parsing.
pub fn load_vendor_modules() -> Result<(), Failure> {
    let cmdline = read_metadata(Path::new("/proc/cmdline"), false)?.unwrap_or_default();
    let bootconfig = read_metadata(Path::new("/proc/bootconfig"), true)?.unwrap_or_default();
    let mode = classify_boot_mode(
        &bootconfig,
        &cmdline,
        Path::new(RECOVERY_EXECUTABLE).exists(),
    );
    let release =
        read_metadata(Path::new("/proc/sys/kernel/osrelease"), false)?.unwrap_or_default();

    let Some((base, list_name)) = vendor_module_source(&release, mode)? else {
        log::warn!("No vendor modules.load found; PID 1 has no vendor modules to load early");
        return Ok(());
    };

    let modules = VendorModules::parse(
        &read_vendor_metadata(&base, &list_name, false)?,
        &read_vendor_metadata(&base, "modules.dep", true)?,
        &read_vendor_metadata(&base, "modules.softdep", true)?,
        &read_vendor_metadata(&base, "modules.options", true)?,
        &cmdline,
    )?;

    modules.load(|name, module_path, params| load_vendor_module(&base, name, module_path, params))
}

/// Resolve kernel modules from the ramdisk root and helpers from the ESP,
/// rejecting any symbolic link component and any non-regular file.
pub fn resolve_payload_file(
    payload_root: &Path,
    relative: &str,
    component: &str,
) -> Result<PathBuf, Failure> {
    let mut path = if relative.ends_with(".ko") {
        PathBuf::from("/")
    } else {
        payload_root.to_path_buf()
    };

    for part in relative.split('/') {
        path.push(part);

        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Failure::at(
                    Stage::Configuration,
                    Some(component),
                    "PathSymlink",
                    format!("{} is a symbolic link", path.display()),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Err(Failure::at(
                    Stage::ModuleLoad,
                    Some(component),
                    "ModuleFileMissing",
                    format!("{} does not exist", path.display()),
                ));
            }
            Err(error) => {
                return Err(Failure::at(
                    Stage::ModuleLoad,
                    Some(component),
                    "PathUnreadable",
                    format!("cannot access {}: {error}", path.display()),
                ));
            }
        }
    }

    let metadata = fs::metadata(&path).map_err(|error| {
        Failure::at(
            Stage::ModuleLoad,
            Some(component),
            "PathUnreadable",
            format!("cannot stat {}: {error}", path.display()),
        )
    })?;

    if !metadata.is_file() {
        return Err(Failure::at(
            Stage::ModuleLoad,
            Some(component),
            "ModuleFileNotRegular",
            format!("{} is not a regular file", path.display()),
        ));
    }

    Ok(path)
}

/// Resolve symbolic partition parameters from sysfs PARTNAME before Android
/// first-stage init has created any /dev/block/by-name symlinks.
pub fn substitute_by_name_params(params: &str) -> std::io::Result<String> {
    substitute_params(params, |name| {
        let device = esu_platform::block::partition_by_name(name)?;
        Ok((rustix::fs::major(device), rustix::fs::minor(device)))
    })
}

fn substitute_params(
    params: &str,
    mut resolve: impl FnMut(&str) -> std::io::Result<(u32, u32)>,
) -> std::io::Result<String> {
    params
        .split_whitespace()
        .map(|token| {
            let Some((prefix, name)) = token.split_once("by-name:") else {
                return Ok(token.to_owned());
            };
            if name.is_empty() || name.contains('/') || name == "." || name == ".." {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidInput,
                    "invalid by-name partition",
                ));
            }
            let (major, minor) = resolve(name)?;
            Ok(format!("{prefix}{major}:{minor}"))
        })
        .collect::<std::io::Result<Vec<_>>>()
        .map(|tokens| tokens.join(" "))
}

/// Load one ESP payload module through the existing relocation loader.
///
/// `params` are passed to the kernel as module parameters and are never
/// evaluated by a shell.
pub fn load_managed_module(path: &Path, entry: &ModuleEntry) -> Result<(), Failure> {
    let data = fs::read(path).map_err(|error| {
        Failure::at(
            Stage::Storage,
            Some(&entry.name),
            "ModuleUnreadable",
            format!("cannot read {}: {error}", path.display()),
        )
    })?;

    let params = substitute_by_name_params(&entry.params).map_err(|error| {
        Failure::at(
            Stage::ModuleLoad,
            Some(&entry.name),
            "ModuleParamsInvalid",
            error.to_string(),
        )
    })?;
    let params = CString::new(params).map_err(|_| {
        Failure::at(
            Stage::Configuration,
            Some(&entry.name),
            "ModuleParamsInvalid",
            "module parameters contain NUL",
        )
    })?;

    log::info!(
        "Loading payload module {} from {}",
        entry.name,
        path.display()
    );

    crate::load_module(&data, &params).map_err(|error| {
        Failure::at(
            Stage::ModuleLoad,
            Some(&entry.name),
            "ModuleLoad",
            format!("{error:#}"),
        )
    })
}

/// Whether `/sys/module/<name>` already exists, i.e. the module is loaded.
///
/// The kernel normalizes `-` to `_` in module names, so both spellings are
/// checked. A preloaded module is never skipped: it still has to pass the same
/// readiness self-check.
pub fn module_loaded(name: &str) -> bool {
    module_sysfs_path(name).is_some()
}

/// Absolute `/sys/module/<name>` path, when the module is loaded.
pub fn module_sysfs_path(name: &str) -> Option<PathBuf> {
    let normalized = name.replace('-', "_");

    for candidate in [normalized.as_str(), name] {
        let path = Path::new("/sys/module").join(candidate);

        if path.is_dir() {
            return Some(path);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_name_parameters_resolve_each_token() {
        let value = substitute_params("dev=by-name:bdsvars other=by-name:esp flag=1", |name| {
            Ok(match name {
                "bdsvars" => (259, 3),
                "esp" => (8, 16),
                _ => unreachable!(),
            })
        })
        .unwrap();
        assert_eq!(value, "dev=259:3 other=8:16 flag=1");
        assert!(substitute_params("dev=by-name:../escape", |_| Ok((8, 16))).is_err());
        assert!(
            substitute_params("dev=by-name:bdsvars", |_| Err(std::io::Error::from(
                ErrorKind::NotFound
            )))
            .is_err()
        );
    }

    #[test]
    fn sysfs_partition_parameters_need_no_by_name_device_nodes() {
        let sources = vec![
            (
                "DEVTYPE=partition\nPARTNAME=bdsvars\n".into(),
                "259:3\n".into(),
            ),
            ("DEVTYPE=partition\nPARTNAME=esp\n".into(), "8:16\n".into()),
        ];
        let value = substitute_params("dev=by-name:bdsvars other=by-name:esp", |name| {
            let device = esu_platform::block::partition_in(name, &sources)?;
            Ok((rustix::fs::major(device), rustix::fs::minor(device)))
        })
        .unwrap();
        assert_eq!(value, "dev=259:3 other=8:16");
        assert!(
            substitute_params("dev=by-name:missing", |name| {
                let device = esu_platform::block::partition_in(name, &sources)?;
                Ok((rustix::fs::major(device), rustix::fs::minor(device)))
            })
            .is_err()
        );
    }

    /// Record `(name, path, params)` in load order.
    fn order(modules: &VendorModules) -> Result<Vec<(String, String, String)>, Failure> {
        let mut loaded = Vec::new();

        modules.load(|name, path, params| {
            loaded.push((name.to_owned(), path.to_owned(), params.to_owned()));
            Ok(())
        })?;

        Ok(loaded)
    }

    fn names(loaded: &[(String, String, String)]) -> Vec<&str> {
        loaded.iter().map(|(name, _, _)| name.as_str()).collect()
    }

    #[test]
    fn an_already_loaded_module_is_not_a_duplicate_load_failure() {
        assert_eq!(finit_outcome(Ok(())), Ok(true));
        assert_eq!(finit_outcome(Err(Errno::EXIST)), Ok(false));
        assert_eq!(finit_outcome(Err(Errno::INVAL)), Err(Errno::INVAL));
    }

    #[test]
    fn hard_dependencies_then_pre_softdeps_load_first() {
        let modules = VendorModules::parse(
            "kernel/a.ko\n",
            "kernel/a.ko: kernel/b.ko\nkernel/b.ko: kernel/c.ko\nkernel/c.ko:\nkernel/s.ko:\n",
            "softdep a pre: s\nsoftdep a post: p\n",
            "options a foo=1\noptions b bar=2\noptions c\n",
            "c.baz=3 a.qux=4 unrelated=5 notamodule.x=1",
        )
        .unwrap();

        let loaded = order(&modules).unwrap();

        assert_eq!(names(&loaded), ["c", "b", "s", "a"]);
        assert_eq!(
            loaded
                .iter()
                .map(|(_, path, _)| path.as_str())
                .collect::<Vec<_>>(),
            ["kernel/c.ko", "kernel/b.ko", "kernel/s.ko", "kernel/a.ko"]
        );
        assert_eq!(
            loaded
                .iter()
                .map(|(_, _, params)| params.as_str())
                .collect::<Vec<_>>(),
            ["baz=3", "bar=2", "", "foo=1 qux=4"]
        );
        assert!(!names(&loaded).contains(&"p"));
    }

    #[test]
    fn list_order_is_preserved_and_modules_load_once() {
        let modules =
            VendorModules::parse("kernel/a.ko\nkernel/b.ko\nkernel/a.ko\n", "", "", "", "")
                .unwrap();

        assert_eq!(names(&order(&modules).unwrap()), ["a", "b"]);
    }

    #[test]
    fn modules_dep_path_is_authoritative() {
        let modules = VendorModules::parse("a.ko\n", "kernel/a.ko:\n", "", "", "").unwrap();
        let loaded = order(&modules).unwrap();

        assert_eq!(names(&loaded), ["a"]);
        assert_eq!(loaded[0].1, "kernel/a.ko");
    }

    #[test]
    fn list_without_modules_dep_uses_its_own_paths() {
        let modules = VendorModules::parse("kernel/a.ko # note\n", "", "", "", "").unwrap();
        let loaded = order(&modules).unwrap();

        assert_eq!(names(&loaded), ["a"]);
        assert_eq!(loaded[0].1, "kernel/a.ko");
    }

    #[test]
    fn dependency_cycles_are_detected() {
        let modules = VendorModules::parse(
            "kernel/a.ko\n",
            "kernel/a.ko: kernel/b.ko\nkernel/b.ko: kernel/a.ko\n",
            "",
            "",
            "",
        )
        .unwrap();

        assert!(order(&modules).is_err());
    }

    #[test]
    fn missing_dependency_path_is_fatal() {
        let modules =
            VendorModules::parse("kernel/a.ko\n", "kernel/a.ko: kernel/b.ko\n", "", "", "")
                .unwrap();

        assert!(order(&modules).is_err());
    }

    #[test]
    fn duplicate_basenames_are_rejected() {
        assert!(VendorModules::parse("", "kernel/a.ko:\nupdates/a.ko:\n", "", "", "").is_err());
    }

    #[test]
    fn malformed_metadata_is_fatal() {
        assert!(VendorModules::parse("", "kernel/a.ko kernel/b.ko\n", "", "", "").is_err());
        assert!(VendorModules::parse("", "", "", "options\n", "").is_err());
        // `modules.softdep` is advisory, so it is deliberately absent here: a
        // line Android's libmodprobe cannot parse is warned about and skipped.
        assert!(VendorModules::parse("", "", "require a\n", "", "").is_ok());
    }

    #[test]
    fn malformed_softdep_lines_are_skipped_and_the_closure_still_loads() {
        // The vendor set really ships this shape: a glued `pre:<dep>` marker,
        // three tokens where Android's libmodprobe wants at least four. Android
        // warns and keeps loading, so the vendor load must too.
        let dep = "kernel/a.ko: kernel/b.ko\nkernel/b.ko:\nkernel/s.ko:\n";
        let softdep = concat!(
            "softdep a pre:b\n",
            "require unrelated\n",
            "softdep a pre: s\n",
            "softdep a post: b\n",
        );

        let modules = VendorModules::parse("kernel/a.ko\n", dep, softdep, "", "").unwrap();

        // Malformed lines are skipped whole, while the valid pre-softdep
        // survives and a post-softdep is never treated as one.
        assert_eq!(modules.pre_softdeps.get("a"), Some(&vec!["s".to_owned()]));

        // The hard closure still loads, with the pre-softdep after the hard
        // dependencies and before the module itself.
        assert_eq!(names(&order(&modules).unwrap()), ["b", "s", "a"]);
    }

    #[test]
    fn shared_boot_modes_select_vendor_lists() {
        let list =
            |cmdline: &str, recovery: bool| mode_list(classify_boot_mode("", cmdline, recovery));

        assert_eq!(
            list("androidboot.mode=recovery", false),
            "modules.load.recovery"
        );
        assert_eq!(
            list("androidboot.mode=charger", true),
            "modules.load.charger"
        );
        assert_eq!(
            list("androidboot.force_normal_boot=0", false),
            "modules.load"
        );
        assert_eq!(list("", true), "modules.load.recovery");
        assert_eq!(
            list("androidboot.force_normal_boot=1", true),
            "modules.load"
        );
    }

    #[test]
    fn absolute_modules_dep_paths_stay_inside_the_module_directory() {
        let dep = concat!(
            "/lib/modules/6.1.0/kernel/a.ko: /lib/modules/6.1.0/kernel/b.ko\n",
            "/lib/modules/6.1.0/kernel/b.ko:\n",
        );
        let modules =
            VendorModules::parse("/lib/modules/6.1.0/kernel/a.ko\n", dep, "", "", "").unwrap();

        let loaded = order(&modules).unwrap();

        assert_eq!(names(&loaded), ["b", "a"]);
        assert_eq!(loaded[0].1, "/lib/modules/6.1.0/kernel/b.ko");
        assert_eq!(loaded[1].1, "/lib/modules/6.1.0/kernel/a.ko");

        // Traversal in depmod-generated paths is still rejected.
        assert!(VendorModules::parse("", "/lib/modules/../a.ko:\n", "", "", "").is_err());
        assert!(
            VendorModules::parse("", "/lib/modules/6.1.0/kernel/../../a.ko:\n", "", "", "")
                .is_err()
        );
    }

    #[test]
    fn module_names_are_normalized() {
        assert_eq!(module_name("kernel/drivers/foo-bar.ko"), "foo_bar");
        assert_eq!(module_name("foo"), "foo");
        assert_eq!(module_name(""), "");
    }

    #[test]
    fn module_paths_stay_inside_the_module_directory() {
        assert!(validate_module_path("kernel/a.ko").is_ok());
        assert!(validate_module_path("/lib/modules/kernel/a.ko").is_ok());
        assert!(validate_module_path("/vendor/lib/modules/a.ko").is_ok());
        assert!(validate_module_path("").is_err());
        assert!(validate_module_path("../a.ko").is_err());
        assert!(validate_module_path("kernel/../../a.ko").is_err());
        assert!(validate_module_path("/lib/modules/../a.ko").is_err());
        assert!(validate_module_path("./a.ko").is_err());
    }

    #[test]
    fn confined_files_cannot_escape_the_module_directory() {
        let root = std::env::temp_dir().join(format!("esu-loader-{}", std::process::id()));
        let base = root.join("modules");
        let outside = root.join("outside");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(base.join("kernel")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(base.join("kernel/a.ko"), b"module").unwrap();
        fs::write(outside.join("a.ko"), b"module").unwrap();
        std::os::unix::fs::symlink(outside.join("a.ko"), base.join("link.ko")).unwrap();

        let base = fs::canonicalize(&base).unwrap();
        let absolute_inside = base.join("kernel/a.ko");
        let absolute_outside = fs::canonicalize(outside.join("a.ko")).unwrap();

        // Depmod-generated absolute paths are accepted when they resolve inside.
        assert_eq!(
            confined_file(&base, absolute_inside.to_str().unwrap()).unwrap(),
            absolute_inside
        );
        assert!(confined_file(&base, "kernel/a.ko").is_ok());
        // Absolute paths, traversal, and symlinks leaving the directory are not.
        assert!(confined_file(&base, absolute_outside.to_str().unwrap()).is_err());
        assert!(confined_file(&base, "../outside/a.ko").is_err());
        assert!(confined_file(&base, "link.ko").is_err());
        assert!(confined_file(&base, "kernel/missing.ko").is_err());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn command_line_tokens_keep_quoted_values() {
        assert_eq!(
            command_line_tokens("a=1 b=\"c d\" e"),
            ["a=1", "b=\"c d\"", "e"]
        );
        assert_eq!(command_line_tokens("b=\"c"), ["b=\"c"]);
        assert_eq!(command_line_tokens("  "), Vec::<&str>::new());
    }
}
