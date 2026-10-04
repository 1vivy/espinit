// SPDX-License-Identifier: Apache-2.0
use crate::{Discards, Error, SECTOR_SIZE, SegmentType, VolumeGroup, add, mul};
use std::collections::BTreeMap;
use std::fmt::Write;

/// A resolved Linux dev_t, never a path taken from untrusted metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceNumber {
    pub major: u32,
    pub minor: u32,
}
impl std::fmt::Display for DeviceNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.major, self.minor)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    /// Public LV; a pool has LVM's linear front device here.
    Volume,
    /// The actual thin-pool target (LVM's `-tpool` layer).
    ThinPool,
}
#[derive(Debug, Default)]
pub struct Devices {
    /// Keys are formatted PV UUIDs, not the non-authoritative `device` hint.
    pub physical_volumes: BTreeMap<String, DeviceNumber>,
    /// Keys are LV metadata names, then layers. Allocate dependencies first.
    pub logical_volumes: BTreeMap<String, BTreeMap<Layer, DeviceNumber>>,
}
impl Devices {
    fn pv(&self, id: &str) -> Result<DeviceNumber, Error> {
        self.physical_volumes
            .get(id)
            .copied()
            .ok_or_else(|| Error::NotFound(format!("device for PV {id}")))
    }
    fn lv(&self, name: &str, layer: Layer) -> Result<DeviceNumber, Error> {
        self.logical_volumes
            .get(name)
            .and_then(|layers| layers.get(&layer))
            .copied()
            .ok_or_else(|| Error::NotFound(format!("device for LV {name}/{layer:?}")))
    }
}
#[derive(Clone, Copy, Debug, Default)]
pub struct TableOptions {
    /// Activation policy, not persisted LVM metadata. Zero matches lvm2 with
    /// thin_pool_autoextend_threshold=100 (and no dmeventd autoextend).
    pub low_water_mark: u64,
}
#[derive(Debug, PartialEq, Eq)]
pub struct PhysicalExtent<'a> {
    pub logical_offset: u64,
    pub length: u64,
    pub pv_id: &'a str,
    pub physical_offset: u64,
}

impl VolumeGroup {
    /// Physical byte spans for a thick LV, in logical order. Thin mappings need
    /// the separate dm-thin B-tree format and are deliberately not approximated.
    pub fn physical_extents(&self, name: &str) -> Result<Vec<PhysicalExtent<'_>>, Error> {
        let lv = self.lv(name)?;
        let bytes_per_extent = mul(self.extent_size(), SECTOR_SIZE)?;
        let mut result = Vec::with_capacity(lv.segments.len());
        for segment in &lv.segments {
            let SegmentType::Linear { pv, start_extent } = &segment.kind else {
                return Err(Error::Unsupported(
                    "physical extents for a non-thick LV".into(),
                ));
            };
            let physical = &self.physical_volumes()[pv];
            result.push(PhysicalExtent {
                logical_offset: mul(segment.start_extent, bytes_per_extent)?,
                length: mul(segment.extent_count, bytes_per_extent)?,
                pv_id: &physical.id,
                physical_offset: add(
                    mul(physical.pe_start, SECTOR_SIZE)?,
                    mul(*start_extent, bytes_per_extent)?,
                )?,
            });
        }
        Ok(result)
    }

    /// Derive `dmsetup table` lines (512-byte sectors, numeric major:minor).
    /// No ioctls, transaction replay, thin_check, or activation occur here.
    /// `transaction_id`/internal `origin` are metadata facts, not table arguments.
    pub fn dm_table(
        &self,
        name: &str,
        layer: Layer,
        devices: &Devices,
        options: TableOptions,
    ) -> Result<Vec<String>, Error> {
        let lv = self.lv(name)?;
        let mut lines = Vec::with_capacity(lv.segments.len());
        for segment in &lv.segments {
            let start = mul(segment.start_extent, self.extent_size())?;
            let length = mul(segment.extent_count, self.extent_size())?;
            let line = match &segment.kind {
                SegmentType::Linear { pv, start_extent } => {
                    if layer != Layer::Volume {
                        return Err(Error::Invalid("thin-pool layer on linear LV"));
                    }
                    let physical = &self.physical_volumes()[pv];
                    let offset = add(physical.pe_start, mul(*start_extent, self.extent_size())?)?;
                    format!(
                        "{start} {length} linear {} {offset}",
                        devices.pv(&physical.id)?
                    )
                }
                SegmentType::ThinPool {
                    metadata,
                    data,
                    chunk_size,
                    zero_new_blocks,
                    discards,
                    ..
                } => {
                    if layer == Layer::Volume {
                        format!(
                            "{start} {length} linear {} 0",
                            devices.lv(name, Layer::ThinPool)?
                        )
                    } else {
                        if options.low_water_mark > length / chunk_size {
                            return Err(Error::Invalid("thin-pool low water mark"));
                        }
                        let error_when_full = lv
                            .status
                            .iter()
                            .chain(&lv.flags)
                            .any(|flag| flag == "ERROR_WHEN_FULL");
                        let count = usize::from(!zero_new_blocks)
                            + usize::from(*discards != Discards::Passdown)
                            + usize::from(error_when_full);
                        let mut line = format!(
                            "{start} {length} thin-pool {} {} {chunk_size} {} {count} ",
                            devices.lv(metadata, Layer::Volume)?,
                            devices.lv(data, Layer::Volume)?,
                            options.low_water_mark
                        );
                        if !zero_new_blocks {
                            line.push_str("skip_block_zeroing ");
                        }
                        match discards {
                            Discards::Ignore => line.push_str("ignore_discard "),
                            Discards::NoPassdown => line.push_str("no_discard_passdown "),
                            Discards::Passdown => {}
                        }
                        if error_when_full {
                            line.push_str("error_if_no_space ");
                        }
                        line
                    }
                }
                SegmentType::Thin {
                    thin_pool,
                    device_id,
                    ..
                } => {
                    if layer != Layer::Volume {
                        return Err(Error::Invalid("thin-pool layer on thin LV"));
                    }
                    format!(
                        "{start} {length} thin {} {device_id}",
                        devices.lv(thin_pool, Layer::ThinPool)?
                    )
                }
            };
            lines.push(line);
        }
        Ok(lines)
    }

    /// LVM's mapper name, including doubled hyphens and optional pool layer.
    pub fn dm_name(&self, name: &str, layer: Layer) -> Result<String, Error> {
        let lv = self.lv(name)?;
        if layer == Layer::ThinPool && !matches!(lv.segments[0].kind, SegmentType::ThinPool { .. })
        {
            return Err(Error::Invalid("thin-pool name on non-pool LV"));
        }
        let mut result = String::new();
        for part in [self.name(), name] {
            if !result.is_empty() {
                result.push('-');
            }
            for character in part.chars() {
                result.push(character);
                if character == '-' {
                    result.push('-');
                }
            }
        }
        if layer == Layer::ThinPool {
            write!(result, "-tpool").expect("String formatting is infallible");
        }
        Ok(result)
    }
}
