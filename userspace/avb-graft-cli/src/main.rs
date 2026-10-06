// SPDX-License-Identifier: GPL-3.0-only
//! Platform I/O for the canonical AVB metadata layout.
use anyhow::{Context, Result, bail, ensure};
use avb_graft::{FOOTER_SIZE, parse_footer, plan_graft, validate_vbmeta};
use esuinit::{
    block, config,
    esp::{ESP_MOUNT_POINT, payload_root},
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::{
        fd::AsRawFd,
        unix::fs::{FileExt, FileTypeExt, OpenOptionsExt},
    },
    path::Path,
};

const MAX_METADATA: u64 = 16 * 1024 * 1024;

fn metadata(file: File) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(MAX_METADATA + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_METADATA,
        "metadata exceeds 16 MiB limit"
    );
    validate_vbmeta(&bytes).map_err(|e| anyhow::anyhow!("invalid replacement metadata: {e}"))?;
    Ok(bytes)
}

fn image_size(file: &File) -> Result<u64> {
    let stat = file.metadata()?;
    if stat.is_file() {
        return Ok(stat.len());
    }
    ensure!(
        stat.file_type().is_block_device(),
        "image must be a regular file or block view"
    );
    let mut size = 0_u64;
    // SAFETY: BLKGETSIZE64 writes one u64 to a live, aligned output pointer.
    let result = unsafe { libc::ioctl(file.as_raw_fd(), 0x80081272u32 as _, &mut size) };
    ensure!(
        result == 0,
        "BLKGETSIZE64: {}",
        std::io::Error::last_os_error()
    );
    Ok(size)
}

fn footer(file: &File, size: u64) -> Result<[u8; FOOTER_SIZE]> {
    ensure!(
        size >= FOOTER_SIZE as u64,
        "image is shorter than AVB footer"
    );
    let mut bytes = [0; FOOTER_SIZE];
    file.read_exact_at(&mut bytes, size - FOOTER_SIZE as u64)?;
    Ok(bytes)
}

fn current_metadata(file: &File, size: u64, footer: &[u8]) -> Result<Option<Vec<u8>>> {
    let layout =
        parse_footer(footer, size).map_err(|e| anyhow::anyhow!("invalid image footer: {e}"))?;
    ensure!(
        layout.vbmeta_size <= MAX_METADATA,
        "current metadata exceeds 16 MiB limit"
    );
    let mut bytes = vec![0; layout.vbmeta_size as usize];
    file.read_exact_at(&mut bytes, layout.vbmeta_offset)?;
    Ok(validate_vbmeta(&bytes).ok().map(|_| bytes))
}

fn write_changed(file: &File, offset: u64, bytes: &[u8]) -> Result<bool> {
    let mut old = vec![0; bytes.len()];
    file.read_exact_at(&mut old, offset)?;
    if old == bytes {
        return Ok(false);
    }
    file.write_all_at(bytes, offset)?;
    Ok(true)
}

