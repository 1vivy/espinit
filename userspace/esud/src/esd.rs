//! The esd device-name tree below `/dev/block/esd/`.
//!
//! LVM, the boot HAL and the ROM OTA address storage by name instead of
//! scanning sysfs: the physical `userdata` PV, every physical partition the GPT
//! projection hides, every active `rom-` device-mapper node and the
//! device-mapper control node are published as block or char nodes under one
//! directory. esud owns that tree because it is the only esu program already
//! running as root at its `early` stage, before any `early_hal` service; the
//! static `lvm` and the HAL need the names after that point.
//!
//! `refresh` is idempotent and reconciles: it re-plans the whole tree from sysfs
//! on every call and removes the nodes whose device is gone, so a promotion that
//! removed a staging LV leaves no stale name behind. Every entry is a real
//! device node, never a symlink, so no later walker can be redirected out of
//! the tree; each node is created with mode 0600 and the `esu_blk_device`
//! SELinux type the policy declares for it.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use esu_platform::identifier;
use ota_core::{
    ESD_ETC, ESD_LOCK, ESD_PV, ESD_ROOT, ESD_RUN, POOL_TDATA, esd_by_name, esd_lv, esd_mapper,
    names::ESD_CONTEXT,
};

use crate::overlay;

/// sysfs block-class directory.
const BLOCK_CLASS: &str = "/sys/class/block";

/// `/proc/misc`, where the kernel publishes dynamic misc minors.
const PROC_MISC: &str = "/proc/misc";

/// Misc device that owns the device-mapper control node.
const DEVICE_MAPPER: &str = "device-mapper";

/// Device-mapper's fixed misc major.
const DEVICE_MAPPER_MAJOR: u32 = 10;

/// Control node of the mapper directory.
const CONTROL: &str = "control";

/// Prefix LVM gives the device-mapper name of every `rom` volume group member.
const ROM_PREFIX: &str = "rom-";

/// Mode of a published device node: root only, no group or world access.
const NODE_MODE: u32 = 0o600;

/// Mode of a directory of the tree.
const DIRECTORY_MODE: u32 = 0o755;

/// Kind of a published node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Block,
    Char,
}

impl Kind {
    /// The `mknod` file type of this kind.
    const fn file_type(self) -> libc::mode_t {
        match self {
            Self::Block => libc::S_IFBLK,
            Self::Char => libc::S_IFCHR,
        }
    }

    /// The kind of an existing inode, or `None` for anything else.
    fn of(kind: fs::FileType) -> Option<Self> {
        if kind.is_block_device() {
            Some(Self::Block)
        } else if kind.is_char_device() {
            Some(Self::Char)
        } else {
            None
        }
    }
}

/// One node the tree must contain.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    /// Path relative to the tree root, for example `by-name/boot_a`.
    relative: String,
    kind: Kind,
    /// Resolved device number; the node's identity.
    device: u64,
}

/// The sysfs roots the plan is derived from. A test supplies its own; the
/// command always reads the real ones.
struct Sysfs {
    block: PathBuf,
    misc: PathBuf,
}

impl Sysfs {
    fn system() -> Self {
        Self {
            block: PathBuf::from(BLOCK_CLASS),
            misc: PathBuf::from(PROC_MISC),
        }
    }
}

/// One block device, as sysfs describes it.
struct Block {
    /// sysfs directory name, for example `sda16` or `dm-3`.
    name: String,
    /// Contents of the `uevent` attribute.
    uevent: String,
    /// Device number from the `dev` attribute.
    device: u64,
    /// `dm/name` of a device-mapper logical device.
    mapper: Option<String>,
}

/// Publish the device-name tree, called by the `ota` module's `early.sh` and by
/// the boot HAL after it created or removed a staging LV.
pub fn refresh() -> Result<()> {
    let plan = plan(&Sysfs::system())?;
    publish(Path::new(ESD_ROOT), &plan)
}

