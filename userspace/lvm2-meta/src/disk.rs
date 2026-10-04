// SPDX-License-Identifier: Apache-2.0
use crate::{Error, MAX_TEXT_BYTES, ReadAt, SECTOR_SIZE, VolumeGroup, add, checksum, mul};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskArea {
    pub offset: u64,
    /// Zero means unspecified; VG extents must still exclude every MDA.
    pub size: u64,
}
#[derive(Clone, Debug)]
pub struct PhysicalVolumeHeader {
    pub label_sector: u64,
    /// The 32 ASCII bytes as stored on disk (without UUID separators).
    pub id: String,
    pub device_size: u64,
    pub data_areas: Vec<DiskArea>,
    pub metadata_areas: Vec<DiskArea>,
}
#[derive(Clone, Debug)]
pub struct RawLocation {
    pub offset: u64,
    pub size: u64,
    pub checksum: u32,
    pub ignored: bool,
}
#[derive(Clone, Debug)]
pub struct MetadataArea {
    pub area: DiskArea,
    /// Entry zero is committed; later entries are precommitted, never selected.
    pub locations: Vec<RawLocation>,
}
#[derive(Debug)]
pub struct Metadata {
    pub pv: PhysicalVolumeHeader,
    pub areas: Vec<MetadataArea>,
    pub text: String,
    pub vg: VolumeGroup,
}

fn bytes<const N: usize>(raw: &[u8], offset: usize) -> Result<[u8; N], Error> {
    raw.get(offset..offset.checked_add(N).ok_or(Error::Invalid("offset"))?)
        .and_then(|v| v.try_into().ok())
        .ok_or(Error::Invalid("truncated header"))
}
fn u32_at(raw: &[u8], offset: usize) -> Result<u32, Error> {
    Ok(u32::from_le_bytes(bytes(raw, offset)?))
}
fn u64_at(raw: &[u8], offset: usize) -> Result<u64, Error> {
    Ok(u64::from_le_bytes(bytes(raw, offset)?))
}

fn areas(
    raw: &[u8],
    cursor: &mut usize,
    device_size: u64,
    data: bool,
) -> Result<Vec<DiskArea>, Error> {
    let mut result = Vec::new();
    loop {
        let offset = u64_at(raw, *cursor)?;
        let size = u64_at(raw, *cursor + 8)?;
        *cursor += 16;
        if offset == 0 {
            if size != 0 {
                return Err(Error::Invalid("area terminator"));
            }
            return Ok(result);
        }
        if offset < 2048
            || !offset.is_multiple_of(SECTOR_SIZE)
            || !size.is_multiple_of(SECTOR_SIZE)
            || offset >= device_size
            || add(offset, size)? > device_size
            || (!data && size < 1024)
        {
            return Err(Error::Invalid("PV area bounds"));
        }
        let end = add(offset, size.max(1))?;
        if result.iter().any(|a: &DiskArea| {
            let other_end = a.offset + a.size.max(1);
            offset < other_end && a.offset < end
        }) {
            return Err(Error::Invalid("overlapping PV areas"));
        }
        result.push(DiskArea { offset, size });
    }
}

fn label(source: &mut impl ReadAt) -> Result<PhysicalVolumeHeader, Error> {
    let mut scan = [0; 2048];
    source.read_exact_at(0, &mut scan)?;
    let mut found = None;
    for (sector, raw) in scan.as_chunks::<512>().0.iter().enumerate() {
        if &raw[..8] != b"LABELONE" {
            continue;
        }
        if found.is_some() {
            return Err(Error::Invalid("multiple labels"));
        }
        if checksum(&raw[20..]) != u32_at(raw, 16)? {
            return Err(Error::Checksum("label"));
        }
        if u64_at(raw, 8)? != sector as u64 || &raw[24..32] != b"LVM2 001" {
            return Err(Error::Invalid("label type or sector"));
        }
        let start = u32_at(raw, 20)? as usize;
        if !(32..=432).contains(&start) {
            return Err(Error::Invalid("PV header offset"));
        }
        let id = bytes::<32>(raw, start)?;
        if !id.iter().all(u8::is_ascii_alphanumeric) {
            return Err(Error::Invalid("PV UUID"));
        }
        let device_size = u64_at(raw, start + 32)?;
        if !device_size.is_multiple_of(SECTOR_SIZE) {
            return Err(Error::Invalid("PV size"));
        }
        let mut cursor = start + 40;
        let data_areas = areas(raw, &mut cursor, device_size, true)?;
        let metadata_areas = areas(raw, &mut cursor, device_size, false)?;
        if data_areas.is_empty() {
            return Err(Error::Invalid("missing data area"));
        }
        for mda in &metadata_areas {
            if data_areas.iter().any(|a| {
                let end = a.offset + a.size.max(1);
                a.offset < mda.offset + mda.size && mda.offset < end
            }) {
                return Err(Error::Invalid("metadata overlaps data"));
            }
        }
        found = Some(PhysicalVolumeHeader {
            label_sector: sector as u64,
            id: String::from_utf8(id.to_vec()).map_err(|_| Error::Invalid("PV UUID"))?,
            device_size,
            data_areas,
            metadata_areas,
        });
    }
    found.ok_or(Error::MissingLabel)
}

