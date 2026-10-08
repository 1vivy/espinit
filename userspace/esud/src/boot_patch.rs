//! Explicit-input, host-only packaging. No device discovery or Android runtime calls.
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use esu_config as config;
use esu_platform as platform;
use goblin::elf::{Elf, header, program_header};
use ota_core::Kmi;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const VERIFIER: &[u8] = include_bytes!("../../../scripts/kmi_modules.py");
const MAX_BINARY: u64 = 64 * 1024 * 1024;

/// The configuration every `lvm` invocation passes with `--config`, shipped as
/// `esu/bin/lvm.conf`. Embedded so the payload copy is compared against the
/// tools' own file instead of a second hand-written string: the two cannot
/// drift, because a change to `tools/lvm2/lvm.conf` changes this constant.
const LVM_CONF: &[u8] = include_bytes!("../../../tools/lvm2/lvm.conf");

#[derive(clap::Args, Debug)]
pub struct BootPatchArgs {
    /// Static ELF PID-1 binary (never executed)
    #[arg(long)]
    pub esuinit: PathBuf,
    /// Complete payload root containing manifest.toml, roms/, bin/, and modules/
    #[arg(long)]
    pub payload: PathBuf,
    /// Kernel modules and schema-2 compatibility receipts for the ramdisk
    #[arg(long)]
    pub modules_dir: PathBuf,
    /// KMI reference output used to verify the captured modules
    #[arg(long)]
    pub kmi_out: PathBuf,
    /// Explicit ROM ID selected from the manifest's ROM directory
    #[arg(long)]
    pub rom: String,
    /// New output directory; must not exist, and its parent must already exist
    #[arg(long)]
    pub out: PathBuf,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
struct Artifact {
    sha256: String,
    size: u64,
    mode: u32,
}

#[derive(Serialize)]
struct Receipt {
    schema_version: u32,
    tool: &'static str,
    tool_version: &'static str,
    verifier_sha256: String,
    rom: String,
    build_id: String,
    build_id_inputs: BTreeMap<String, String>,
    archive_path: String,
    kmi: ReceiptKmi,
    sources: BTreeMap<String, Artifact>,
    artifacts: BTreeMap<String, Artifact>,
    directories: Vec<String>,
    module_verification: serde_json::Value,
}

/// The KMI the emitted module set and overlay were built for.
#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct ReceiptKmi {
    branch: String,
    generation: u32,
}

/// One `set.json`, the manifest `select_module_set` verifies on the device.
#[derive(Serialize)]
struct ModuleSetManifest {
    schema_version: u32,
    kmi: ReceiptKmi,
    modules: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CompatibilityReceipt {
    schema_version: u32,
    kmi: serde_json::Value,
    kmi_out_inputs: BTreeMap<String, String>,
    module_sha256: String,
    imports: serde_json::Value,
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn digest(bytes: &[u8]) -> String {
    lowercase_hex(&Sha256::digest(bytes))
}

fn artifact_build_id<'a>(hashes: impl Iterator<Item = &'a str>) -> String {
    let mut hashes: Vec<_> = hashes.collect();
    hashes.sort_unstable();
    let mut digest = Sha256::new();
    for hash in hashes {
        digest.update(hash.as_bytes());
        digest.update(b"\n");
    }
    lowercase_hex(&digest.finalize())[..12].to_owned()
}

fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    ensure!(
        path.components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_))),
        "path must not contain dot or parent components"
    );
    Ok(path)
}

fn input_file(path: &Path) -> Result<File> {
    let path = absolute(path)?;
    let root = platform::open_root(path.parent().context("input has no parent")?)?;
    platform::open_file(
        &root,
        path.file_name()
            .and_then(|name| name.to_str())
            .context("invalid input name")?,
    )
}

fn read_bounded(mut file: File, max: u64) -> Result<Vec<u8>> {
    let size = file.metadata()?.len();
    ensure!(size > 0 && size <= max, "empty or oversized input");
    let mut data = Vec::with_capacity(usize::try_from(size)?);
    (&mut file).take(max + 1).read_to_end(&mut data)?;
    ensure!(data.len() as u64 == size, "input changed while reading");
    Ok(data)
}

fn write_file(path: &Path, data: &[u8], mode: u32) -> Result<Artifact> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    file.write_all(data)?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.sync_all()?;
    Ok(Artifact {
        sha256: digest(data),
        size: data.len() as u64,
        mode,
    })
}