/// Every node the tree must contain, in a deterministic order.
fn plan(sysfs: &Sysfs) -> Result<Vec<Node>> {
    let blocks = blocks(&sysfs.block)?;
    let mut nodes = Vec::new();
    nodes.push(Node {
        relative: relative(ESD_PV)?,
        kind: Kind::Block,
        device: pool_slave(&sysfs.block, &blocks)?,
    });
    for (name, device) in partitions(&blocks) {
        nodes.push(Node {
            relative: relative(&esd_by_name(&name))?,
            kind: Kind::Block,
            device,
        });
    }
    for (name, device) in volumes(&blocks) {
        nodes.push(Node {
            relative: relative(&esd_lv(&name))?,
            kind: Kind::Block,
            device,
        });
    }
    nodes.push(Node {
        relative: relative(&esd_mapper(CONTROL))?,
        kind: Kind::Char,
        device: libc::makedev(DEVICE_MAPPER_MAJOR, mapper_minor(&sysfs.misc)?),
    });
    nodes.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(nodes)
}

/// Read every block device of one sysfs block-class directory.
fn blocks(root: &Path) -> Result<Vec<Block>> {
    let mut entries = fs::read_dir(root)
        .with_context(|| format!("read {}", root.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut blocks = Vec::new();
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-UTF8 sysfs block name"))?;
        let directory = entry.path();
        let uevent = match fs::read_to_string(directory.join("uevent")) {
            Ok(uevent) => uevent,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let device = match fs::read_to_string(directory.join("dev")) {
            Ok(device) => device,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let mapper = match fs::read_to_string(directory.join("dm/name")) {
            Ok(mapper) => Some(mapper.trim().to_owned()),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        blocks.push(Block {
            name,
            uevent,
            device: device_number(device.trim())?,
            mapper,
        });
    }
    Ok(blocks)
}

/// Every physical partition under its original `PARTNAME`.
///
/// A name the kernel reports on two devices is published for neither: a
/// consumer addressing the name would reach an arbitrary one of them, which is
/// worse than a missing node, so the collision is logged and dropped.
fn partitions(blocks: &[Block]) -> Vec<(String, u64)> {
    let mut seen: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for block in blocks {
        if field(&block.uevent, "DEVTYPE") != Some("partition") {
            continue;
        }
        let Some(name) = field(&block.uevent, "PARTNAME") else {
            continue;
        };
        if let Err(error) = identifier(name) {
            log::warn!("esd: PARTNAME {name:?} is unusable: {error}");
            continue;
        }
        seen.entry(name.to_owned()).or_default().push(block.device);
    }
    let mut published = Vec::new();
    for (name, devices) in seen {
        match devices.as_slice() {
            [device] => published.push((name, *device)),
            devices => log::warn!(
                "esd: PARTNAME {name} is on {} devices; published for neither",
                devices.len()
            ),
        }
    }
    published
}

/// Every active `rom` volume group member under its LVM name.
fn volumes(blocks: &[Block]) -> Vec<(String, u64)> {
    let mut volumes = Vec::new();
    for block in blocks {
        let Some(mapper) = &block.mapper else {
            continue;
        };
        let Some(name) = lv_name(mapper) else {
            continue;
        };
        if let Err(error) = identifier(&name) {
            log::warn!("esd: mapper {mapper:?} is unusable: {error}");
            continue;
        }
        volumes.push((name, block.device));
    }
    volumes.sort();
    volumes
}

/// LVM's device-mapper name mapping undone: the VG and LV hyphens arrive
/// doubled, so `rom-rom2--stage--boot` is the volume `rom2-stage-boot`.
fn lv_name(mapper: &str) -> Option<String> {
    Some(mapper.strip_prefix(ROM_PREFIX)?.replace("--", "-"))
}

/// The single `slaves/` entry of the pool's data sub-volume: the physical
/// `userdata` PV. Any other count is an error, because a missing PV makes the
/// whole tree unusable and a second one would make `pv/a` a lie.
fn pool_slave(root: &Path, blocks: &[Block]) -> Result<u64> {
    let pool = blocks
        .iter()
        .find(|block| block.mapper.as_deref() == Some(POOL_TDATA))
        .with_context(|| format!("no {POOL_TDATA} device-mapper device"))?;
    let directory = root.join(&pool.name).join("slaves");
    let mut slaves = fs::read_dir(&directory)
        .with_context(|| format!("read {}", directory.display()))?
        .map(|entry| {
            entry?
                .file_name()
                .into_string()
                .map_err(|_| std::io::Error::other("non-UTF8 slave name"))
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    slaves.sort();
    let [slave] = slaves.as_slice() else {
        bail!("{POOL_TDATA} has {} slaves, not one", slaves.len());
    };
    let device = blocks
        .iter()
        .find(|block| &block.name == slave)
        .with_context(|| format!("{POOL_TDATA} slave {slave} is not a block device"))?;
    ensure!(
        field(&device.uevent, "DEVTYPE") == Some("partition"),
        "{POOL_TDATA} slave {slave} is not a partition"
    );
    Ok(device.device)
}

/// device-mapper's dynamic minor from `/proc/misc`, one `<minor> <name>` pair
/// per line.
fn mapper_minor(misc: &Path) -> Result<u32> {
    let text = fs::read_to_string(misc).with_context(|| format!("read {}", misc.display()))?;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(minor), Some(name)) = (fields.next(), fields.next()) else {
            continue;
        };
        if name == DEVICE_MAPPER {
            return minor
                .parse()
                .with_context(|| format!("device-mapper minor {minor:?}"));
        }
    }
    bail!("{DEVICE_MAPPER} is not in {}", misc.display())
}

/// One `KEY=VALUE` line of a sysfs `uevent` file.
fn field<'a>(uevent: &'a str, key: &str) -> Option<&'a str> {
    uevent
        .lines()
        .filter_map(|line| line.split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value)
}

/// One sysfs `dev` value, `major:minor`, as a device number.
fn device_number(value: &str) -> Result<u64> {
    let (major, minor) = value
        .split_once(':')
        .with_context(|| format!("malformed sysfs device number {value:?}"))?;
    let major: u32 = major
        .trim()
        .parse()
        .with_context(|| format!("sysfs major {major:?}"))?;
    let minor: u32 = minor
        .trim()
        .parse()
        .with_context(|| format!("sysfs minor {minor:?}"))?;
    Ok(libc::makedev(major, minor))
}

/// Path of an absolute tree member below [`ESD_ROOT`].
fn relative(absolute: &str) -> Result<String> {
    absolute
        .strip_prefix(ESD_ROOT)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(str::to_owned)
        .with_context(|| format!("{absolute} is not below {ESD_ROOT}"))
}

/// Create every directory of the tree, remove the nodes the plan dropped, and
/// publish the planned nodes.
fn publish(root: &Path, plan: &[Node]) -> Result<()> {
    make_directory(root)?;
    let mut directories = BTreeSet::new();
    for node in plan {
        let (parent, _) = node
            .relative
            .rsplit_once('/')
            .with_context(|| format!("{} has no directory", node.relative))?;
        directories.insert(parent.to_owned());
    }
    for extra in [ESD_LOCK, ESD_RUN, ESD_ETC] {
        directories.insert(relative(extra)?);
    }
    for directory in &directories {
        make_directory(&root.join(directory))?;
    }
    for directory in &directories {
        let prefix = format!("{directory}/");
        let keep: BTreeSet<String> = plan
            .iter()
            .filter_map(|node| node.relative.strip_prefix(&prefix).map(str::to_owned))
            .collect();
        reconcile(&root.join(directory), &keep)?;
    }
    for node in plan {
        publish_node(root, node)?;
    }
    log::info!(
        "esd: published {} nodes below {}",
        plan.len(),
        root.display()
    );
    Ok(())
}

/// Create one directory with the tree's mode and label, adopting an existing
/// one. A symlink or any other non-directory already at the path fails closed.
fn make_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(metadata.is_dir(), "{} is not a directory", path.display()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir(path).with_context(|| format!("create {}", path.display()))?;
        }
        Err(error) => return Err(error.into()),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(DIRECTORY_MODE))
        .with_context(|| format!("set the mode of {}", path.display()))?;
    overlay::label(path, ESD_CONTEXT).with_context(|| format!("label {}", path.display()))?;
    Ok(())
}

