// SPDX-License-Identifier: GPL-3.0-only
use lvm2_meta::{DeviceNumber, Devices, Layer, SegmentType, VolumeGroup};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub start: u64,
    pub length: u64,
    pub kind: String,
    pub params: String,
}

pub trait Mapper {
    fn activate(&mut self, name: &str, targets: &[Target]) -> Result<DeviceNumber, String>;
}

fn targets(lines: Vec<String>) -> Result<Vec<Target>, String> {
    lines
        .into_iter()
        .map(|line| {
            let mut fields = line.splitn(4, ' ');
            let start = fields
                .next()
                .ok_or("missing table start")?
                .parse()
                .map_err(|_| "invalid table start")?;
            let length = fields
                .next()
                .ok_or("missing table length")?
                .parse()
                .map_err(|_| "invalid table length")?;
            let kind = fields.next().ok_or("missing table target")?.to_owned();
            let params = fields.next().unwrap_or_default().trim_end().to_owned();
            if kind.is_empty() || kind.len() >= 16 || params.as_bytes().contains(&0) {
                return Err("invalid derived device-mapper table".to_owned());
            }
            Ok(Target {
                start,
                length,
                kind,
                params,
            })
        })
        .collect()
}

fn activate_one(
    vg: &VolumeGroup,
    name: &str,
    layer: Layer,
    devices: &mut Devices,
    active: &mut BTreeSet<(String, Layer)>,
    visiting: &mut BTreeSet<(String, Layer)>,
    mapper: &mut impl Mapper,
) -> Result<(), String> {
    let key = (name.to_owned(), layer);
    if active.contains(&key) {
        return Ok(());
    }
    if !visiting.insert(key.clone()) {
        return Err(format!("LVM dependency cycle at {name}/{layer:?}"));
    }

    let lv = vg.lv(name).map_err(|error| error.to_string())?;
    match layer {
        Layer::Volume => {
            for segment in &lv.segments {
                match &segment.kind {
                    SegmentType::Linear { .. } => {}
                    SegmentType::ThinPool { .. } => {
                        activate_one(vg, name, Layer::ThinPool, devices, active, visiting, mapper)?
                    }
                    SegmentType::Thin { thin_pool, .. } => activate_one(
                        vg,
                        thin_pool,
                        Layer::ThinPool,
                        devices,
                        active,
                        visiting,
                        mapper,
                    )?,
                }
            }
        }
        Layer::ThinPool => {
            for segment in &lv.segments {
                let SegmentType::ThinPool { metadata, data, .. } = &segment.kind else {
                    return Err(format!("{name} is not entirely a thin pool"));
                };
                activate_one(
                    vg,
                    metadata,
                    Layer::Volume,
                    devices,
                    active,
                    visiting,
                    mapper,
                )?;
                activate_one(vg, data, Layer::Volume, devices, active, visiting, mapper)?;
            }
        }
    }

    let table = vg
        .dm_table(name, layer, devices, Default::default())
        .map_err(|error| error.to_string())?;
    let dm_name = vg.dm_name(name, layer).map_err(|error| error.to_string())?;
    let number = mapper.activate(&dm_name, &targets(table)?)?;
    devices
        .logical_volumes
        .entry(name.to_owned())
        .or_default()
        .insert(layer, number);
    visiting.remove(&key);
    active.insert(key);
    Ok(())
}

pub fn activate_visible(
    vg: &VolumeGroup,
    pv_number: DeviceNumber,
    mapper: &mut impl Mapper,
) -> Result<Devices, String> {
    if vg.name() != "rom" {
        return Err(format!("expected VG rom, found {}", vg.name()));
    }
    if vg.physical_volumes().len() != 1 {
        return Err("VG rom must contain exactly one physical volume".to_owned());
    }
    for required in ["metadata_1", "userdata_1"] {
        let lv = vg.lv(required).map_err(|error| error.to_string())?;
        if !lv
            .segments
            .iter()
            .all(|segment| matches!(segment.kind, SegmentType::Thin { .. }))
        {
            return Err(format!("required LV {required} is not thin"));
        }
    }

    let mut devices = Devices::default();
    for pv in vg.physical_volumes().values() {
        devices.physical_volumes.insert(pv.id.clone(), pv_number);
    }
    let mut active = BTreeSet::new();
    let mut visiting = BTreeSet::new();
    for (name, lv) in vg.logical_volumes() {
        if lv.status.iter().any(|status| status == "VISIBLE")
            && !lv.flags.iter().any(|flag| flag == "ACTIVATION_SKIP")
        {
            activate_one(
                vg,
                name,
                Layer::Volume,
                &mut devices,
                &mut active,
                &mut visiting,
                mapper,
            )?;
        }
    }
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recording {
        next_minor: u32,
        calls: Vec<(String, Vec<Target>)>,
    }

    impl Mapper for Recording {
        fn activate(&mut self, name: &str, targets: &[Target]) -> Result<DeviceNumber, String> {
            self.calls.push((name.to_owned(), targets.to_vec()));
            let result = DeviceNumber {
                major: 252,
                minor: self.next_minor,
            };
            self.next_minor += 1;
            Ok(result)
        }
    }

    #[test]
    fn activates_real_lvm_dependencies_before_visible_volumes() {
        let vg = VolumeGroup::parse(include_str!("../tests/fixtures/rom-vg.txt")).unwrap();
        let mut mapper = Recording::default();
        activate_visible(&vg, DeviceNumber { major: 7, minor: 0 }, &mut mapper).unwrap();

        let names: Vec<_> = mapper.calls.iter().map(|(name, _)| name.as_str()).collect();
        let pool = names
            .iter()
            .position(|name| *name == "rom-pool-tpool")
            .unwrap();
        let metadata = names
            .iter()
            .position(|name| *name == "rom-pool_tmeta")
            .unwrap();
        let data = names
            .iter()
            .position(|name| *name == "rom-pool_tdata")
            .unwrap();
        let userdata = names
            .iter()
            .position(|name| *name == "rom-userdata_1")
            .unwrap();
        assert!(metadata < pool && data < pool && pool < userdata);

        let pool_table = &mapper.calls[pool].1;
        assert_eq!(pool_table.len(), 1);
        assert_eq!(pool_table[0].kind, "thin-pool");
        assert!(
            pool_table[0]
                .params
                .ends_with("skip_block_zeroing no_discard_passdown")
        );
    }

    #[test]
    fn refuses_wrong_volume_group() {
        let source = include_str!("../tests/fixtures/rom-vg.txt").replacen("rom {", "other {", 1);
        let vg = VolumeGroup::parse(&source).unwrap();
        let error = activate_visible(
            &vg,
            DeviceNumber { major: 7, minor: 0 },
            &mut Recording::default(),
        )
        .unwrap_err();
        assert_eq!(error, "expected VG rom, found other");
    }
}