fn make_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn reserve_name(directory: &Path, expected: &str) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let name = entry?.file_name();
        let name = name.to_str().context("non-UTF8 payload name")?;
        ensure!(
            !name.eq_ignore_ascii_case(expected) || name == expected,
            "case-folding collision with generated {expected}"
        );
    }
    Ok(())
}

/// Copy through descriptor-relative, no-follow opens. Even unreferenced files
/// are checked: a hidden symlink or orphan .ko must never enter a provisionable ESP.
fn snapshot(
    source: &Path,
    root: &File,
    relative: &str,
    target: &Path,
    files: &mut BTreeMap<String, Artifact>,
) -> Result<()> {
    let directory = platform::open_root(&source.join(relative))?;
    let mut entries = fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut folded = BTreeSet::new();
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-UTF8 payload name"))?;
        platform::identifier(&name)?;
        ensure!(!name.ends_with('.'), "trailing dot is ambiguous on FAT");
        ensure!(
            folded.insert(name.to_ascii_lowercase()),
            "case-folding collision on ESP"
        );
        let path = if relative.is_empty() {
            name
        } else {
            format!("{relative}/{name}")
        };
        let kind = entry.file_type()?;
        ensure!(!kind.is_symlink(), "payload symlink is forbidden: {path}");
        let destination = target.join(&path);
        if kind.is_dir() {
            make_dir(&destination)?;
            snapshot(source, root, &path, target, files)?;
        } else {
            ensure!(
                kind.is_file(),
                "payload contains a non-regular file: {path}"
            );
            let mut input = platform::open_file(root, &path)?;
            let metadata = input.metadata()?;
            let mode = if metadata.permissions().mode() & 0o111 == 0 {
                0o644
            } else {
                0o755
            };
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(destination)?;
            let mut hash = Sha256::new();
            let mut buffer = [0u8; 64 * 1024];
            let mut size = 0u64;
            loop {
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                output.write_all(&buffer[..count])?;
                hash.update(&buffer[..count]);
                size += count as u64;
            }
            ensure!(
                size == metadata.len(),
                "payload changed while copying: {path}"
            );
            output.set_permissions(fs::Permissions::from_mode(mode))?;
            output.sync_all()?;
            files.insert(
                path,
                Artifact {
                    sha256: lowercase_hex(&hash.finalize()),
                    size,
                    mode,
                },
            );
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Linkage {
    DynamicAllowed,
    StaticRequired,
}

fn executable(data: &[u8], machine: Option<u16>, linkage: Linkage) -> Result<u16> {
    let elf = Elf::parse(data).context("executable ELF")?;
    ensure!(
        elf.is_64 && elf.little_endian && matches!(elf.header.e_machine, 62 | 183),
        "unsupported executable architecture"
    );
    ensure!(
        matches!(elf.header.e_type, header::ET_EXEC | header::ET_DYN),
        "not an executable ELF"
    );
    let mut entry_backed = false;
    for segment in elf
        .program_headers
        .iter()
        .filter(|segment| segment.p_type == program_header::PT_LOAD)
    {
        ensure!(
            segment.p_filesz <= segment.p_memsz
                && segment
                    .p_offset
                    .checked_add(segment.p_filesz)
                    .is_some_and(|end| end <= data.len() as u64),
            "ELF load segment is outside input"
        );
        let end = segment
            .p_vaddr
            .checked_add(segment.p_filesz)
            .context("ELF address overflow")?;
        entry_backed |= segment.p_flags & program_header::PF_X != 0
            && (segment.p_vaddr..end).contains(&elf.entry);
    }
    ensure!(
        entry_backed,
        "ELF entry point lacks an executable file-backed load segment"
    );
    if let Some(machine) = machine {
        ensure!(
            elf.header.e_machine == machine,
            "payload architecture mismatch"
        );
    }
    if matches!(linkage, Linkage::StaticRequired) {
        ensure!(
            elf.interpreter.is_none() && elf.libraries.is_empty(),
            "early executable must be statically linked"
        );
    }
    Ok(elf.header.e_machine)
}

fn check_binary(path: &Path, machine: u16, linkage: Linkage) -> Result<()> {
    executable(
        &read_bounded(input_file(path)?, MAX_BINARY)?,
        Some(machine),
        linkage,
    )?;
    Ok(())
}

fn configs(payload: &Path, id: &str) -> Result<(config::Manifest, config::RomConfig, String)> {
    let manifest = config::parse_manifest(&fs::read_to_string(payload.join("manifest.toml"))?)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    // The id names the file below the manifest's ROM directory. Only an
    // identifier may do so, and the rejection keeps the shared schema's
    // `RomIdInvalid` classification, so a traversal or an over-long id never
    // reaches a read.
    ensure!(config::rom_id(id), "RomIdInvalid: {id:?}");
    let rom_path = config::rom_config_path(&manifest, id);
    ensure!(
        rom_path.len() <= config::MAX_PATH_BYTES,
        "PathTooLong: manifest ROM path is {} bytes",
        rom_path.len()
    );
    let rom = config::parse_selected_rom(&fs::read_to_string(payload.join(&rom_path))?, id)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    config::validate_managed(&manifest, &rom).map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok((manifest, rom, rom_path))
}

fn validate_payload(
    payload: &Path,
    manifest: &config::Manifest,
    machine: u16,
    files: &BTreeMap<String, Artifact>,
) -> Result<()> {
    ensure!(
        !files.keys().any(|path| path.ends_with(".ko")),
        "kernel modules must be supplied in --modules-dir, not the ESP payload"
    );
    ensure!(
        !files.contains_key("build-id"),
        "build-id is generated by boot-patch"
    );
    let root = platform::open_root(payload)?;
    for (path, linkage) in [
        ("bin/esud", Linkage::DynamicAllowed),
        ("bin/busybox", Linkage::StaticRequired),
        ("bin/thin-activate", Linkage::StaticRequired),
        ("bin/lvm", Linkage::StaticRequired),
        ("bin/ota-stage", Linkage::StaticRequired),
    ] {
        check_binary(&payload.join(path), machine, linkage)?;
        ensure!(
            files.get(path).is_some_and(|file| file.mode == 0o755),
            "payload binary is not executable: {path}"
        );
    }
    // Every lvm invocation passes this text with --config, so the payload copy
    // is what runs; it must be the tools' file byte for byte, not a paraphrase.
    let conf = read_bounded(platform::open_file(&root, "bin/lvm.conf")?, MAX_BINARY)?;
    ensure!(
        conf == LVM_CONF,
        "payload bin/lvm.conf differs from tools/lvm2/lvm.conf"
    );
    ensure!(
        files
            .get("bin/lvm.conf")
            .is_some_and(|file| file.mode == 0o644),
        "payload bin/lvm.conf is not a regular 0644 file"
    );
    for path in files.keys().filter(|path| path.starts_with("bin/")) {
        let bytes = read_bounded(platform::open_file(&root, path)?, MAX_BINARY)?;
        if bytes.starts_with(b"\x7fELF") {
            executable(
                &bytes,
                Some(machine),
                if matches!(path.as_str(), "bin/fw-views" | "bin/avb-graft") {
                    Linkage::StaticRequired
                } else {
                    Linkage::DynamicAllowed
                },
            )?;
        }
    }
    for id in &manifest.modules_order {
        let directory = payload.join("modules").join(id);
        let esuinit::platform::ModulePolicy::Admitted { critical } =
            esuinit::platform::module_policy(&directory, false)
                .map_err(|failure| anyhow::anyhow!("{}: {}", failure.error, failure.detail))?
        else {
            continue;
        };
        let metadata = (|| -> Result<()> {
            let prop = fs::read_to_string(directory.join("module.prop"))?;
            ensure!(
                prop.lines().any(|line| line == format!("id={id}")),
                "module.prop id mismatch: {id}"
            );
            let attrs = directory.join("attrs");
            if attrs.exists() {
                crate::overlay::parse_attrs(&fs::read_to_string(attrs)?)?;
            }
            let policy = directory.join("sepolicy.rule");
            if policy.exists() {
                crate::sepolicy::check_rule(&fs::read_to_string(policy)?)?;
            }
            Ok(())
        })();
        if let Err(error) = metadata {
            if critical {
                return Err(error.context(format!("critical module {id}")));
            }
            eprintln!("warning: optional module {id}: {error:#}");
        }
        for partition in crate::overlay::PARTITIONS {
            let prefix = format!("modules/{id}/{partition}/");
            for path in files.keys().filter(|path| path.starts_with(&prefix)) {
                let bytes = read_bounded(platform::open_file(&root, path)?, MAX_BINARY)?;
                if bytes.starts_with(b"\x7fELF") {
                    executable(&bytes, Some(machine), Linkage::DynamicAllowed)?;
                }
            }
        }
    }
    let prefix = format!("{}/", manifest.rom);
    for path in files
        .keys()
        .filter(|path| path.starts_with(&prefix) && path.ends_with(".toml"))
    {
        let id = path
            .strip_prefix(&prefix)
            .and_then(|name| name.strip_suffix(".toml"))
            .context("invalid ROM path")?;
        let other = config::parse_selected_rom(&fs::read_to_string(payload.join(path))?, id)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        config::validate_managed(manifest, &other).map_err(|error| anyhow::anyhow!("{error}"))?;
        let image_prefix = format!("rom/{id}/");
        for partition in &other.partitions {
            if let Some(path) = partition.backend.strip_prefix("esp-file:") {
                if path.starts_with(&image_prefix) {
                    continue;
                }
                let path = path.strip_prefix("esu/").context(
                    "ESP-file backend must be in the supplied esu payload or owned ROM tree",
                )?;
                ensure!(
                    platform::open_file(&root, path)?.metadata()?.len() > 0,
                    "empty ESP backend"
                );
            }
        }
    }
    Ok(())
}

fn verify_modules(
    modules_dir: &Path,
    kmi_out: &Path,
    manifest: &config::Manifest,
    machine: u16,
) -> Result<serde_json::Value> {
    let verifier = tempfile::tempdir()?;
    let script = verifier.path().join("kmi_modules.py");
    write_file(&script, VERIFIER, 0o644)?;
    let mut command = Command::new("python3");
    command
        .arg("-I")
        .arg(script)
        .arg("verify")
        .arg("--kmi-out")
        .arg(kmi_out);
    let mut declared = BTreeSet::new();
    for module in &manifest.modules {
        ensure!(
            module.path == format!("lib/{}.ko", module.name),
            "kernel module must use lib/<name>.ko: {}",
            module.path
        );
        ensure!(declared.insert(&module.path), "duplicate module path");
        let name = format!("{}.ko", module.name);
        let path = modules_dir.join(&name);
        let bytes = input_file(&path)
            .and_then(|file| read_bounded(file, MAX_BINARY))
            .with_context(|| format!("required manifest module missing: {}", module.path))?;
        let elf = Elf::parse(&bytes)?;
        ensure!(
            elf.header.e_type == header::ET_REL && elf.header.e_machine == machine,
            "module architecture/type mismatch"
        );
        let receipt: CompatibilityReceipt = serde_json::from_slice(&read_bounded(
            input_file(&modules_dir.join(format!("{name}.compat.json")))?,
            MAX_BINARY,
        )?)?;
        ensure!(
            receipt.schema_version == 2 && receipt.module_sha256 == digest(&bytes),
            "module/compatibility receipt mismatch: {}",
            module.name
        );
        command.arg("--module").arg(path);
    }
    ensure!(
        [
            "lib/kernelesp.ko",
            "lib/thin.ko",
            "lib/gpt.ko",
            "lib/efivarfs.ko",
            "lib/efivar_store.ko"
        ]
        .iter()
        .all(|path| declared.contains(&path.to_string())),
        "all four kernel modules are required"
    );
    let output = command
        .output()
        .context("run embedded kmi_modules.py (Python 3.11+ required)")?;
    ensure!(
        output.status.success(),
        "KMI module admission failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        report["status"] == "accepted",
        "KMI verifier did not accept modules"
    );
    Ok(report)
}

fn sync_tree(path: &Path, relative: &str, directories: &mut Vec<String>) -> Result<()> {
    let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        if entry.file_type()?.is_dir() {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("invalid output name"))?;
            let child = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            directories.push(child.clone());
            sync_tree(&entry.path(), &child, directories)?;
        }
    }
    File::open(path)?.sync_all()?;
    Ok(())
}

