// SPDX-License-Identifier: GPL-3.0-only
//! Shared ESP package contract. Kernel-module loading remains in manifest.modules.
pub mod generation;
pub mod staging;
pub mod tiny;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Component, Path};

pub const ROOT: &str = "/metadata/espinit";
pub const HAL: &str = "modules/boot-hal/android.hardware.boot-service.gblbds";
pub const HELPER: &str = "modules/tiny-espsu/tiny-espsu";
pub const HAL_RC: &str = "initrc/boot-gblbds.rc";
const MAX_TEXT: u64 = 64 * 1024;
const MAX_BINARY: u64 = 64 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    pub metadata_filesystem: String,
    pub packages: Vec<String>,
    #[serde(default)]
    pub recovery_packages: Vec<String>,
}

impl Platform {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.metadata_filesystem.as_str(), "ext4" | "f2fs"),
            "unsupported metadata filesystem"
        );
        let mut ids = BTreeSet::new();
        for id in &self.packages {
            identifier(id)?;
            ensure!(ids.insert(id.as_str()), "duplicate package id {id}");
        }
        let mut recovery_ids = BTreeSet::new();
        for id in &self.recovery_packages {
            identifier(id)?;
            ensure!(
                id != "boot-hal" && id != "tiny-espsu",
                "normal HAL packages cannot run in recovery"
            );
            ensure!(
                recovery_ids.insert(id),
                "duplicate recovery package id {id}"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub schema_version: u64,
    pub generation: String,
    pub id: String,
    pub files: Vec<PackageFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageFile {
    pub source: String,
    /// Relative to this module's installed directory, never the system root.
    pub destination: String,
    pub mode: String,
    pub kind: Kind,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Binary,
    Script,
    Policy,
    Initrc,
    Data,
}

pub fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 64
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "invalid identifier {value:?}"
    );
    Ok(())
}

pub fn relative(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 4096,
        "invalid path length"
    );
    for part in value.split('/') {
        identifier(part).with_context(|| format!("unsafe relative path {value:?}"))?;
    }
    Ok(())
}

fn validate_generation(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 63
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
        "invalid generation"
    );
    Ok(())
}

pub fn parse_package(text: &str, id: &str, generation: &str) -> Result<Package> {
    let package: Package = toml::from_str(text).context("module.toml parse")?;
    identifier(id)?;
    identifier(&package.id)?;
    validate_generation(&package.generation)?;
    ensure!(package.schema_version == 1, "unknown package schema");
    ensure!(package.id == id, "package id disagrees with directory/list");
    ensure!(
        package.generation == generation,
        "package generation mismatch"
    );
    ensure!(!package.files.is_empty(), "empty package");
    let mut destinations = BTreeSet::new();
    for file in &package.files {
        relative(&file.source)?;
        relative(&file.destination)?;
        ensure!(
            !matches!(
                file.destination.as_str(),
                "module.toml" | "disable" | "remove" | "update"
            ),
            "reserved destination"
        );
        let executable = matches!(file.kind, Kind::Binary | Kind::Script);
        ensure!(
            file.mode == if executable { "0755" } else { "0644" },
            "invalid mode for {:?}",
            file.kind
        );
        if file.kind == Kind::Initrc {
            ensure!(
                file.destination.starts_with("initrc/") && file.destination.ends_with(".rc"),
                "initrc must be installed in initrc/*.rc"
            );
            ensure!(
                file.destination.split('/').count() == 2,
                "nested initrc is not supported"
            );
        } else {
            ensure!(
                !file.destination.starts_with("initrc/"),
                "non-initrc file in initrc directory"
            );
        }
        ensure!(
            destinations.insert(file.destination.as_str()),
            "duplicate destination"
        );
    }
    check_prefixes(destinations.iter().copied())?;
    Ok(package)
}

fn check_prefixes<'a>(paths: impl Iterator<Item = &'a str>) -> Result<()> {
    let paths: BTreeSet<_> = paths.collect();
    for path in &paths {
        for (offset, _) in path.match_indices('/') {
            ensure!(
                !paths.contains(&path[..offset]),
                "file/directory destination collision at {path}"
            );
        }
    }
    Ok(())
}

/// Open each path component using directory descriptors. No symlink is followed,
/// including ancestors of the supplied root; the opened inode survives renames.
pub fn open_root(path: &Path) -> Result<File> {
    ensure!(path.is_absolute(), "root must be absolute");
    let mut directory = File::open("/")?;
    for component in path.components() {
        match component {
            Component::RootDir => (),
            Component::Normal(name) => {
                let name = name.to_str().context("non-UTF8 root")?;
                directory = open_at(&directory, name, true)?;
            }
            _ => bail!("non-normal root component"),
        }
    }
    Ok(directory)
}

