//! Installed-boot admission: the selected ROM's base image set.
//!
//! A ROM 1 boot reads the physical firmware partitions of the booted slot; a
//! ROM `>= 2` boot reads its own base image files from the ESP, or the staging
//! logical volumes of a staged letter, as the executor decides.

use crate::{
    Backend, Error, IMAGE_BASES, Manifest, RomConfig, base_image_path, parse_manifest,
    parse_selected_rom, validate_bootstrap, validate_managed, validate_rom,
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

/// One base image of an admitted ROM image set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelImage {
    /// Base name, in [`IMAGE_BASES`] order.
    pub base: &'static str,
    /// ESP-root-relative path of the base's one image file, already validated
    /// as a [`crate::relative_path`].
    pub path: String,
}

/// Base images of the selected ROM.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KernelImages {
    /// ROM 1: the physical kernel partitions of the booted slot are used.
    Physical,
    /// ROM `>= 2`: the declared bases' ESP image files, in [`IMAGE_BASES`]
    /// order. Both letters of a base are served by the same file.
    Esp(Vec<KernelImage>),
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

    /// The base images of the selected ROM, one entry per declared base.
    ///
    /// Only [`parse_installed`] builds an `InstalledConfig`, and it admits
    /// either the physical set (ROM 1) or every declared base as both letters of
    /// an `rom-image:` projection, so the declared bases here are exactly the
    /// admitted ones and their paths follow
    /// [`base_image_path`](crate::base_image_path).
    pub fn kernel_images(&self) -> KernelImages {
        if self.rom_number < 2 {
            return KernelImages::Physical;
        }

        KernelImages::Esp(
            IMAGE_BASES
                .into_iter()
                .filter(|base| {
                    self.rom
                        .partitions
                        .iter()
                        .any(|partition| declares(base, &partition.name))
                })
                .map(|base| KernelImage {
                    base,
                    path: base_image_path(&self.rom.id, base),
                })
                .collect(),
        )
    }
}

/// Installed-boot admission: parse and validate the installed manifest, admit
/// its cpio bootstrap modules, then validate the selected ROM and admit its
/// base image set.
///
/// The manifest must agree with the `kernelesp`/`efivarfs`/`efivar_store` modules
/// the loader already inserted ([`validate_bootstrap`]), because firmware refuses
/// exactly what esu PID 1 refuses at boot.
///
/// ROM 1 must not shadow a physical partition with an image role: no partition
/// may use an `esp-file:` or `rom-image:` backend (`KernelSetBackend`), and the
/// boot reads the physical partitions. ROM `>= 2` declares at least one base
/// (`KernelSetEmpty`); every partition named `<base>_a`/`<base>_b` of a declared
/// base must carry the `rom-image:<base>` backend (`KernelSetBackend`), be
/// writable (`KernelSetReadOnly`) and have both letters present
/// (`KernelSetIncomplete`).
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
    admit_image_set(&rom, rom_number)?;

    Ok(InstalledConfig {
        manifest,
        rom,
        rom_number,
    })
}

/// Admit the base image set of an already validated ROM.
fn admit_image_set(rom: &RomConfig, rom_number: u32) -> Result<(), Error> {
    if rom_number < 2 {
        for partition in &rom.partitions {
            let backend = partition.backend();
            let image_role = matches!(backend, Ok(Backend::RomImage(_)))
                || (is_image_partition(&partition.name)
                    && matches!(backend, Ok(Backend::EspFile(_))));
            if image_role {
                return Err(Error::at("KernelSetBackend", &partition.name));
            }
        }

        return Ok(());
    }

    let mut bases: Vec<&'static str> = Vec::new();

    for base in IMAGE_BASES {
        let present: Vec<&crate::schema::PartitionEntry> = rom
            .partitions
            .iter()
            .filter(|partition| declares(base, &partition.name))
            .collect();
        if present.is_empty() {
            continue;
        }

        for partition in &present {
            if !matches!(partition.backend(), Ok(Backend::RomImage(declared)) if declared == base) {
                return Err(Error::at("KernelSetBackend", &partition.name));
            }
        }

        for partition in &present {
            if partition.read_only {
                return Err(Error::at("KernelSetReadOnly", &partition.name));
            }
        }

        for slot in [Slot::A, Slot::B] {
            let name = format!("{base}{}", slot.suffix());
            if !present.iter().any(|partition| partition.name == name) {
                return Err(Error::at("KernelSetIncomplete", name));
            }
        }

        bases.push(base);
    }

    if bases.is_empty() {
        return Err(Error::new("KernelSetEmpty"));
    }

    Ok(())
}

/// Whether `name` is one of `base`'s two slot partitions.
fn declares(base: &str, name: &str) -> bool {
    name.strip_prefix(base)
        .is_some_and(|rest| rest == Slot::A.suffix() || rest == Slot::B.suffix())
}

/// Whether `name` is one of the image bases with a slot suffix.
fn is_image_partition(name: &str) -> bool {
    IMAGE_BASES.iter().any(|base| declares(base, name))
}
