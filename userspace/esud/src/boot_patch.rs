//! Explicit-input, host-only packaging. No device discovery or Android runtime calls.
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use android_bootimg::cpio::{Cpio, CpioEntry};
use android_bootimg::parser::BootImage;
use android_bootimg::patcher::BootImagePatchOption;
use anyhow::{Context, Result, ensure};
use esu_platform::{self as platform, BootMode};
use esuinit::config;
use goblin::elf::{Elf, header, program_header, section_header, sym};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const VERIFIER: &[u8] = include_bytes!("../../../scripts/phone_modules.py");
const MAX_BINARY: u64 = 64 * 1024 * 1024;
const MAX_BOOT: u64 = 512 * 1024 * 1024;

const LZ4_LEGACY_MAGIC: [u8; 4] = 0x184C_2102u32.to_le_bytes();
const LZ4_BLOCK_SIZE: usize = 8 * 1024 * 1024;

#[derive(clap::Args, Debug)]
pub struct BootPatchArgs {
    /// Static ELF PID-1 binary, built for the payload generation (never executed)
    #[arg(long)]
    pub esuinit: PathBuf,
    /// Complete payload root containing manifest.toml, roms/, bin/, and modules/
    #[arg(long)]
    pub payload: PathBuf,
    /// Explicit ROM ID selected from the manifest's ROM directory
    #[arg(long)]
    pub rom: String,
    /// New output directory; must not exist, and its parent must already exist
    #[arg(long)]
    pub out: PathBuf,
    /// Stock boot/init_boot v3/v4 image whose effective /init is preserved
    #[arg(long)]
    pub boot: PathBuf,
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
    tool_generation: &'static str,
    verifier_sha256: String,
    rom: String,
    generation: String,
    archive_path: String,
    boot_contract: String,
    boot_image: &'static str,
    sources: BTreeMap<String, Artifact>,
    artifacts: BTreeMap<String, Artifact>,
    directories: Vec<String>,
    module_verification: serde_json::Value,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CompatibilityReceipt {
    schema_version: u32,
    kernel_src: PathBuf,
    kernel_out: PathBuf,
    kernel_config: PathBuf,
    stable: String,
    inputs: BTreeMap<String, String>,
    module_sha256: String,
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

fn check_binary(path: &Path, generation: &str, machine: u16, linkage: Linkage) -> Result<()> {
    platform::check_artifact(&mut input_file(path)?, generation)?;
    executable(
        &read_bounded(input_file(path)?, MAX_BINARY)?,
        Some(machine),
        linkage,
    )?;
    Ok(())
}

fn module_generation(data: &[u8], name: &str, generation: &str, machine: u16) -> Result<()> {
    let elf = Elf::parse(data)?;
    ensure!(
        elf.header.e_type == header::ET_REL && elf.header.e_machine == machine,
        "module architecture/type mismatch"
    );
    let symbol_name = if name == "kernelesp" {
        "ksu_build_generation"
    } else {
        "generation"
    };
    let mut symbols = elf
        .syms
        .iter()
        .filter(|symbol| elf.strtab.get_at(symbol.st_name) == Some(symbol_name));
    let symbol = symbols
        .next()
        .context("module lacks retained generation symbol")?;
    ensure!(
        symbols.next().is_none() && symbol.st_type() == sym::STT_OBJECT,
        "ambiguous module generation symbol"
    );
    let section = elf
        .section_headers
        .get(symbol.st_shndx)
        .context("invalid generation section")?;
    ensure!(
        section.sh_type == section_header::SHT_PROGBITS
            && section.sh_flags & u64::from(section_header::SHF_ALLOC) != 0,
        "generation must be runtime data"
    );
    let end = symbol
        .st_value
        .checked_add(symbol.st_size)
        .context("generation bounds overflow")?;
    ensure!(
        symbol.st_size > 1 && symbol.st_size <= 64 && end <= section.sh_size,
        "invalid generation symbol bounds"
    );
    let start = usize::try_from(
        section
            .sh_offset
            .checked_add(symbol.st_value)
            .context("generation offset overflow")?,
    )?;
    let end = start
        .checked_add(usize::try_from(symbol.st_size)?)
        .context("generation size overflow")?;
    let value = data.get(start..end).context("generation outside ELF")?;
    ensure!(
        value.last() == Some(&0) && &value[..value.len() - 1] == generation.as_bytes(),
        "module generation mismatch: {name}"
    );
    Ok(())
}

fn configs(payload: &Path, id: &str) -> Result<(config::Manifest, config::RomConfig, String)> {
    let manifest = config::parse_manifest(&fs::read_to_string(payload.join("manifest.toml"))?)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let rom_path = config::rom_path(&manifest, id).map_err(|error| anyhow::anyhow!("{error}"))?;
    let rom = config::parse_selected_rom(
        &fs::read_to_string(payload.join(&rom_path))?,
        &manifest.generation,
        id,
    )
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    config::validate_managed(&manifest, &rom).map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok((manifest, rom, rom_path))
}

fn validate_payload(
    payload: &Path,
    manifest: &config::Manifest,
    rom: &config::RomConfig,
    rom_path: &str,
    machine: u16,
    files: &BTreeMap<String, Artifact>,
) -> Result<()> {
    let root = platform::open_root(payload)?;
    let platform_config = manifest
        .platform
        .as_ref()
        .context("complete payload requires [platform]")?;
    let package_ids: BTreeSet<_> = platform_config
        .packages
        .iter()
        .chain(&platform_config.recovery_packages)
        .map(String::as_str)
        .collect();
    for path in files.keys().filter(|path| path.ends_with("/module.toml")) {
        let id = path
            .strip_prefix("modules/")
            .and_then(|path| path.strip_suffix("/module.toml"))
            .context("package manifest outside modules")?;
        ensure!(
            package_ids.contains(id),
            "unlisted package manifest: {path}"
        );
    }
    // Use the runtime planners, but never execute their mount/publication phase.
    let mode = if rom.managed {
        BootMode::Managed
    } else {
        BootMode::Unmanaged
    };
    platform::plan(
        payload,
        platform_config,
        &manifest.generation,
        rom_path,
        mode,
    )?;
    platform::plan(
        payload,
        platform_config,
        &manifest.generation,
        rom_path,
        BootMode::Recovery,
    )?;
    check_binary(
        &payload.join("bin/esud"),
        &manifest.generation,
        machine,
        Linkage::DynamicAllowed,
    )?;
    let busybox = read_bounded(platform::open_file(&root, "bin/busybox")?, MAX_BINARY)?;
    executable(&busybox, Some(machine), Linkage::StaticRequired)?;
    for path in ["bin/esud", "bin/busybox"] {
        ensure!(
            files.get(path).is_some_and(|file| file.mode == 0o755),
            "payload binary is not executable: {path}"
        );
    }
    for id in platform_config
        .packages
        .iter()
        .chain(&platform_config.recovery_packages)
    {
        let package = platform::parse_package(
            &fs::read_to_string(payload.join(format!("modules/{id}/module.toml")))?,
            id,
            &manifest.generation,
        )?;
        for entry in package.files {
            let path = format!("modules/{id}/{}", entry.source);
            let file = platform::open_file(&root, &path)?;
            ensure!(file.metadata()?.len() > 0, "empty package source: {path}");
            if entry.kind == platform::Kind::Binary {
                check_binary(
                    &payload.join(path),
                    &manifest.generation,
                    machine,
                    Linkage::DynamicAllowed,
                )?;
            }
        }
    }
    // Additional tools (notably thin-activate and fw-views) also carry
    // generation notes; both are executed by PID 1 from the ESP, so both must be
    // interpreter-free.
    for path in files.keys().filter(|path| {
        path.starts_with("bin/")
            && !matches!(path.as_str(), "bin/busybox" | "bin/esuinit" | "bin/esud")
    }) {
        let bytes = read_bounded(platform::open_file(&root, path)?, MAX_BINARY)?;
        if bytes.starts_with(b"\x7fELF") {
            check_binary(
                &payload.join(path),
                &manifest.generation,
                machine,
                if matches!(path.as_str(), "bin/thin-activate" | "bin/fw-views") {
                    Linkage::StaticRequired
                } else {
                    Linkage::DynamicAllowed
                },
            )?;
        }
    }
    // A complete copied ROM directory must not hide a stale generation.
    let prefix = format!("{}/", manifest.rom);
    for path in files
        .keys()
        .filter(|path| path.starts_with(&prefix) && path.ends_with(".toml"))
    {
        let id = path
            .strip_prefix(&prefix)
            .and_then(|name| name.strip_suffix(".toml"))
            .context("invalid ROM path")?;
        let other = config::parse_selected_rom(
            &fs::read_to_string(payload.join(path))?,
            &manifest.generation,
            id,
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        config::validate_managed(manifest, &other).map_err(|error| anyhow::anyhow!("{error}"))?;
        for partition in &other.partitions {
            if let Some(path) = partition.backend.strip_prefix("esp-file:") {
                // esp-file is relative to the ESP mount, not /esu.
                let path = path
                    .strip_prefix("esu/")
                    .context("ESP-file backend must be inside supplied esu payload")?;
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
    payload: &Path,
    manifest: &config::Manifest,
    machine: u16,
    files: &BTreeMap<String, Artifact>,
) -> Result<serde_json::Value> {
    let declared: BTreeSet<_> = manifest
        .modules
        .iter()
        .map(|module| module.path.as_str())
        .collect();
    ensure!(
        declared.len() == manifest.modules.len(),
        "duplicate module path"
    );
    for path in files.keys() {
        if path.ends_with(".ko") {
            ensure!(declared.contains(path.as_str()), "unlisted module: {path}");
        }
        if let Some(module) = path.strip_suffix(".compat.json") {
            ensure!(
                module.ends_with(".ko") && declared.contains(module),
                "orphan compatibility receipt: {path}"
            );
        }
    }
    let verifier = tempfile::tempdir()?;
    let script = verifier.path().join("phone_modules.py");
    write_file(&script, VERIFIER, 0o644)?;
    let mut command = Command::new("python3");
    command.arg("-I").arg(script).arg("verify");
    let mut first: Option<CompatibilityReceipt> = None;
    for module in &manifest.modules {
        if !module.path.ends_with(".ko") {
            // A userspace helper module has no kernel ABI and no compatibility
            // receipt: it is executed by its own module script from the ESP, so
            // only its presence, its executability and its generation note are
            // checked. Its parameters must stay empty for the same reason.
            ensure!(
                matches!(module.name.as_str(), "fw-views"),
                "phone verifier does not admit helper module {}",
                module.name
            );
            ensure!(
                module.params.is_empty(),
                "helper module {} takes no parameters",
                module.name
            );
            ensure!(
                files
                    .get(&module.path)
                    .is_some_and(|file| file.mode == 0o755),
                "helper module is not executable: {}",
                module.path
            );
            check_binary(
                &payload.join(&module.path),
                &manifest.generation,
                machine,
                Linkage::StaticRequired,
            )?;
            continue;
        }
        ensure!(
            matches!(module.name.as_str(), "kernelesp" | "thin" | "gpt"),
            "phone verifier does not admit module {}",
            module.name
        );
        ensure!(module.path.ends_with(".ko"), "module must use .ko suffix");
        let bytes = read_bounded(input_file(&payload.join(&module.path))?, MAX_BINARY)?;
        module_generation(&bytes, &module.name, &manifest.generation, machine)?;
        let receipt: CompatibilityReceipt = serde_json::from_slice(&read_bounded(
            input_file(&payload.join(format!("{}.compat.json", module.path)))?,
            MAX_BINARY,
        )?)?;
        ensure!(
            receipt.schema_version == 1
                && receipt.stable == "1"
                && receipt.module_sha256 == digest(&bytes),
            "module/compatibility receipt mismatch: {}",
            module.name
        );
        ensure!(
            receipt.kernel_src.is_absolute()
                && receipt.kernel_out.is_absolute()
                && receipt.kernel_config.is_absolute(),
            "compatibility receipt requires absolute kernel inputs"
        );
        if let Some(first) = &first {
            ensure!(
                first.kernel_src == receipt.kernel_src
                    && first.kernel_out == receipt.kernel_out
                    && first.kernel_config == receipt.kernel_config
                    && first.inputs == receipt.inputs,
                "module receipts disagree on exact kernel inputs"
            );
        } else {
            command
                .arg("--kernel-src")
                .arg(&receipt.kernel_src)
                .arg("--kernel-out")
                .arg(&receipt.kernel_out)
                .arg("--kernel-config")
                .arg(&receipt.kernel_config);
            first = Some(receipt);
        }
        command
            .arg(format!("--{}", module.name))
            .arg(payload.join(&module.path));
    }
    let output = command
        .output()
        .context("run embedded phone_modules.py (Python 3.11+ required)")?;
    ensure!(
        output.status.success(),
        "phone module admission failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("invalid phone verifier result")?;
    ensure!(
        report["status"] == "accepted",
        "phone verifier did not accept payload"
    );
    Ok(report)
}

fn takeover_cpio(binary: Vec<u8>, real_init: Vec<u8>) -> Result<Vec<u8>> {
    let mut cpio = Cpio::new();
    cpio.add("init", CpioEntry::regular(0o755, Box::new(binary)))?;
    cpio.add("init.real", CpioEntry::regular(0o755, Box::new(real_init)))?;
    let mut bytes = Vec::new();
    cpio.dump(&mut bytes)?;
    bytes.resize(bytes.len().next_multiple_of(512), 0);
    Ok(bytes)
}

fn legacy_lz4(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoded = Vec::with_capacity(data.len());
    encoded.extend_from_slice(&LZ4_LEGACY_MAGIC);
    for chunk in data.chunks(LZ4_BLOCK_SIZE) {
        let block = lz4::block::compress(
            chunk,
            Some(lz4::block::CompressionMode::HIGHCOMPRESSION(12)),
            false,
        )?;
        encoded.extend_from_slice(&u32::try_from(block.len())?.to_le_bytes());
        encoded.extend_from_slice(&block);
    }
    Ok(encoded)
}

fn stock_init(source: &[u8], machine: u16) -> Result<Vec<u8>> {
    let boot = BootImage::parse(source).context("parse stock boot image")?;
    let mut ramdisk = Vec::new();
    boot.get_blocks()
        .get_ramdisk()
        .context("stock boot image has no ramdisk")?
        .dump(&mut ramdisk, false)?;
    validate_cpio(&ramdisk)?;
    let cpio = Cpio::load_from_data(&ramdisk)?;
    ensure!(
        !cpio.exists("init.real"),
        "stock ramdisk already contains reserved init.real"
    );
    let init = cpio
        .entry_by_name("init")
        .and_then(CpioEntry::data)
        .context("stock ramdisk /init is absent or empty")?
        .to_vec();
    let _ =
        executable(&init, Some(machine), Linkage::DynamicAllowed).context("stock ramdisk /init")?;
    Ok(init)
}

/// Validate framing before preserving stock archives verbatim. Re-serializing
/// with Cpio would discard hardlink identity and other original newc metadata.
fn validate_cpio(data: &[u8]) -> Result<()> {
    let mut offset = 0usize;
    while offset < data.len() {
        while data.get(offset) == Some(&0) {
            offset += 1;
        }
        if offset == data.len() {
            break;
        }
        loop {
            let raw = data
                .get(offset..offset.checked_add(110).context("cpio offset overflow")?)
                .context("truncated cpio header")?;
            ensure!(
                &raw[..6] == b"070701" || &raw[..6] == b"070702",
                "unsupported cpio framing"
            );
            let mut fields = [0u32; 13];
            for (index, field) in fields.iter_mut().enumerate() {
                *field = u32::from_str_radix(
                    std::str::from_utf8(&raw[6 + index * 8..14 + index * 8])?,
                    16,
                )?;
            }
            let name_start = offset + 110;
            let name_end = name_start
                .checked_add(fields[11] as usize)
                .context("cpio name overflow")?;
            let name = data
                .get(name_start..name_end)
                .context("truncated cpio name")?;
            ensure!(
                name.last() == Some(&0) && !name[..name.len() - 1].contains(&0),
                "invalid cpio name"
            );
            let name = std::str::from_utf8(&name[..name.len() - 1])?;
            ensure!(
                !name.is_empty()
                    && !name.starts_with('/')
                    && !name.split('/').any(|part| part == ".."),
                "cpio path traversal"
            );
            let start = name_end.checked_add(3).context("cpio alignment overflow")? & !3;
            let end = start
                .checked_add(fields[6] as usize)
                .context("cpio data overflow")?;
            let content = data.get(start..end).context("truncated cpio data")?;
            offset = end.checked_add(3).context("cpio alignment overflow")? & !3;
            ensure!(offset <= data.len(), "truncated cpio padding");
            if &raw[..6] == b"070702" {
                ensure!(
                    content
                        .iter()
                        .fold(0u32, |sum, byte| sum.wrapping_add(u32::from(*byte)))
                        == fields[12],
                    "cpio checksum mismatch"
                );
            }
            if name == "TRAILER!!!" {
                ensure!(fields[6] == 0, "nonempty cpio trailer");
                break;
            }
        }
    }
    Ok(())
}

fn boot_cmdline(original: &[u8], rom: &str) -> Result<String> {
    let end = original
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(original.len());
    let original = std::str::from_utf8(&original[..end])?;
    let mut result = String::with_capacity(original.len() + rom.len() + 32);
    let mut quoted = false;
    let mut start = None;
    for (offset, ch) in original
        .char_indices()
        .chain(std::iter::once((original.len(), ' ')))
    {
        if ch == '"' {
            quoted = !quoted;
        }
        if ch.is_ascii_whitespace() && !quoted {
            if let Some(start) = start.take() {
                let token = &original[start..offset];
                let key = token.strip_prefix('"').unwrap_or(token);
                if !key.starts_with("rdinit=") && !key.starts_with("androidboot.esu.rom=") {
                    if !result.is_empty() {
                        result.push(' ');
                    }
                    result.push_str(token);
                }
            }
        } else if start.is_none() {
            start = Some(offset);
        }
    }
    ensure!(!quoted, "unterminated quote in boot command line");
    if !result.is_empty() {
        result.push(' ');
    }
    result.push_str("androidboot.esu.rom=");
    result.push_str(rom);
    ensure!(
        result.len() < 1536,
        "boot command line exceeds v3/v4 capacity"
    );
    Ok(result)
}

fn patch_boot(source: &[u8], overlay: &[u8], rom: &str) -> Result<Vec<u8>> {
    // Guard the upstream parser's fixed header slicing and avoid carrying invalid
    // signatures into a test image. Only Android boot/init_boot v3/v4 is supported.
    ensure!(
        source.len() >= 4096 && source.starts_with(b"ANDROID!"),
        "expected boot/init_boot image"
    );
    let version = u32::from_le_bytes(source[40..44].try_into()?);
    ensure!(
        matches!(version, 3 | 4),
        "only Android boot/init_boot v3/v4 is supported"
    );
    let header_size = u32::from_le_bytes(source[20..24].try_into()?);
    ensure!(
        header_size == if version == 3 { 1580 } else { 1584 },
        "invalid boot header size"
    );
    let kernel_size = u32::from_le_bytes(source[8..12].try_into()?) as usize;
    let ramdisk_size = u32::from_le_bytes(source[12..16].try_into()?) as usize;
    let unsigned_size = 4096usize
        .checked_add(kernel_size.next_multiple_of(4096))
        .and_then(|size| size.checked_add(ramdisk_size.next_multiple_of(4096)))
        .context("boot image size overflow")?;
    ensure!(unsigned_size <= source.len(), "truncated boot image");
    // Deliberately omit GKI signature/AVB data rather than claiming it still signs
    // changed bytes. Firmware-specific signing is not part of this host builder.
    let mut unsigned = Cow::Borrowed(&source[..unsigned_size]);
    if version == 4 && source[1580..1584] != [0; 4] {
        unsigned.to_mut()[1580..1584].fill(0);
    }
    let boot = BootImage::parse(&unsigned)?;
    let cmdline = boot_cmdline(boot.get_header().get_cmdline(), rom)?;
    let mut ramdisk = Vec::new();
    if let Some(original) = boot.get_blocks().get_ramdisk() {
        original.dump(&mut ramdisk, false)?;
    }
    validate_cpio(&ramdisk)?;
    ramdisk.resize(ramdisk.len().next_multiple_of(4), 0);
    ramdisk.extend_from_slice(overlay);
    let mut patcher = BootImagePatchOption::new(&boot);
    patcher.override_cmdline(cmdline.as_bytes());
    // Preserve the original compression using the historical patcher, including
    // its default legacy-LZ4 encoding when adding a previously absent ramdisk.
    patcher.replace_ramdisk(Box::new(Cursor::new(ramdisk)), false);
    let mut output = Cursor::new(Vec::new());
    patcher.patch(&mut output)?;
    Ok(output.into_inner())
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
    make_dir(&staged.join("esp"))?;
    let payload = staged.join("esp/esu");
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
    let (manifest, rom, rom_path) = configs(&payload, &args.rom)?;
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
    platform::check_artifact(&mut input_file(&staged_binary)?, &manifest.generation)?;
    validate_payload(&payload, &manifest, &rom, &rom_path, machine, &source_files)?;
    let module_verification = verify_modules(&payload, &manifest, machine, &source_files)?;
    make_dir(&payload.join("receipts"))?;
    let mut sources: BTreeMap<_, _> = source_files
        .iter()
        .map(|(path, item)| (format!("payload/{path}"), item.clone()))
        .collect();
    sources.insert("esuinit".to_owned(), binary_artifact.clone());
    let mut artifacts: BTreeMap<_, _> = source_files
        .into_iter()
        .map(|(path, item)| (format!("esp/esu/{path}"), item))
        .collect();
    artifacts.insert("esp/esu/bin/esuinit".to_owned(), binary_artifact);
    let source = read_bounded(input_file(&args.boot)?, MAX_BOOT)?;
    sources.insert(
        "boot".to_owned(),
        Artifact {
            sha256: digest(&source),
            size: source.len() as u64,
            mode: 0o644,
        },
    );
    let real_init = stock_init(&source, machine)?;
    let overlay = takeover_cpio(binary, real_init)?;
    let archive = legacy_lz4(&overlay)?;
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
    let patched = patch_boot(&source, &overlay, &args.rom)?;
    artifacts.insert(
        "patched.img".to_owned(),
        write_file(&staged.join("patched.img"), &patched, 0o644)?,
    );
    let mut directories = Vec::new();
    sync_tree(&staged, "", &mut directories)?;
    let receipt = Receipt {
        schema_version: 1,
        tool: "esud boot-patch",
        tool_version: env!("CARGO_PKG_VERSION"),
        tool_generation: platform::generation::generation(),
        verifier_sha256: digest(VERIFIER),
        rom: args.rom.clone(),
        generation: manifest.generation,
        archive_path,
        boot_contract: format!("androidboot.esu.rom={}", args.rom),
        boot_image: "unsigned-conventional-test-only",
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
