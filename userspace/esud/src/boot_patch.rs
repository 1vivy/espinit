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
use esu_config as config;
use esu_platform as platform;
use goblin::elf::{Elf, header, program_header};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const VERIFIER: &[u8] = include_bytes!("../../../scripts/kmi_modules.py");
const MAX_BINARY: u64 = 64 * 1024 * 1024;
const MAX_BOOT: u64 = 512 * 1024 * 1024;

const LZ4_LEGACY_MAGIC: [u8; 4] = 0x184C_2102u32.to_le_bytes();
const LZ4_BLOCK_SIZE: usize = 8 * 1024 * 1024;

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
    verifier_sha256: String,
    rom: String,
    build_id: String,
    build_id_inputs: BTreeMap<String, String>,
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
    ] {
        check_binary(&payload.join(path), machine, linkage)?;
        ensure!(
            files.get(path).is_some_and(|file| file.mode == 0o755),
            "payload binary is not executable: {path}"
        );
    }
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

fn takeover_cpio(
    binary: Vec<u8>,
    real_init: Vec<u8>,
    modules: BTreeMap<String, Vec<u8>>,
    build_id: &str,
) -> Result<Vec<u8>> {
    let mut cpio = Cpio::new();
    cpio.add("init", CpioEntry::regular(0o755, Box::new(binary)))?;
    cpio.add(
        "init.esureal",
        CpioEntry::regular(0o755, Box::new(real_init)),
    )?;
    cpio.add(
        "esu-build-id",
        CpioEntry::regular(0o644, Box::new(format!("{build_id}\n").into_bytes())),
    )?;
    if !modules.is_empty() {
        cpio.add("lib", CpioEntry::dir(0o755))?;
    }
    for (path, bytes) in modules {
        cpio.add(&path, CpioEntry::regular(0o644, Box::new(bytes)))?;
    }
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
        !cpio.exists("init.esureal"),
        "stock ramdisk already contains reserved init.esureal"
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

fn boot_cmdline(original: &[u8]) -> Result<String> {
    let end = original
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(original.len());
    let original = std::str::from_utf8(&original[..end])?;
    let mut result = String::with_capacity(original.len());
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
    ensure!(
        result.len() < 1536,
        "boot command line exceeds v3/v4 capacity"
    );
    Ok(result)
}

fn patch_boot(source: &[u8], overlay: &[u8]) -> Result<Vec<u8>> {
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
    let cmdline = boot_cmdline(boot.get_header().get_cmdline())?;
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
    let mut modules = BTreeMap::new();
    for entry in manifest
        .modules
        .iter()
        .filter(|entry| entry.path.ends_with(".ko"))
    {
        modules.insert(
            entry.path.clone(),
            read_bounded(
                input_file(&captured_modules.join(format!("{}.ko", entry.name)))?,
                MAX_BINARY,
            )?,
        );
    }
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
    let overlay = takeover_cpio(binary, real_init, modules, &build_id)?;
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
    let patched = patch_boot(&source, &overlay)?;
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
        verifier_sha256: digest(VERIFIER),
        rom: args.rom.clone(),
        build_id,
        build_id_inputs,
        archive_path,
        boot_contract: "bdsvars BootedRom via efivarfs".to_owned(),
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