fn open_at(directory: &File, name: &str, is_dir: bool) -> Result<File> {
    let name = CString::new(name)?;
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if is_dir { libc::O_DIRECTORY } else { 0 };
    // SAFETY: live dirfd and a NUL-terminated one-component name.
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    ensure!(
        fd >= 0,
        "openat failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: successful openat transfers ownership of this fd.
    let file = unsafe { File::from_raw_fd(fd) };
    ensure!(
        if is_dir {
            file.metadata()?.is_dir()
        } else {
            file.metadata()?.is_file()
        },
        "unexpected inode type"
    );
    Ok(file)
}

pub fn open_file(root: &File, path: &str) -> Result<File> {
    relative(path)?;
    let mut directory = root.try_clone()?;
    let mut components = path.split('/').peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            return open_at(&directory, component, false).with_context(|| path.to_owned());
        }
        directory = open_at(&directory, component, true)?;
    }
    bail!("empty path")
}

fn bytes(file: &mut File, max: u64) -> Result<Vec<u8>> {
    let size = file.metadata()?.len();
    ensure!(size > 0 && size <= max, "empty or oversized package file");
    file.seek(SeekFrom::Start(0))?;
    let mut data = Vec::with_capacity(size as usize);
    (&mut *file).take(max + 1).read_to_end(&mut data)?;
    ensure!(data.len() as u64 == size, "file changed during planning");
    file.seek(SeekFrom::Start(0))?;
    Ok(data)
}

pub fn check_artifact(file: &mut File, generation: &str) -> Result<()> {
    let data = bytes(file, MAX_BINARY)?;
    let elf = goblin::elf::Elf::parse(&data).context("package binary is not ELF")?;
    ensure!(
        elf.is_64 && elf.little_endian && matches!(elf.header.e_machine, 62 | 183),
        "unsupported package architecture"
    );
    ensure!(matches!(elf.header.e_type, 2 | 3), "not an executable ELF");
    let mut sections = elf
        .section_headers
        .iter()
        .filter(|section| elf.shdr_strtab.get_at(section.sh_name) == Some(".note.espinit"));
    let section = sections
        .next()
        .context("binary generation note is missing")?;
    ensure!(
        sections.next().is_none(),
        "duplicate binary generation note"
    );
    let start = usize::try_from(section.sh_offset)?;
    let end = start
        .checked_add(usize::try_from(section.sh_size)?)
        .context("note bounds overflow")?;
    let note = data.get(start..end).context("note outside ELF")?;
    ensure!(
        note.len() == 84
            && note[..20]
                == [
                    8, 0, 0, 0, 64, 0, 0, 0, 1, 0, 0, 0, b'E', b'S', b'P', b'I', b'N', b'I', b'T',
                    0
                ],
        "invalid generation note"
    );
    let size = note[20..]
        .iter()
        .position(|b| *b == 0)
        .context("unterminated generation")?;
    ensure!(
        note[20 + size..].iter().all(|b| *b == 0),
        "invalid generation padding"
    );
    ensure!(
        &note[20..20 + size] == generation.as_bytes(),
        "binary generation mismatch"
    );
    Ok(())
}

