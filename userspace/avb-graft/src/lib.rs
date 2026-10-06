#![no_std]
//! Allocation-free AVB metadata layout, not signature or payload verification.
//! Wire offsets come from AOSP external/avb/libavb/{avb_footer.h,
//! avb_vbmeta_image.h,avb_descriptor.h}; all integers are big-endian.
//! Only the fixed end-of-image footer locates metadata. No marker scanning.

use core::fmt;

pub const FOOTER_SIZE: usize = 64;
pub const HEADER_SIZE: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Truncated,
    Magic,
    Version,
    Reserved,
    ReleaseString,
    Overflow,
    BlockAlignment,
    MetadataBounds,
    DescriptorBounds,
    TrailingData,
    FooterGeometry,
    ReplacementDoesNotFit,
    ReadBounds,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Truncated => "truncated AVB structure",
            Self::Magic => "invalid AVB magic",
            Self::Version => "unsupported AVB version",
            Self::Reserved => "nonzero reserved AVB bytes",
            Self::ReleaseString => "vbmeta release string lacks terminating NUL",
            Self::Overflow => "AVB offset arithmetic overflow",
            Self::BlockAlignment => "AVB block size is not 64-byte aligned",
            Self::MetadataBounds => "vbmeta field lies outside its block",
            Self::DescriptorBounds => "invalid descriptor extent",
            Self::TrailingData => "nonzero bytes after standalone vbmeta",
            Self::FooterGeometry => "footer metadata overlaps payload or end footer",
            Self::ReplacementDoesNotFit => "replacement metadata does not fit before footer",
            Self::ReadBounds => "logical read lies outside image",
        })
    }
}

impl core::error::Error for Error {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Vbmeta {
    size: u64,
}

impl Vbmeta {
    pub fn size(self) -> u64 {
        self.size
    }
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, Error> {
    let value = bytes.get(offset..offset + 8).ok_or(Error::Truncated)?;
    Ok(u64::from_be_bytes(
        value.try_into().map_err(|_| Error::Truncated)?,
    ))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    let value = bytes.get(offset..offset + 4).ok_or(Error::Truncated)?;
    Ok(u32::from_be_bytes(
        value.try_into().map_err(|_| Error::Truncated)?,
    ))
}

fn bounded(offset: u64, size: u64, limit: u64) -> Result<(), Error> {
    if offset.checked_add(size).ok_or(Error::Overflow)? > limit {
        return Err(Error::MetadataBounds);
    }
    Ok(())
}

/// Validate structural bounds of standalone AVB0 metadata. Zero file padding
/// is accepted but excluded from `size()`. Cryptographic admission stays in AVB.
pub fn validate_vbmeta(bytes: &[u8]) -> Result<Vbmeta, Error> {
    if bytes.len() < HEADER_SIZE {
        return Err(Error::Truncated);
    }
    if &bytes[..4] != b"AVB0" {
        return Err(Error::Magic);
    }
    if u32_at(bytes, 4)? != 1 || u32_at(bytes, 8)? > 4 {
        return Err(Error::Version);
    }
    if bytes[176..256].iter().any(|byte| *byte != 0) {
        return Err(Error::Reserved);
    }
    if bytes[175] != 0 {
        return Err(Error::ReleaseString);
    }
    let auth = u64_at(bytes, 12)?;
    let aux = u64_at(bytes, 20)?;
    if auth & 63 != 0 || aux & 63 != 0 {
        return Err(Error::BlockAlignment);
    }
    let aux_start = (HEADER_SIZE as u64)
        .checked_add(auth)
        .ok_or(Error::Overflow)?;
    let size = aux_start.checked_add(aux).ok_or(Error::Overflow)?;
    let end = usize::try_from(size).map_err(|_| Error::Overflow)?;
    if end > bytes.len() {
        return Err(Error::Truncated);
    }
    for (offset, limit) in [(32, auth), (48, auth), (64, aux), (80, aux), (96, aux)] {
        bounded(u64_at(bytes, offset)?, u64_at(bytes, offset + 8)?, limit)?;
    }
    let descriptors_offset = u64_at(bytes, 96)?;
    let descriptors_size = u64_at(bytes, 104)?;
    let start = usize::try_from(aux_start + descriptors_offset).map_err(|_| Error::Overflow)?;
    let stop = usize::try_from(aux_start + descriptors_offset + descriptors_size)
        .map_err(|_| Error::Overflow)?;
    let mut cursor = start;
    while cursor < stop {
        if stop - cursor < 16 {
            return Err(Error::DescriptorBounds);
        }
        let following = u64_at(bytes, cursor + 8)?;
        if following & 7 != 0 {
            return Err(Error::DescriptorBounds);
        }
        let length = following.checked_add(16).ok_or(Error::Overflow)?;
        let length = usize::try_from(length).map_err(|_| Error::Overflow)?;
        cursor = cursor.checked_add(length).ok_or(Error::Overflow)?;
        if cursor > stop {
            return Err(Error::DescriptorBounds);
        }
    }
    if bytes[end..].iter().any(|byte| *byte != 0) {
        return Err(Error::TrailingData);
    }
    Ok(Vbmeta { size })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Footer {
    pub original_image_size: u64,
    pub vbmeta_offset: u64,
    pub vbmeta_size: u64,
}

/// Parse the exact 64 bytes at `image_size - 64`. Original metadata need not
/// be usable: an empty region with valid footer geometry can be grafted.
pub fn parse_footer(bytes: &[u8], image_size: u64) -> Result<Footer, Error> {
    if bytes.len() != FOOTER_SIZE {
        return Err(Error::Truncated);
    }
    if &bytes[..4] != b"AVBf" {
        return Err(Error::Magic);
    }
    if u32_at(bytes, 4)? != 1 || u32_at(bytes, 8)? != 0 {
        return Err(Error::Version);
    }
    if bytes[36..].iter().any(|byte| *byte != 0) {
        return Err(Error::Reserved);
    }
    let footer = Footer {
        original_image_size: u64_at(bytes, 12)?,
        vbmeta_offset: u64_at(bytes, 20)?,
        vbmeta_size: u64_at(bytes, 28)?,
    };
    let limit = image_size
        .checked_sub(FOOTER_SIZE as u64)
        .ok_or(Error::FooterGeometry)?;
    if footer.original_image_size > footer.vbmeta_offset
        || footer
            .vbmeta_offset
            .checked_add(footer.vbmeta_size)
            .ok_or(Error::Overflow)?
            > limit
    {
        return Err(Error::FooterGeometry);
    }
    Ok(footer)
}

/// Compute a serialized footer from explicit geometry (validated by planning).
pub fn replacement_footer(footer: Footer) -> [u8; FOOTER_SIZE] {
    let mut bytes = [0; FOOTER_SIZE];
    bytes[..4].copy_from_slice(b"AVBf");
    bytes[4..8].copy_from_slice(&1_u32.to_be_bytes());
    bytes[12..20].copy_from_slice(&footer.original_image_size.to_be_bytes());
    bytes[20..28].copy_from_slice(&footer.vbmeta_offset.to_be_bytes());
    bytes[28..36].copy_from_slice(&footer.vbmeta_size.to_be_bytes());
    bytes
}

#[derive(Clone, Debug)]
pub struct Graft<'a> {
    image_size: u64,
    metadata_offset: u64,
    metadata: &'a [u8],
    footer: [u8; FOOTER_SIZE],
}

pub fn plan_graft<'a>(
    image_size: u64,
    footer_bytes: &[u8],
    metadata: &'a [u8],
) -> Result<Graft<'a>, Error> {
    let original = parse_footer(footer_bytes, image_size)?;
    let size = validate_vbmeta(metadata)?.size();
    if original
        .vbmeta_offset
        .checked_add(size)
        .ok_or(Error::Overflow)?
        > image_size - FOOTER_SIZE as u64
    {
        return Err(Error::ReplacementDoesNotFit);
    }
    Ok(Graft {
        image_size,
        metadata_offset: original.vbmeta_offset,
        metadata: &metadata[..size as usize],
        footer: replacement_footer(Footer {
            vbmeta_size: size,
            ..original
        }),
    })
}

