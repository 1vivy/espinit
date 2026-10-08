//! Installed-boot admission: the selected ROM's kernel image set.
//!
//! A ROM 1 boot reads the physical firmware partitions of the current slot; a
//! ROM `>= 2` boot reads its own seven AVB kernel images from the ESP for the
//! slot decided by the executor.

use crate::{
    Backend, Error, KERNEL_SET_BASES, Manifest, RomConfig, parse_manifest, parse_selected_rom,
    validate_bootstrap, validate_managed, validate_rom,
};

/// Partition-name suffix of a kernel image slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// Slot A, suffix `_a`.
    A,
    /// Slot B, suffix `_b`.
    B,
}

impl Slot {
    /// Partition-name suffix of this slot.
    pub fn suffix(self) -> &'static str {
        match self {
            Slot::A => "_a",
            Slot::B => "_b",
        }
    }

    /// AOSP slot index of this slot: A is 0, B is 1.
    pub fn index(self) -> u8 {
        match self {
            Slot::A => 0,
            Slot::B => 1,
        }
    }

    /// Slot of an AOSP slot index, `None` for any other value.
    pub fn from_index(index: u8) -> Option<Self> {
        match index {
            0 => Some(Slot::A),
            1 => Some(Slot::B),
            _ => None,
        }
    }
}

/// One AVB kernel image of an admitted ESP kernel set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelImage<'a> {
    /// AVB base name, in [`KERNEL_SET_BASES`] order.
    pub base: &'static str,
    /// Partition name this image was admitted under: `<base>_a` or `<base>_b`.
    pub partition: &'a str,
    /// ESP-root-relative path of the image file: the text after `esp-file:`,
    /// already validated as a [`crate::relative_path`].
    pub path: &'a str,
}

/// Kernel images of the selected ROM for one slot.
///
/// The seven images are always kept inline: an admitted boot hands every image
/// to the executor without allocating, and the array order is the contract.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelImages<'a> {
    /// ROM 1: the physical kernel partitions of the current slot are used.
    Physical,
    /// ROM `>= 2`: the seven ESP kernel image files of the requested slot, in
    /// [`KERNEL_SET_BASES`] order.
    Esp([KernelImage<'a>; 7]),
}

/// An admitted installed configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledConfig {
    manifest: Manifest,
    rom: RomConfig,
    rom_number: u32,
}

impl InstalledConfig {
    /// The installed ESP manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The selected ROM configuration.
    pub fn rom(&self) -> &RomConfig {
        &self.rom
    }

    /// The bdsvars ROM number of the selected ROM, 1..=5.
    pub fn rom_number(&self) -> u32 {
        self.rom_number
    }

    /// Kernel images of `slot`.
    ///
    /// Only [`parse_installed`] builds an `InstalledConfig`, and it admits
    /// either the physical set (ROM 1) or all fourteen kernel partitions as
    /// distinct ESP files (ROM `>= 2`), so every partition looked up here
    /// exists with an `esp-file:` backend.
    pub fn kernel_images(&self, slot: Slot) -> KernelImages<'_> {
        if self.rom_number < 2 {
            return KernelImages::Physical;
        }

        KernelImages::Esp(std::array::from_fn(|index| {
            let base = KERNEL_SET_BASES[index];
            let suffix = slot.suffix();
            let partition = self
                .rom
                .partitions
                .iter()
                .find(|partition| {
                    partition
                        .name
                        .strip_prefix(base)
                        .is_some_and(|rest| rest == suffix)
                })
                .expect("kernel-set admission guarantees every kernel partition");
            let Ok(Backend::EspFile(path)) = partition.backend() else {
                unreachable!("kernel-set admission guarantees esp-file kernel backends");
            };

            KernelImage {
                base,
                partition: &partition.name,
                path,
            }
        }))
    }
}

/// Installed-boot admission: parse and validate the installed manifest, admit
/// its cpio bootstrap modules, then validate the selected ROM and admit its kernel
/// image set.
///
/// The manifest must agree with the `kernelesp`/`efivarfs`/`efivar_store` modules
/// the loader already inserted ([`validate_bootstrap`]), because firmware refuses
/// exactly what esu PID 1 refuses at boot.
///
/// ROM 1 must not shadow a kernel base: no `<base>_a`/`<base>_b` partition may
/// use an `esp-file:` backend (`KernelSetBackend`), and the boot reads the
/// physical partitions. ROM `>= 2` must carry all fourteen `<base>_a`/`<base>_b`
/// partitions (`KernelSetIncomplete`), each with an `esp-file:` backend
/// (`KernelSetBackend`) and fourteen distinct paths (`KernelSetDuplicatePath`).
pub fn parse_installed(
    manifest_text: &str,
    rom_text: &str,
    selected_id: &str,
    rom_number: u32,
) -> Result<InstalledConfig, Error> {
    let manifest = parse_manifest(manifest_text)?;
    validate_bootstrap(&manifest)?;
    let rom = parse_selected_rom(rom_text, selected_id)?;
    validate_rom(&rom, rom_number)?;
    validate_managed(&manifest, &rom)?;
    admit_kernel_set(&rom, rom_number)?;

    Ok(InstalledConfig {
        manifest,
        rom,
        rom_number,
    })
}

/// Admit the kernel image set of an already validated ROM.
fn admit_kernel_set(rom: &RomConfig, rom_number: u32) -> Result<(), Error> {
    if rom_number < 2 {
        for partition in &rom.partitions {
            if is_kernel_partition(&partition.name)
                && matches!(partition.backend(), Ok(Backend::EspFile(_)))
            {
                return Err(Error::at("KernelSetBackend", &partition.name));
            }
        }

        return Ok(());
    }

    let mut paths: Vec<&str> = Vec::with_capacity(KERNEL_SET_BASES.len() * 2);

    for base in KERNEL_SET_BASES {
        for slot in [Slot::A, Slot::B] {
            let name = format!("{base}{}", slot.suffix());
            let partition = rom
                .partitions
                .iter()
                .find(|partition| partition.name == name)
                .ok_or_else(|| Error::at("KernelSetIncomplete", name.clone()))?;
            let Ok(Backend::EspFile(path)) = partition.backend() else {
                return Err(Error::at("KernelSetBackend", &partition.name));
            };

            if paths
                .iter()
                .any(|admitted| admitted.eq_ignore_ascii_case(path))
            {
                return Err(Error::at("KernelSetDuplicatePath", &partition.name));
            }
            paths.push(path);
        }
    }

    Ok(())
}

/// Whether `name` is one of the kernel-set bases with a slot suffix.
fn is_kernel_partition(name: &str) -> bool {
    KERNEL_SET_BASES.iter().any(|base| {
        name.strip_prefix(base)
            .is_some_and(|rest| rest == Slot::A.suffix() || rest == Slot::B.suffix())
    })
}
