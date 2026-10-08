// SPDX-License-Identifier: GPL-3.0-only
//! Base-image geometry and in-place block copies.
//!
//! Staging works on whole base images: the staging LV is created exactly the
//! size of the ESP image it replaces, prefilled with that image's bytes, and
//! promoted by copying the staged bytes back over the image. All three steps use
//! the same length, so the length is derived once from the resolved image and
//! every copy is exactly that long.

use anyhow::{Context, Result, ensure};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

/// Bytes in one device-mapper sector, the unit every table length is expressed
/// in. Equal to `lvm2_meta::SECTOR_SIZE`.
pub const SECTOR_SIZE: u64 = 512;

/// Bytes a base image must be a multiple of: the smallest image the GPT
/// projection and the AVB footer geometry are validated against.
pub const IMAGE_ALIGNMENT: u64 = 4096;

/// Chunk size of every copy and comparison below.
const CHUNK: usize = 1024 * 1024;

/// Sectors of the resolved base-image file at `image`.
///
/// The caller resolves the path: the ESP root plus `rom/<id>/<base>.img`, which
/// `esu_config::base_image_path(id, base)` derives for the ESP-relative part.
/// This function takes the resolved path so it never has to know the mount or
/// the ROM directory layout. A missing, empty or misaligned image is an error:
/// every later length (LV creation, prefill, promote, switch table) is derived
/// from this one answer.
pub fn exact_sectors(image: &Path) -> Result<u64> {
    let bytes = std::fs::metadata(image)
        .with_context(|| format!("base image {}", image.display()))?
        .len();
    ensure!(bytes > 0, "base image {} is empty", image.display());
    ensure!(
        bytes.is_multiple_of(IMAGE_ALIGNMENT),
        "base image {} is {bytes} bytes, not a multiple of {IMAGE_ALIGNMENT}",
        image.display()
    );
    Ok(bytes / SECTOR_SIZE)
}

/// Copy exactly `len` bytes from `src` to `dst`, both at offset 0.
///
/// The destination is written in place and never truncated, so a block device
/// or an already-sized ESP image keeps its size; a shorter regular file is
/// extended. One `sync_all` at the end covers the whole copy: a partially
/// synced image is never a state the caller can observe.
pub fn copy_range(src: &File, dst: &File, len: u64) -> Result<()> {
    let mut buffer = vec![0; CHUNK];
    let mut offset = 0;
    while offset < len {
        let take = usize::try_from((len - offset).min(CHUNK as u64)).expect("chunk fits usize");
        src.read_exact_at(&mut buffer[..take], offset)
            .with_context(|| format!("read source at {offset}"))?;
        dst.write_all_at(&buffer[..take], offset)
            .with_context(|| format!("write destination at {offset}"))?;
        offset += take as u64;
    }
    dst.sync_all().context("sync destination")?;
    Ok(())
}

/// Whether `a` and `b` hold identical bytes over their first `len` bytes.
///
/// `Ok(false)` is a mismatch, not an error: the promote path retries a mismatch
/// once and treats a read failure as fatal.
pub fn verify_equal(a: &File, b: &File, len: u64) -> Result<bool> {
    let mut left = vec![0; CHUNK];
    let mut right = vec![0; CHUNK];
    let mut offset = 0;
    while offset < len {
        let take = usize::try_from((len - offset).min(CHUNK as u64)).expect("chunk fits usize");
        a.read_exact_at(&mut left[..take], offset)
            .with_context(|| format!("read first at {offset}"))?;
        b.read_exact_at(&mut right[..take], offset)
            .with_context(|| format!("read second at {offset}"))?;
        if left[..take] != right[..take] {
            return Ok(false);
        }
        offset += take as u64;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;

    fn file(bytes: &[u8]) -> (tempfile::TempDir, File) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.write_all(bytes).unwrap();
        drop(file);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        (dir, file)
    }

    #[test]
    fn exact_sectors_requires_a_nonempty_aligned_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boot.img");

        assert!(exact_sectors(&path).is_err());
        std::fs::write(&path, []).unwrap();
        assert!(exact_sectors(&path).is_err());
        std::fs::write(&path, [0u8; 512]).unwrap();
        assert!(exact_sectors(&path).is_err());
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        assert_eq!(exact_sectors(&path).unwrap(), 8);
        std::fs::write(&path, vec![0u8; 4096 * 3]).unwrap();
        assert_eq!(exact_sectors(&path).unwrap(), 24);
    }

    #[test]
    fn copy_range_writes_exactly_the_requested_length() {
        let (_dir, source) = file(&[7u8; 4096]);
        let (_dest_dir, destination) = file(&[0u8; 4096]);

        copy_range(&source, &destination, 1024).unwrap();

        let mut bytes = vec![0u8; 4096];
        destination.read_exact_at(&mut bytes, 0).unwrap();
        assert!(bytes[..1024].iter().all(|byte| *byte == 7));
        assert!(bytes[1024..].iter().all(|byte| *byte == 0));
        assert!(verify_equal(&source, &destination, 1024).unwrap());
        assert!(!verify_equal(&source, &destination, 4096).unwrap());
    }

    #[test]
    fn copy_range_spans_chunks_and_a_short_source_is_an_error() {
        // One byte past a chunk boundary exercises the two-iteration path.
        let source_bytes: Vec<u8> = (0..CHUNK + 1).map(|index| index as u8).collect();
        let (_dir, source) = file(&source_bytes);
        let (_dest_dir, destination) = file(&[]);
        let len = (CHUNK + 1) as u64;

        copy_range(&source, &destination, len).unwrap();
        assert!(verify_equal(&source, &destination, len).unwrap());
        assert_eq!(destination.metadata().unwrap().len(), len);

        assert!(copy_range(&source, &destination, len + 1).is_err());
        assert!(verify_equal(&source, &destination, len + 1).is_err());
    }
}