pub enum Contents {
    Source(File),
    Generated(Vec<u8>),
}
pub struct PlannedFile {
    pub destination: String,
    pub mode: u32,
    pub contents: Contents,
}
pub struct Plan {
    pub files: Vec<PlannedFile>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootMode {
    Managed,
    Unmanaged,
    Recovery,
}

/// All sources are opened and validated before metadata is mounted or changed.
/// BTreeMap orders the publication independently of input package/file ordering.
pub fn plan(
    payload: &Path,
    platform: &Platform,
    generation: &str,
    rom: &str,
    mode: BootMode,
) -> Result<Plan> {
    platform.validate()?;
    validate_generation(generation)?;
    // Recovery receives a separate complete snapshot, so no previous normal
    // generation's daemon/modules.rc can accidentally start the normal HAL.
    let selected_packages = if mode == BootMode::Recovery {
        &platform.recovery_packages
    } else {
        &platform.packages
    };
    let root = open_root(payload)?;
    let mut files = BTreeMap::new();
    let mut daemon = open_file(&root, "bin/espinitd")?;
    check_artifact(&mut daemon, generation)?;
    files.insert("espinitd".to_owned(), (0o755, Contents::Source(daemon)));
    files.insert(
        "rom.toml".to_owned(),
        (0o644, Contents::Source(open_file(&root, rom)?)),
    );
    let mut initrc = BTreeMap::new();
    for id in selected_packages {
        if mode == BootMode::Unmanaged && matches!(id.as_str(), "boot-hal" | "tiny-espsu") {
            continue;
        }
        let manifest = format!("modules/{id}/module.toml");
        let mut source = open_file(&root, &manifest)?;
        let text = bytes(&mut source, MAX_TEXT)?;
        let package = parse_package(std::str::from_utf8(&text)?, id, generation)?;
        files.insert(manifest, (0o644, Contents::Source(source)));
        for entry in package.files {
            let destination = format!("modules/{id}/{}", entry.destination);
            let mut source = open_file(&root, &format!("modules/{id}/{}", entry.source))?;
            ensure!(source.metadata()?.len() > 0, "empty package file");
            if entry.kind == Kind::Binary {
                check_artifact(&mut source, generation)?;
            }
            if destination == HAL || destination == HELPER {
                ensure!(
                    entry.kind == Kind::Binary,
                    "platform entrypoint must be a generation-checked binary"
                );
            }
            if entry.kind == Kind::Initrc {
                let text = bytes(&mut source, MAX_TEXT)?;
                std::str::from_utf8(&text).context("initrc is not UTF8")?;
                initrc.insert(destination.clone(), text);
            }
            let mode = if entry.mode == "0755" { 0o755 } else { 0o644 };
            ensure!(
                files
                    .insert(destination, (mode, Contents::Source(source)))
                    .is_none(),
                "duplicate installation destination"
            );
        }
    }
    if mode == BootMode::Managed {
        ensure!(
            files.contains_key(HAL) && files.contains_key(HELPER),
            "platform binaries are missing"
        );
        ensure!(
            initrc.contains_key(&format!("modules/boot-hal/{HAL_RC}")),
            "boot HAL initrc is missing"
        );
    }
    let mut rc = b"# Generated ESP package services\n".to_vec();
    for (path, text) in initrc {
        rc.extend_from_slice(format!("\n# ESP package {path}\n").as_bytes());
        rc.extend_from_slice(&text);
        rc.push(b'\n');
    }
    ensure!(
        rc.len() <= MAX_TEXT as usize,
        "combined module RC exceeds limit"
    );
    files.insert(
        "initrc/modules.rc".to_owned(),
        (0o644, Contents::Generated(rc)),
    );
    check_prefixes(files.keys().map(String::as_str))?;
    Ok(Plan {
        files: files
            .into_iter()
            .map(|(destination, (mode, contents))| PlannedFile {
                destination,
                mode,
                contents,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const PACKAGE: &str = "schema_version=1\ngeneration='g1'\nid='boot-hal'\n[[files]]\nsource='hal'\ndestination='hal'\nmode='0755'\nkind='binary'\n";
    #[test]
    fn strict_package_fields_identity_modes_and_paths() {
        assert!(parse_package(PACKAGE, "boot-hal", "g1").is_ok());
        for text in [
            PACKAGE.replace("schema_version=1", "schema_version=2"),
            format!("unexpected=1\n{PACKAGE}"),
            PACKAGE.replace("0755", "04755"),
            format!("generation='g1'\n{PACKAGE}"),
            PACKAGE.replace("mode='0755'\n", ""),
            PACKAGE.replace("destination='hal'", "destination='module.toml'"),
            PACKAGE.replace("0755", "0777"),
            PACKAGE.replace("source='hal'", "source='../hal'"),
            PACKAGE.replace("destination='hal'", "destination='/hal'"),
            format!("{PACKAGE}extra=true\n"),
            PACKAGE.replace("kind='binary'", "kind='unknown'"),
        ] {
            assert!(parse_package(&text, "boot-hal", "g1").is_err(), "{text}");
        }
        assert!(parse_package(PACKAGE, "other", "g1").is_err());
        assert!(parse_package(PACKAGE, "boot-hal", "g2").is_err());
        let second = "\n[[files]]\nsource='other'\ndestination='hal'\nmode='0644'\nkind='data'\n";
        assert!(parse_package(&format!("{PACKAGE}{second}"), "boot-hal", "g1").is_err());
        assert!(
            parse_package(
                &format!(
                    "{PACKAGE}{}",
                    second.replace("destination='hal'", "destination='hal/child'")
                ),
                "boot-hal",
                "g1"
            )
            .is_err()
        );
    }
    #[test]
    fn path_and_package_list_rejections() {
        for path in [
            "", "/a", "../a", "a/../b", "a//b", "a/", "./a", "a\\b", "a\0b", "a b",
        ] {
            assert!(relative(path).is_err(), "{path:?}");
        }
        let mut platform = Platform {
            metadata_filesystem: "ext4".into(),
            packages: vec!["boot-hal".into(), "tiny-espsu".into()],
            recovery_packages: Vec::new(),
        };
        assert!(platform.validate().is_ok());
        platform.packages.push("boot-hal".into());
        assert!(platform.validate().is_err());
        platform.packages.pop();
        platform.metadata_filesystem = "auto".into();
        assert!(platform.validate().is_err());
    }
}