/// Remove every entry a fully managed directory must no longer contain. Only
/// files are removed: nothing here creates a subdirectory, so one is reported
/// and kept instead of being followed.
fn reconcile(directory: &Path, keep: &BTreeSet<String>) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("read {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-UTF8 name in {}", directory.display()))?;
        if keep.contains(&name) {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            log::warn!("esd: {} is a directory; not removed", path.display());
            continue;
        }
        fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

/// Create or adopt one node, requiring the exact kind and device on the path
/// afterwards so a leftover node of another device never stands in.
fn publish_node(root: &Path, node: &Node) -> Result<()> {
    let path = root.join(&node.relative);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if Kind::of(metadata.file_type()) == Some(node.kind) && metadata.rdev() == node.device {
                overlay::label(&path, ESD_CONTEXT)
                    .with_context(|| format!("label {}", path.display()))?;
                return Ok(());
            }
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let name = CString::new(path.as_os_str().as_encoded_bytes())?;
    // SAFETY: the path is NUL-free and lives for the call; the mode carries no
    // setuid or setgid bit and the device number comes from sysfs.
    let result = unsafe {
        libc::mknod(
            name.as_ptr(),
            node.kind.file_type() | NODE_MODE,
            node.device,
        )
    };
    ensure!(
        result == 0,
        "cannot create {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        Kind::of(metadata.file_type()) == Some(node.kind) && metadata.rdev() == node.device,
        "unusable node {}",
        path.display()
    );
    overlay::label(&path, ESD_CONTEXT).with_context(|| format!("label {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake sysfs tree, so the planner is exercised without a phone.
    struct Fake {
        root: tempfile::TempDir,
    }

    impl Fake {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            fs::create_dir_all(root.path().join("class/block")).unwrap();
            Self { root }
        }

        fn block(&self, name: &str, uevent: &str, dev: &str, mapper: Option<&str>) {
            let directory = self.root.path().join("class/block").join(name);
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("uevent"), uevent).unwrap();
            fs::write(directory.join("dev"), format!("{dev}\n")).unwrap();
            if let Some(mapper) = mapper {
                fs::create_dir_all(directory.join("dm")).unwrap();
                fs::write(directory.join("dm/name"), format!("{mapper}\n")).unwrap();
            }
        }

        fn slave(&self, pool: &str, name: &str) {
            fs::create_dir_all(
                self.root
                    .path()
                    .join("class/block")
                    .join(pool)
                    .join("slaves")
                    .join(name),
            )
            .unwrap();
        }

        fn misc(&self, text: &str) {
            fs::write(self.root.path().join("misc"), text).unwrap();
        }

        fn sysfs(&self) -> Sysfs {
            Sysfs {
                block: self.root.path().join("class/block"),
                misc: self.root.path().join("misc"),
            }
        }
    }

    fn node(relative: &str, kind: Kind, device: u64) -> Node {
        Node {
            relative: relative.to_owned(),
            kind,
            device,
        }
    }

    fn pool(fake: &Fake) {
        fake.block(
            "dm-0",
            "DEVTYPE=disk\nDEVNAME=dm-0\n",
            "253:0",
            Some(POOL_TDATA),
        );
        fake.slave("dm-0", "sda1");
    }

    #[test]
    fn the_plan_publishes_every_name_by_its_device() {
        let fake = Fake::new();
        fake.block("sda", "DEVTYPE=disk\nDEVNAME=sda\n", "8:0", None);
        fake.block(
            "sda1",
            "DEVTYPE=partition\nDEVNAME=sda1\nPARTNAME=userdata\n",
            "8:1",
            None,
        );
        fake.block(
            "sda16",
            "DEVTYPE=partition\nDEVNAME=sda16\nPARTNAME=bdsvars\n",
            "8:16",
            None,
        );
        pool(&fake);
        fake.block(
            "dm-3",
            "DEVTYPE=disk\nDEVNAME=dm-3\n",
            "253:3",
            Some("rom-rom2--stage--boot"),
        );
        fake.block("dm-4", "DEVTYPE=disk\n", "253:4", Some("rom-pool"));
        fake.block("dm-5", "DEVTYPE=disk\n", "253:5", Some("other-rom-pool"));
        fake.misc(" 10 device-mapper\n 11 kmsg\n");

        let plan = plan(&fake.sysfs()).unwrap();
        assert_eq!(
            plan,
            vec![
                node("by-name/bdsvars", Kind::Block, libc::makedev(8, 16)),
                node("by-name/userdata", Kind::Block, libc::makedev(8, 1)),
                node("lv/pool", Kind::Block, libc::makedev(253, 4)),
                node("lv/pool_tdata", Kind::Block, libc::makedev(253, 0)),
                node("lv/rom2-stage-boot", Kind::Block, libc::makedev(253, 3)),
                node("mapper/control", Kind::Char, libc::makedev(10, 10)),
                node("pv/a", Kind::Block, libc::makedev(8, 1)),
            ]
        );
    }

    #[test]
    fn a_partname_on_two_devices_is_published_for_neither() {
        let fake = Fake::new();
        fake.block(
            "sda1",
            "DEVTYPE=partition\nDEVNAME=sda1\nPARTNAME=userdata\n",
            "8:1",
            None,
        );
        for (name, dev) in [("sda15", "8:15"), ("sdb15", "8:31"), ("sdc15", "8:47")] {
            let partname = if name == "sdc15" {
                "unique"
            } else {
                "metadata"
            };
            fake.block(
                name,
                &format!("DEVTYPE=partition\nPARTNAME={partname}\n"),
                dev,
                None,
            );
        }
        pool(&fake);
        fake.misc(" 10 device-mapper\n");

        let plan = plan(&fake.sysfs()).unwrap();
        assert!(!plan.iter().any(|node| node.relative == "by-name/metadata"));
        assert!(plan.iter().any(|node| node.relative == "by-name/unique"));
    }

    #[test]
    fn lvm_doubling_is_undone_for_rom_volumes_only() {
        assert_eq!(
            lv_name("rom-rom2--stage--boot").as_deref(),
            Some("rom2-stage-boot")
        );
        assert_eq!(lv_name("rom-rom1").as_deref(), Some("rom1"));
        assert_eq!(lv_name("rom-rom1--fw--x_a").as_deref(), Some("rom1-fw-x_a"));
        assert_eq!(lv_name("userdata"), None);
        assert_eq!(lv_name("dm-3"), None);
    }

    #[test]
    fn the_pool_slave_must_be_exactly_one_partition() {
        let fake = Fake::new();
        fake.block(
            "sda1",
            "DEVTYPE=partition\nPARTNAME=userdata\n",
            "8:1",
            None,
        );
        fake.block("sdb1", "DEVTYPE=partition\nPARTNAME=other\n", "8:17", None);
        fake.block("sdc", "DEVTYPE=disk\n", "8:32", None);
        fake.block("dm-0", "DEVTYPE=disk\n", "253:0", Some(POOL_TDATA));
        assert!(plan(&fake.sysfs()).is_err(), "no slave");
        fake.slave("dm-0", "sda1");
        fake.slave("dm-0", "sdb1");
        assert!(plan(&fake.sysfs()).is_err(), "two slaves");
        fs::remove_dir(fake.root.path().join("class/block/dm-0/slaves/sdb1")).unwrap();
        fake.slave("dm-0", "missing");
        assert!(plan(&fake.sysfs()).is_err(), "an unknown slave name");
        fs::remove_dir(fake.root.path().join("class/block/dm-0/slaves/missing")).unwrap();
        fs::remove_dir(fake.root.path().join("class/block/dm-0/slaves/sda1")).unwrap();
        fake.slave("dm-0", "sdc");
        assert!(plan(&fake.sysfs()).is_err(), "the slave is a whole disk");
    }

    #[test]
    fn the_mapper_minor_comes_from_proc_misc() {
        let fake = Fake::new();
        fake.misc("  1 ram0\n 10 device-mapper\n 11 kmsg\n");
        assert_eq!(mapper_minor(&fake.sysfs().misc).unwrap(), 10);
        fake.misc(" 11 kmsg\n");
        assert!(mapper_minor(&fake.sysfs().misc).is_err());
        fake.misc(" ten device-mapper\n");
        assert!(mapper_minor(&fake.sysfs().misc).is_err());
    }

    #[test]
    fn reconcile_removes_only_files_the_plan_dropped() {
        let fake = Fake::new();
        let directory = fake.root.path().join("lv");
        fs::create_dir(&directory).unwrap();
        for name in ["kept", "gone"] {
            fs::write(directory.join(name), b"").unwrap();
        }
        fs::create_dir(directory.join("subdirectory")).unwrap();
        let keep = BTreeSet::from(["kept".to_owned()]);

        reconcile(&directory, &keep).unwrap();

        assert!(directory.join("kept").is_file());
        assert!(!directory.join("gone").exists());
        assert!(directory.join("subdirectory").is_dir());
    }

    #[test]
    fn every_tree_path_is_below_its_root() {
        assert_eq!(relative(ESD_PV).unwrap(), "pv/a");
        assert_eq!(relative(&esd_by_name("boot_a")).unwrap(), "by-name/boot_a");
        assert_eq!(
            relative(&esd_lv("rom2-stage-boot")).unwrap(),
            "lv/rom2-stage-boot"
        );
        assert_eq!(relative(&esd_mapper(CONTROL)).unwrap(), "mapper/control");
        assert!(relative("/dev/block/other").is_err());
    }
}