fn mda(source: &mut impl ReadAt, area: &DiskArea) -> Result<MetadataArea, Error> {
    let mut raw = [0; 512];
    source.read_exact_at(area.offset, &mut raw)?;
    if checksum(&raw[4..]) != u32_at(&raw, 0)? {
        return Err(Error::Checksum("MDA"));
    }
    if &raw[4..20] != b" LVM2 x[5A%r0N*>" || u32_at(&raw, 20)? != 1 {
        return Err(Error::Invalid("MDA magic or version"));
    }
    if u64_at(&raw, 24)? != area.offset || u64_at(&raw, 32)? != area.size {
        return Err(Error::Invalid("MDA location"));
    }
    let mut locations = Vec::new();
    let mut cursor = 40;
    loop {
        let offset = u64_at(&raw, cursor)?;
        let size = u64_at(&raw, cursor + 8)?;
        let checksum = u32_at(&raw, cursor + 16)?;
        let flags = u32_at(&raw, cursor + 20)?;
        cursor += 24;
        if flags & !1 != 0 {
            return Err(Error::Unsupported("raw_locn flags".into()));
        }
        if offset == 0 {
            if size != 0 {
                return Err(Error::Invalid("raw_locn terminator"));
            }
            break;
        }
        if offset < 512
            || offset >= area.size
            || !offset.is_multiple_of(SECTOR_SIZE)
            || size == 0
            || size > area.size - 512
        {
            return Err(Error::Invalid("raw_locn bounds"));
        }
        if size > MAX_TEXT_BYTES as u64 {
            return Err(Error::Limit("metadata text"));
        }
        locations.push(RawLocation {
            offset,
            size,
            checksum,
            ignored: flags == 1,
        });
    }
    Ok(MetadataArea {
        area: area.clone(),
        locations,
    })
}

fn extract(
    source: &mut impl ReadAt,
    mda: &MetadataArea,
    raw: &RawLocation,
) -> Result<String, Error> {
    let mut text = vec![0; raw.size as usize];
    let first = raw.size.min(mda.area.size - raw.offset) as usize;
    source.read_exact_at(add(mda.area.offset, raw.offset)?, &mut text[..first])?;
    if first < text.len() {
        // The circular buffer resumes after, never over, the MDA header.
        source.read_exact_at(add(mda.area.offset, 512)?, &mut text[first..])?;
    }
    if checksum(&text) != raw.checksum {
        return Err(Error::Checksum("metadata text"));
    }
    if text.last() == Some(&0) {
        text.pop();
    }
    String::from_utf8(text).map_err(|_| Error::Invalid("metadata UTF-8"))
}

/// Read all listed MDAs fail-closed, choosing the highest committed VG seqno.
/// Equal-seqno disagreement or a different VG identity is an error. This is not
/// a recovery reader: a corrupt copy is not silently bypassed by another copy.
pub fn read(source: &mut impl ReadAt) -> Result<Metadata, Error> {
    let pv = label(source)?;
    let mut areas = Vec::new();
    let mut selected: Option<(String, VolumeGroup)> = None;
    for area in &pv.metadata_areas {
        let mda = mda(source, area)?;
        if let Some(raw) = mda.locations.first().filter(|r| !r.ignored) {
            let text = extract(source, &mda, raw)?;
            let vg = VolumeGroup::parse(&text)?;
            if let Some((previous, group)) = &selected
                && (group.id() != vg.id()
                    || group.name() != vg.name()
                    || (group.seqno() == vg.seqno() && previous != &text))
            {
                return Err(Error::ConflictingMetadata);
            }
            if selected
                .as_ref()
                .is_none_or(|(_, group)| vg.seqno() > group.seqno())
            {
                selected = Some((text, vg));
            }
        }
        areas.push(mda);
    }
    let (text, vg) = selected.ok_or(Error::Invalid("no committed metadata"))?;
    let member = vg
        .physical_volumes()
        .values()
        .find(|member| member.id.bytes().filter(|b| *b != b'-').eq(pv.id.bytes()))
        .ok_or(Error::Invalid("PV not in VG"))?;
    let start = mul(member.pe_start, SECTOR_SIZE)?;
    let end = add(
        start,
        mul(mul(member.pe_count, vg.extent_size())?, SECTOR_SIZE)?,
    )?;
    if end > pv.device_size
        || member
            .dev_size
            .is_some_and(|s| s > pv.device_size / SECTOR_SIZE)
        || !pv.data_areas.iter().any(|a| {
            let area_end = if a.size != 0 {
                a.offset + a.size
            } else {
                pv.data_areas
                    .iter()
                    .chain(&pv.metadata_areas)
                    .filter(|other| other.offset > a.offset)
                    .map(|other| other.offset)
                    .min()
                    .unwrap_or(pv.device_size)
            };
            a.offset <= start && end <= area_end
        })
    {
        return Err(Error::Invalid("VG extents outside PV data area"));
    }
    Ok(Metadata {
        pv,
        areas,
        text,
        vg,
    })
}