fn publish(stage: &Path, output: &Path) -> Result<()> {
    let source = CString::new(stage.as_os_str().as_encoded_bytes())?;
    let destination = CString::new(output.as_os_str().as_encoded_bytes())?;
    // SAFETY: both paths are NUL-terminated and live for the syscall. NOREPLACE
    // keeps a racing output creator (or source path) intact, even if empty.
    let status = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    ensure!(
        status == 0,
        "atomic output publication failed: {}",
        std::io::Error::last_os_error()
    );
    File::open(output.parent().context("output parent")?)?.sync_all()?;
    Ok(())
}

/// The KMI the embedded verifier accepted, as the module-set directory names
/// it. The verifier's report is the only place the accepted identity is stated,
/// and `ota_core::kmi::from_parts` refuses an identity that could not name a
/// directory.
fn verified_kmi(report: &serde_json::Value) -> Result<Kmi> {
    let branch = report["kmi"]["branch"]
        .as_str()
        .context("KMI verifier reported no branch")?;
    let generation = report["kmi"]["generation"]
        .as_u64()
        .and_then(|generation| u32::try_from(generation).ok())
        .context("KMI verifier reported no generation")?;
    ota_core::kmi::from_parts(branch, generation)
}

/// Place the verified module set under `esu/kmi/<branch>-<generation>/` and
/// return every file it wrote, keyed by its path below `esp/`.
///
/// The set is generated, never taken from the payload: an operator-supplied set
/// would be a second, unverified statement of the same modules, and the device
/// selects from this tree alone.
fn place_module_set(
    payload_root: &Path,
    kmi: &Kmi,
    manifest: &config::Manifest,
    captured: &Path,
) -> Result<BTreeMap<String, Artifact>> {
    let directory = ota_core::set_dir(payload_root, kmi);
    let relative = directory
        .strip_prefix(payload_root)
        .context("module set is outside the payload tree")?;
    ensure!(
        !directory.exists(),
        "the payload must not carry a module set; it is generated from --modules-dir"
    );
    make_dir(&directory)?;
    make_dir(&directory.join(ota_core::modules::LIB_DIR))?;
    let mut written = BTreeMap::new();
    let mut modules = BTreeMap::new();
    for entry in &manifest.modules {
        let name = format!("{}.ko", entry.name);
        let bytes = read_bounded(input_file(&captured.join(&name))?, MAX_BINARY)?;
        modules.insert(name.clone(), digest(&bytes));
        let path = format!("{}/{}", ota_core::modules::LIB_DIR, name);
        let artifact = write_file(&directory.join(&path), &bytes, 0o644)?;
        written.insert(format!("esp/{}/{}", relative.display(), path), artifact);
        // The schema-2 receipt travels with the module it verified, so the set
        // is self-describing wherever it is later read from.
        let receipt = read_bounded(
            input_file(&captured.join(format!("{name}.compat.json")))?,
            MAX_BINARY,
        )?;
        let path = format!("{name}.compat.json");
        let artifact = write_file(&directory.join(&path), &receipt, 0o644)?;
        written.insert(format!("esp/{}/{}", relative.display(), path), artifact);
    }
    let mut set_json = serde_json::to_vec(&ModuleSetManifest {
        schema_version: ota_core::modules::SCHEMA_VERSION,
        kmi: ReceiptKmi {
            branch: kmi.branch.clone(),
            generation: kmi.generation,
        },
        modules,
    })?;
    set_json.push(b'\n');
    let artifact = write_file(
        &directory.join(ota_core::modules::SET_FILE),
        &set_json,
        0o644,
    )?;
    written.insert(
        format!("esp/{}/{}", relative.display(), ota_core::modules::SET_FILE),
        artifact,
    );
    Ok(written)
}

