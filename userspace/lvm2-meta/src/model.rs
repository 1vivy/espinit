// SPDX-License-Identifier: Apache-2.0
use crate::{
    Error, SECTOR_SIZE, add, mul,
    text::{self, Section, Value},
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalVolume {
    pub id: String,
    /// Hint only; never used as device identity by table derivation.
    pub device: Option<String>,
    pub dev_size: Option<u64>,
    pub pe_start: u64,
    pub pe_count: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicalVolume {
    pub id: String,
    pub status: Vec<String>,
    pub flags: Vec<String>,
    pub segments: Vec<Segment>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub start_extent: u64,
    pub extent_count: u64,
    pub kind: SegmentType,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discards {
    Passdown,
    NoPassdown,
    Ignore,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SegmentType {
    /// On disk this is type="striped", stripe_count=1.
    Linear { pv: String, start_extent: u64 },
    ThinPool {
        metadata: String,
        data: String,
        chunk_size: u64,
        transaction_id: u64,
        zero_new_blocks: bool,
        discards: Discards,
    },
    Thin {
        thin_pool: String,
        device_id: u32,
        transaction_id: u64,
        origin: Option<String>,
    },
}
#[derive(Debug)]
pub struct VolumeGroup {
    name: String,
    id: String,
    seqno: u64,
    extent_size: u64,
    pvs: BTreeMap<String, PhysicalVolume>,
    lvs: BTreeMap<String, LogicalVolume>,
}

fn required<'a, 'b>(s: &'b Section<'a>, key: &str) -> Result<&'b Value<'a>, Error> {
    s.get(key).ok_or(Error::Invalid("missing metadata field"))
}
fn uint(s: &Section<'_>, key: &str) -> Result<u64, Error> {
    required(s, key)?.unsigned()
}
fn string(s: &Section<'_>, key: &str) -> Result<String, Error> {
    Ok(required(s, key)?.string()?.into())
}
fn optional_string(s: &Section<'_>, key: &str) -> Result<Option<String>, Error> {
    s.get(key)
        .map(|v| v.string().map(str::to_owned))
        .transpose()
}
fn strings(s: &Section<'_>, key: &str) -> Result<Vec<String>, Error> {
    s.get(key)
        .map(|v| {
            v.array()?
                .iter()
                .map(|v| v.string().map(str::to_owned))
                .collect()
        })
        .unwrap_or_else(|| Ok(Vec::new()))
}
fn id(s: &Section<'_>) -> Result<String, Error> {
    let id = string(s, "id")?;
    let lengths = [6, 4, 4, 4, 4, 4, 6];
    let mut parts = id.split('-');
    if lengths.into_iter().any(|len| {
        parts
            .next()
            .is_none_or(|p| p.len() != len || !p.bytes().all(|b| b.is_ascii_alphanumeric()))
    }) || parts.next().is_some()
    {
        return Err(Error::Invalid("UUID"));
    }
    Ok(id)
}
fn name(value: &str) -> Result<(), Error> {
    if value.is_empty()
        || value.len() > 127
        || value.starts_with('-')
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.+".contains(&b))
    {
        return Err(Error::Invalid("volume name"));
    }
    Ok(())
}
fn segment(s: &Section<'_>) -> Result<Segment, Error> {
    let kind = required(s, "type")?.string()?;
    let allowed: &[&str] = match kind {
        "striped" => &["stripe_count", "stripe_size", "stripes"],
        "thin-pool" => &[
            "metadata",
            "pool",
            "chunk_size",
            "transaction_id",
            "zero_new_blocks",
            "discards",
            "crop_metadata",
        ],
        "thin" => &["thin_pool", "device_id", "transaction_id", "origin"],
        other => return Err(Error::Unsupported(format!("segment type {other}"))),
    };
    for key in s.keys() {
        if !["start_extent", "extent_count", "type", "tags"].contains(&key.as_ref())
            && !allowed.contains(&key.as_ref())
        {
            return Err(Error::Unsupported(format!("{kind} field {key}")));
        }
    }
    let kind = match kind {
        "striped" => {
            let count = uint(s, "stripe_count")?;
            if count != 1 {
                return Err(Error::Unsupported(format!("stripe_count={count}")));
            }
            let stripes = required(s, "stripes")?.array()?;
            if stripes.len() != 2 {
                return Err(Error::Invalid("linear stripes"));
            }
            SegmentType::Linear {
                pv: stripes[0].string()?.into(),
                start_extent: stripes[1].unsigned()?,
            }
        }
        "thin-pool" => {
            let zero = s
                .get("zero_new_blocks")
                .map(Value::unsigned)
                .transpose()?
                .unwrap_or(0);
            if zero > 1 {
                return Err(Error::Invalid("zero_new_blocks"));
            }
            let crop = s
                .get("crop_metadata")
                .map(Value::unsigned)
                .transpose()?
                .unwrap_or(0);
            if crop != 0 {
                return Err(Error::Unsupported("cropped thin metadata".into()));
            }
            let discards = match s
                .get("discards")
                .map(Value::string)
                .transpose()?
                .unwrap_or("ignore")
            {
                "passdown" => Discards::Passdown,
                "nopassdown" => Discards::NoPassdown,
                "ignore" => Discards::Ignore,
                _ => return Err(Error::Invalid("discards")),
            };
            let chunk_size = uint(s, "chunk_size")?;
            if !(128..=2097152).contains(&chunk_size) || !chunk_size.is_multiple_of(128) {
                return Err(Error::Invalid("thin chunk_size"));
            }
            SegmentType::ThinPool {
                metadata: string(s, "metadata")?,
                data: string(s, "pool")?,
                chunk_size,
                transaction_id: uint(s, "transaction_id")?,
                zero_new_blocks: zero == 1,
                discards,
            }
        }
        "thin" => {
            let device_id = uint(s, "device_id")?;
            if device_id >= 1 << 24 {
                return Err(Error::Invalid("thin device_id"));
            }
            SegmentType::Thin {
                thin_pool: string(s, "thin_pool")?,
                device_id: device_id as u32,
                transaction_id: uint(s, "transaction_id")?,
                origin: optional_string(s, "origin")?,
            }
        }
        _ => unreachable!(),
    };
    Ok(Segment {
        start_extent: uint(s, "start_extent")?,
        extent_count: uint(s, "extent_count")?,
        kind,
    })
}

impl VolumeGroup {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn seqno(&self) -> u64 {
        self.seqno
    }
    /// Sectors (512 bytes), not bytes or hardware logical blocks.
    pub fn extent_size(&self) -> u64 {
        self.extent_size
    }
    pub fn physical_volumes(&self) -> &BTreeMap<String, PhysicalVolume> {
        &self.pvs
    }
    pub fn logical_volumes(&self) -> &BTreeMap<String, LogicalVolume> {
        &self.lvs
    }
    pub fn lv(&self, name: &str) -> Result<&LogicalVolume, Error> {
        self.lvs
            .get(name)
            .ok_or_else(|| Error::NotFound(name.into()))
    }
    /// Parse text already supplied by a trusted transport. Use `read` for disk
    /// data: this entry point does not verify a label or any on-disk checksum.
    pub fn parse(input: &str) -> Result<Self, Error> {
        let root = text::parse(input)?;
        if uint(&root, "version")? != 1
            || required(&root, "contents")?.string()? != "Text Format Volume Group"
        {
            return Err(Error::Invalid("text format version"));
        }
        let mut groups = root.iter().filter(|(_, v)| matches!(v, Value::Section(_)));
        let (vg_name, vg) = groups.next().ok_or(Error::Invalid("missing VG"))?;
        if groups.next().is_some() {
            return Err(Error::Invalid("multiple VGs"));
        }
        name(vg_name)?;
        let vg = vg.section()?;
        let extent_size = uint(vg, "extent_size")?;
        if extent_size == 0 || extent_size > u64::from(u32::MAX) {
            return Err(Error::Invalid("extent_size"));
        }
        let mut pvs = BTreeMap::new();
        for (key, value) in required(vg, "physical_volumes")?.section()? {
            name(key)?;
            let value = value.section()?;
            pvs.insert(
                key.to_string(),
                PhysicalVolume {
                    id: id(value)?,
                    device: optional_string(value, "device")?,
                    dev_size: value.get("dev_size").map(Value::unsigned).transpose()?,
                    pe_start: uint(value, "pe_start")?,
                    pe_count: uint(value, "pe_count")?,
                },
            );
        }
        let mut lvs = BTreeMap::new();
        if let Some(volumes) = vg.get("logical_volumes") {
            for (key, value) in volumes.section()? {
                name(key)?;
                let value = value.section()?;
                let mut segments = Vec::new();
                for (key, seg) in value.iter().filter(|(_, v)| matches!(v, Value::Section(_))) {
                    if !key
                        .strip_prefix("segment")
                        .is_some_and(|s| s.parse::<u64>().is_ok_and(|n| n > 0))
                    {
                        return Err(Error::Unsupported(format!("LV section {key}")));
                    }
                    segments.push(segment(seg.section()?)?);
                }
                if segments.len() as u64 != uint(value, "segment_count")? || segments.is_empty() {
                    return Err(Error::Invalid("segment_count"));
                }
                segments.sort_by_key(|s| s.start_extent);
                let status = strings(value, "status")?;
                let flags = strings(value, "flags")?;
                for flag in status.iter().chain(&flags) {
                    if ![
                        "READ",
                        "WRITE",
                        "VISIBLE",
                        "FIXED_MINOR",
                        "ACTIVATION_SKIP",
                        "ERROR_WHEN_FULL",
                    ]
                    .contains(&flag.as_str())
                    {
                        return Err(Error::Unsupported(format!("LV status/flag {flag}")));
                    }
                }
                lvs.insert(
                    key.to_string(),
                    LogicalVolume {
                        id: id(value)?,
                        status,
                        flags,
                        segments,
                    },
                );
            }
        }
        let result = Self {
            name: vg_name.to_string(),
            id: id(vg)?,
            seqno: uint(vg, "seqno")?,
            extent_size,
            pvs,
            lvs,
        };
        result.validate()?;
        Ok(result)
    }
    fn validate(&self) -> Result<(), Error> {
        if self.pvs.is_empty() || self.seqno == 0 {
            return Err(Error::Invalid("empty VG or seqno"));
        }
        let mut ids = BTreeSet::new();
        for pv in self.pvs.values() {
            if !ids.insert(&pv.id) {
                return Err(Error::Invalid("duplicate PV UUID"));
            }
            let end = add(pv.pe_start, mul(pv.pe_count, self.extent_size)?)?;
            mul(end, SECTOR_SIZE)?;
            if pv.pe_start < 4 || pv.dev_size.is_some_and(|size| size < end) {
                return Err(Error::Invalid("PV extent bounds"));
            }
        }
        ids.clear();
        let mut thin_ids = BTreeSet::new();
        let mut allocations = BTreeMap::<&str, Vec<(u64, u64)>>::new();
        for (lv_name, lv) in &self.lvs {
            if !ids.insert(&lv.id) {
                return Err(Error::Invalid("duplicate LV UUID"));
            }
            let mut end = 0;
            for seg in &lv.segments {
                if seg.extent_count == 0 || seg.start_extent != end {
                    return Err(Error::Invalid("LV gap or overlap"));
                }
                end = add(end, seg.extent_count)?;
                mul(mul(end, self.extent_size)?, SECTOR_SIZE)?;
                match &seg.kind {
                    SegmentType::Linear { pv, start_extent } => {
                        let physical = self.pvs.get(pv).ok_or_else(|| {
                            Error::Unsupported(format!("non-PV linear reference {pv}"))
                        })?;
                        let end = add(*start_extent, seg.extent_count)?;
                        if end > physical.pe_count {
                            return Err(Error::Invalid("segment outside PV"));
                        }
                        allocations
                            .entry(pv)
                            .or_default()
                            .push((*start_extent, end));
                    }
                    SegmentType::ThinPool { metadata, data, .. } => {
                        if lv.segments.len() != 1 || metadata == data {
                            return Err(Error::Invalid("thin-pool layout"));
                        }
                        for target in [metadata, data] {
                            let backing = self.lv(target)?;
                            if !backing
                                .segments
                                .iter()
                                .all(|s| matches!(s.kind, SegmentType::Linear { .. }))
                            {
                                return Err(Error::Unsupported(
                                    "nonlinear thin-pool backing".into(),
                                ));
                            }
                        }
                        if self.lv(data)?.extent_count() != seg.extent_count {
                            return Err(Error::Invalid("thin-pool data size"));
                        }
                    }
                    SegmentType::Thin {
                        thin_pool,
                        device_id,
                        transaction_id,
                        origin,
                    } => {
                        if lv.segments.len() != 1 || !thin_ids.insert((thin_pool, device_id)) {
                            return Err(Error::Invalid("thin layout or duplicate device_id"));
                        }
                        let pool = self.lv(thin_pool)?;
                        let [
                            Segment {
                                kind:
                                    SegmentType::ThinPool {
                                        transaction_id: pool_transaction,
                                        ..
                                    },
                                ..
                            },
                        ] = pool.segments.as_slice()
                        else {
                            return Err(Error::Invalid("thin_pool reference"));
                        };
                        if transaction_id > pool_transaction {
                            return Err(Error::Invalid("thin transaction_id"));
                        }
                        if let Some(origin) = origin {
                            if origin == lv_name {
                                return Err(Error::Invalid("self origin"));
                            }
                            let [
                                Segment {
                                    kind:
                                        SegmentType::Thin {
                                            thin_pool: origin_pool,
                                            ..
                                        },
                                    ..
                                },
                            ] = self.lv(origin)?.segments.as_slice()
                            else {
                                return Err(Error::Invalid("thin origin"));
                            };
                            if origin_pool != thin_pool {
                                return Err(Error::Invalid("cross-pool origin"));
                            }
                        }
                    }
                }
            }
        }
        for regions in allocations.values_mut() {
            regions.sort_unstable();
            if regions.windows(2).any(|w| w[0].1 > w[1].0) {
                return Err(Error::Invalid("overlapping physical allocations"));
            }
        }
        for lv in self.lvs.values() {
            let mut current = lv;
            let mut hops = 0;
            while let SegmentType::Thin {
                origin: Some(origin),
                ..
            } = &current.segments[0].kind
            {
                hops += 1;
                if hops >= self.lvs.len() {
                    return Err(Error::Invalid("origin cycle"));
                }
                current = self.lv(origin)?;
            }
        }
        Ok(())
    }
}
impl LogicalVolume {
    pub fn extent_count(&self) -> u64 {
        self.segments
            .last()
            .map_or(0, |s| s.start_extent + s.extent_count)
    }
}