#[derive(Debug, Eq, PartialEq)]
pub enum ReadError<E> {
    Layout(Error),
    Original(E),
}

impl<'a> Graft<'a> {
    pub fn image_size(&self) -> u64 {
        self.image_size
    }
    pub fn metadata_offset(&self) -> u64 {
        self.metadata_offset
    }
    pub fn metadata(&self) -> &'a [u8] {
        self.metadata
    }
    pub fn footer_offset(&self) -> u64 {
        self.image_size - FOOTER_SIZE as u64
    }
    pub fn footer(&self) -> &[u8; FOOTER_SIZE] {
        &self.footer
    }

    /// Serve a logical read without whole-image allocation. The callback sees
    /// only original extents; each overlay is copied directly into the caller.
    pub fn read<E>(
        &self,
        offset: u64,
        out: &mut [u8],
        mut read_original: impl FnMut(u64, &mut [u8]) -> Result<(), E>,
    ) -> Result<(), ReadError<E>> {
        if offset
            .checked_add(out.len() as u64)
            .ok_or(ReadError::Layout(Error::ReadBounds))?
            > self.image_size
        {
            return Err(ReadError::Layout(Error::ReadBounds));
        }
        let metadata_end = self.metadata_offset + self.metadata.len() as u64;
        let mut position = offset;
        let mut remaining = out;
        while !remaining.is_empty() {
            let (end, overlay) = if position < self.metadata_offset {
                (self.metadata_offset, None)
            } else if position < metadata_end {
                (metadata_end, Some((self.metadata, self.metadata_offset)))
            } else if position < self.footer_offset() {
                (self.footer_offset(), None)
            } else {
                (
                    self.image_size,
                    Some((&self.footer[..], self.footer_offset())),
                )
            };
            let count = core::cmp::min(end - position, remaining.len() as u64) as usize;
            let (part, rest) = remaining.split_at_mut(count);
            if let Some((bytes, start)) = overlay {
                let index = (position - start) as usize;
                part.copy_from_slice(&bytes[index..index + count]);
            } else {
                read_original(position, part).map_err(ReadError::Original)?;
            }
            position += count as u64;
            remaining = rest;
        }
        Ok(())
    }

    /// In-memory adapter with the same read-boundary semantics as physical I/O.
    pub fn copy_from(&self, original: &[u8], offset: u64, out: &mut [u8]) -> Result<(), Error> {
        if original.len() as u64 != self.image_size {
            return Err(Error::ReadBounds);
        }
        self.read(offset, out, |position, part| {
            let start = usize::try_from(position).map_err(|_| Error::ReadBounds)?;
            let bytes = original
                .get(start..start + part.len())
                .ok_or(Error::ReadBounds)?;
            part.copy_from_slice(bytes);
            Ok(())
        })
        .map_err(|error| match error {
            ReadError::Layout(error) | ReadError::Original(error) => error,
        })
    }
}