pub fn patch(args: &BootPatchArgs) -> Result<()> {
    let payload_source = absolute(&args.payload)?;
    let source_root = platform::open_root(&payload_source)?;
    let output = absolute(&args.out)?;
    ensure!(
        !output.starts_with(&payload_source),
        "output must be outside payload source"
    );
    ensure!(
        fs::symlink_metadata(&output)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "output already exists or is inaccessible"
    );
    let parent = output.parent().context("output has no parent")?;
    let _parent = platform::open_root(parent)?;
    let stage = tempfile::Builder::new()
        .prefix(".esud-boot-patch-")
        .tempdir_in(parent)?;
    let staged = stage.path().join("result");
    make_dir(&staged)?;
    let payload_root = staged.join("esp");
    make_dir(&payload_root)?;
    let payload = payload_root.join("esu");
    make_dir(&payload)?;
    let mut source_files = BTreeMap::new();
    snapshot(
        &payload_source,
        &source_root,
        "",
        &payload,
        &mut source_files,
    )?;
    reserve_name(&payload, "bin")?;
    reserve_name(&payload, "receipts")?;
    reserve_name(&payload, "kmi")?;
    let (manifest, _, _) = configs(&payload, &args.rom)?;
    let binary = read_bounded(input_file(&args.esuinit)?, MAX_BINARY)?;
    let machine = executable(&binary, None, Linkage::StaticRequired)?;
    // Validate exactly the captured bytes, not a reopened mutable source.
    make_dir(&payload.join("bin"))?;
    reserve_name(&payload.join("bin"), "esuinit")?;
    let staged_binary = payload.join("bin/esuinit");
    if staged_binary.exists() {
        ensure!(
            source_files
                .get("bin/esuinit")
                .is_some_and(|file| file.sha256 == digest(&binary)),
            "payload bin/esuinit disagrees with --esu"
        );
        fs::remove_file(&staged_binary)?;
    }
    let binary_artifact = write_file(&staged_binary, &binary, 0o755)?;
    validate_payload(&payload, &manifest, machine, &source_files)?;
    let captured_modules = stage.path().join("modules");
    make_dir(&captured_modules)?;
    let mut module_files = BTreeMap::new();
    let module_root = platform::open_root(&absolute(&args.modules_dir)?)?;
    snapshot(
        &absolute(&args.modules_dir)?,
        &module_root,
        "",
        &captured_modules,
        &mut module_files,
    )?;
    let module_verification = verify_modules(
        &captured_modules,
        &absolute(&args.kmi_out)?,
        &manifest,
        machine,
    )?;
    let kmi = verified_kmi(&module_verification)?;
    make_dir(&payload.join("receipts"))?;
    let mut sources: BTreeMap<_, _> = source_files
        .iter()
        .map(|(path, item)| (format!("payload/{path}"), item.clone()))
        .collect();
    sources.insert("esuinit".to_owned(), binary_artifact.clone());
    for (path, artifact) in module_files {
        sources.insert(format!("modules/{path}"), artifact);
    }
    let mut artifacts: BTreeMap<_, _> = source_files
        .into_iter()
        .map(|(path, item)| (format!("esp/esu/{path}"), item))
        .collect();
    artifacts.insert("esp/esu/bin/esuinit".to_owned(), binary_artifact);
    let build_id_inputs: BTreeMap<String, String> = sources
        .iter()
        .map(|(path, artifact)| (path.clone(), artifact.sha256.clone()))
        .collect();
    let build_id = artifact_build_id(build_id_inputs.values().map(String::as_str));
    artifacts.insert(
        "esp/esu/build-id".to_owned(),
        write_file(
            &payload.join("build-id"),
            format!("{build_id}\n").as_bytes(),
            0o644,
        )?,
    );
    // The verified set is placed first and the archive is built through the same
    // selector the device uses, so the modules the device loads are the modules
    // this receipt hashed.
    artifacts.extend(place_module_set(
        &payload_root,
        &kmi,
        &manifest,
        &captured_modules,
    )?);
    let members = ota_core::select_module_set(&payload_root, &kmi)?.payload_members()?;
    let archive = ota_core::build_overlay(&binary, &members, &build_id)?;
    artifacts.insert(
        "esu.cpio".to_owned(),
        write_file(&staged.join("esu.cpio"), &archive, 0o644)?,
    );
    let archive_path = format!("rom/{}/esu.cpio", args.rom);
    make_dir(&staged.join("esp/rom"))?;
    make_dir(
        staged
            .join("esp")
            .join(&archive_path)
            .parent()
            .context("archive parent")?,
    )?;
    artifacts.insert(
        format!("esp/{archive_path}"),
        write_file(&staged.join("esp").join(&archive_path), &archive, 0o644)?,
    );
    let mut directories = Vec::new();
    sync_tree(&staged, "", &mut directories)?;
    let receipt = Receipt {
        schema_version: 1,
        tool: "esud boot-patch",
        tool_version: env!("CARGO_PKG_VERSION"),
        verifier_sha256: digest(VERIFIER),
        rom: args.rom.clone(),
        build_id,
        build_id_inputs,
        archive_path,
        kmi: ReceiptKmi {
            branch: kmi.branch,
            generation: kmi.generation,
        },
        sources,
        artifacts,
        directories,
        module_verification,
    };
    let mut receipt_bytes = serde_json::to_vec_pretty(&receipt)?;
    receipt_bytes.push(b'\n');
    write_file(&staged.join("receipt.json"), &receipt_bytes, 0o644)?;
    File::open(&staged)?.sync_all()?;
    publish(&staged, &output)?;
    println!("{}", output.join("receipt.json").display());
    Ok(())
}

#[cfg(test)]
#[path = "boot_patch_tests.rs"]
mod tests;