fn apply(file: &File, replacement: &[u8]) -> Result<bool> {
    let size = image_size(file)?;
    let old_footer = footer(file, size)?;
    let graft = plan_graft(size, &old_footer, replacement)
        .map_err(|e| anyhow::anyhow!("cannot graft image of {size} bytes: {e}"))?;
    let metadata_changed = write_changed(file, graft.metadata_offset(), graft.metadata())?;
    // Publish the footer last; sync metadata before making a larger extent visible.
    if metadata_changed {
        file.sync_data()?;
    }
    let footer_changed = write_changed(file, graft.footer_offset(), graft.footer())?;
    if metadata_changed || footer_changed {
        file.sync_all()?;
        if file.metadata()?.file_type().is_block_device() {
            // SAFETY: BLKFLSBUF takes no pointer and acts on the owned COW fd.
            let result = unsafe { libc::ioctl(file.as_raw_fd(), 0x1261 as libc::Ioctl) };
            ensure!(
                result == 0,
                "BLKFLSBUF after graft: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    let mut readback = vec![0; graft.metadata().len()];
    file.read_exact_at(&mut readback, graft.metadata_offset())?;
    ensure!(
        readback == graft.metadata(),
        "graft metadata readback mismatch"
    );
    let mut readback_footer = [0; FOOTER_SIZE];
    file.read_exact_at(&mut readback_footer, graft.footer_offset())?;
    ensure!(
        &readback_footer == graft.footer(),
        "graft footer readback mismatch"
    );
    Ok(metadata_changed || footer_changed)
}

/// Query a thin target's live status, not its table. Any private mapped block
/// means the current per-ROM state (including OTA writes) wins over stale input.
fn thin_has_writes(name: &str) -> Result<bool> {
    ensure!(name.len() < 128, "mapper name too long");
    let control = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/mapper/control")?;
    let mut storage = [0_u64; 512];
    // SAFETY: initialized u64 storage is aligned for DM ioctl and byte-addressable.
    let bytes = unsafe { std::slice::from_raw_parts_mut(storage.as_mut_ptr().cast::<u8>(), 4096) };
    bytes[0..4].copy_from_slice(&4_u32.to_ne_bytes());
    bytes[12..16].copy_from_slice(&4096_u32.to_ne_bytes());
    bytes[16..20].copy_from_slice(&312_u32.to_ne_bytes());
    bytes[48..48 + name.len()].copy_from_slice(name.as_bytes());
    let request = ((3_u64 << 30) | (312_u64 << 16) | (0xfd << 8) | 12) as libc::Ioctl;
    // SAFETY: live fd and 4096-byte writable aligned buffer; DM header advertises bounds.
    let result = unsafe { libc::ioctl(control.as_raw_fd(), request, bytes.as_mut_ptr()) };
    ensure!(
        result == 0,
        "DM_TABLE_STATUS {name}: {}",
        std::io::Error::last_os_error()
    );
    let field = |offset| u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap());
    let size = field(12) as usize;
    let start = field(16) as usize;
    ensure!(
        field(20) == 1
            && field(28) & (1 << 8) == 0
            && size <= bytes.len()
            && start >= 312
            && start.checked_add(40).is_some_and(|end| end < size),
        "invalid thin status for {name}"
    );
    ensure!(
        &bytes[start + 24..start + 29] == b"thin\0",
        "{name} is not a thin COW view; physical/linear targets forbidden"
    );
    let params = &bytes[start + 40..size];
    let end = params
        .iter()
        .position(|b| *b == 0)
        .context("unterminated thin status")?;
    let status = std::str::from_utf8(&params[..end])?;
    let mapped = status
        .split_whitespace()
        .next()
        .context("empty thin status")?
        .parse::<u64>()
        .with_context(|| format!("invalid thin status for {name}: {status}"))?;
    Ok(mapped != 0)
}

fn module() -> Result<()> {
    let root = payload_root(ESP_MOUNT_POINT);
    let manifest = config::parse_manifest(&fs::read_to_string(root.join("manifest.toml"))?)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let id = std::env::var("ESU_ROM").context("ESU_ROM missing")?;
    let number = std::env::var("ESU_ROM_NUMBER")
        .context("ESU_ROM_NUMBER missing")?
        .parse::<u32>()?;
    let path = config::rom_path(&manifest, &id).map_err(|e| anyhow::anyhow!("{e}"))?;
    let rom = config::parse_selected_rom(&fs::read_to_string(root.join(path))?, &id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    config::validate_rom(&rom, number).map_err(|e| anyhow::anyhow!("{e}"))?;
    let esp = esu_platform::open_root(Path::new(ESP_MOUNT_POINT))?;
    for partition in &rom.partitions {
        let Some(input) = &partition.metadata else {
            continue;
        };
        ensure!(
            !partition.read_only,
            "{}: metadata target is read-only",
            partition.name
        );
        if partition.backend.starts_with("esp-file:") {
            // Provisioning grafts ESP images once; OTA/promotion contents win.
            println!(
                "avb-graft: {} preserving provisioned ESP file",
                partition.name
            );
            continue;
        }
        let replacement = metadata(esu_platform::open_file(&esp, input)?)
            .with_context(|| format!("{}: metadata {input}", partition.name))?;
        let (target, preserve) = if let Some(name) = partition.backend.strip_prefix("/dev/mapper/")
        {
            ensure!(
                name.starts_with(&format!("rom{number}-")),
                "{}: mapper is not owned by selected ROM",
                partition.name
            );
            let resolved =
                block::resolve(&partition.backend, ESP_MOUNT_POINT, block::Access::Writable)?;
            let preserve = thin_has_writes(name)?;
            let target = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&resolved.path)?;
            (target, preserve)
        } else {
            bail!(
                "{}: metadata backend {} is physical-direct or unsupported; only writable ESP files or per-ROM thin COW views are allowed",
                partition.name,
                partition.backend
            );
        };
        if preserve {
            let size = image_size(&target)?;
            let current = footer(&target, size)
                .and_then(|tail| current_metadata(&target, size, &tail))
                .with_context(|| {
                    format!(
                        "{}: current metadata unusable; drop the view to reseed",
                        partition.backend
                    )
                })?
                .with_context(|| {
                    format!(
                        "{}: current metadata unusable; drop the view to reseed",
                        partition.backend
                    )
                })?;
            let replacement_size = validate_vbmeta(&replacement)
                .map_err(|e| anyhow::anyhow!("invalid replacement metadata: {e}"))?
                .size() as usize;
            if current != replacement[..replacement_size] {
                println!(
                    "avb-graft: {} preserving current view; drop to reseed",
                    partition.name
                );
            }
            continue;
        }
        let changed =
            apply(&target, &replacement).with_context(|| format!("{}: graft", partition.name))?;
        println!(
            "avb-graft: {} {}",
            partition.name,
            if changed { "changed" } else { "unchanged" }
        );
    }
    Ok(())
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [mode] if mode == "module" => module(),
        [mode, image, output] if mode == "extract" => {
            let image = File::open(image)?;
            let size = image_size(&image)?;
            let tail = footer(&image, size)?;
            let bytes = current_metadata(&image, size, &tail)?
                .context("image metadata is not valid AVB0")?;
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)?;
            std::io::Write::write_all(&mut out, &bytes)?;
            Ok(())
        }
        [mode, image, input, output] if mode == "apply" => {
            let replacement = metadata(File::open(input)?)?;
            let mut source = File::open(image)?;
            ensure!(
                source.metadata()?.is_file(),
                "host apply input must be a regular image file"
            );
            let size = image_size(&source)?;
            let tail = footer(&source, size)?;
            plan_graft(size, &tail, &replacement)
                .map_err(|e| anyhow::anyhow!("cannot graft image: {e}"))?;
            source.seek(SeekFrom::Start(0))?;
            // Never overwrite an existing destination or alias the input.
            let mut target = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(output)?;
            std::io::copy(&mut source, &mut target)?;
            let changed = apply(&target, &replacement)?;
            println!(
                "avb-graft: {}",
                if changed { "changed" } else { "unchanged" }
            );
            Ok(())
        }
        _ => bail!(
            "usage: avb-graft apply IMAGE METADATA NEW_OUTPUT | extract IMAGE NEW_METADATA | module"
        ),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("avb-graft: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use avb_graft::{Footer, replacement_footer};

    #[test]
    fn empty_metadata_region_is_seeded_without_changing_payload_or_size() {
        let path = std::env::temp_dir().join(format!("esu-graft-{}", std::process::id()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let mut image = vec![0; 8192];
        image[..4096].fill(0x5a);
        let tail = replacement_footer(Footer {
            original_image_size: 4096,
            vbmeta_offset: 4096,
            vbmeta_size: 256,
        });
        image[8192 - FOOTER_SIZE..].copy_from_slice(&tail);
        file.write_all_at(&image, 0).unwrap();
        let mut replacement = vec![0; 256];
        replacement[..4].copy_from_slice(b"AVB0");
        replacement[4..8].copy_from_slice(&1_u32.to_be_bytes());
        assert!(current_metadata(&file, 8192, &tail).unwrap().is_none());
        assert!(apply(&file, &replacement).unwrap());
        assert!(!apply(&file, &replacement).unwrap());
        let mut actual = vec![0; 8192];
        file.read_exact_at(&mut actual, 0).unwrap();
        assert_eq!(&actual[..4096], &image[..4096]);
        assert_eq!(image_size(&file).unwrap(), 8192);
        assert_eq!(
            current_metadata(&file, 8192, &tail).unwrap(),
            Some(replacement)
        );
        drop(file);
        fs::remove_file(path).unwrap();
    }
}
