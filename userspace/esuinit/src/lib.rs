pub mod block;
pub mod config;
pub mod esp;
pub mod gpt;
pub mod gpt_uapi;
pub mod gptctl;
pub mod handoff;
pub mod init;
pub mod loader;
pub mod platform;
pub mod probe;
pub mod receipt;
pub mod scripts;
pub mod selfcheck;

use anyhow::{Context, Result, bail};
use goblin::elf::{Elf, section_header, sym::Sym};
use rustix::system::init_module;
use scroll::{Pwrite, ctx::SizeWith};
use std::collections::HashMap;
use std::ffi::CStr;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;

struct Kptr {
    value: String,
}

impl Kptr {
    pub fn new() -> Result<Self> {
        let value = fs::read_to_string("/proc/sys/kernel/kptr_restrict")?;
        fs::write("/proc/sys/kernel/kptr_restrict", "1")?;
        Ok(Kptr { value })
    }
}

impl Drop for Kptr {
    fn drop(&mut self) {
        let _ = fs::write("/proc/sys/kernel/kptr_restrict", self.value.as_bytes());
    }
}

pub struct KptrOwnedIter<I> {
    _kptr: Kptr,
    iter: I,
}

impl<I: Iterator> Iterator for KptrOwnedIter<I> {
    type Item = I::Item;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next()
    }
}

pub fn kernel_symbols_iter() -> Result<impl Iterator<Item = (String, u64)>> {
    let kptr = Kptr::new()?;

    let iter = BufReader::new(File::open("/proc/kallsyms")?)
        .lines()
        // https://github.com/torvalds/linux/blob/7f87a5ea75f011d2c9bc8ac0167e5e2d1adb1594/kernel/kallsyms.c#L727
        // We can stop read as soon as we read all kernel symbols
        .map_while(|line| {
            line.ok().and_then(|line| {
                let mut splits = line.split_whitespace();
                splits
                    .next()
                    .and_then(|addr| u64::from_str_radix(addr, 16).ok())
                    .and_then(|addr| {
                        splits
                            .nth(1)
                            .take_if(|_| splits.next().is_none()) // stop at module symbols
                            .map(|symbol| {
                                (
                                    symbol
                                        .find("$")
                                        .or_else(|| symbol.find(".llvm."))
                                        .map(|pos| &symbol[0..pos])
                                        .unwrap_or(symbol)
                                        .to_owned(),
                                    addr,
                                )
                            })
                    })
            })
        });

    Ok(KptrOwnedIter { _kptr: kptr, iter })
}

pub fn for_each_kernel_symbols<F: FnMut(&(String, u64)) -> Result<bool>>(mut f: F) -> Result<()> {
    for item in kernel_symbols_iter()? {
        if !f(&item)? {
            break;
        }
    }
    Ok(())
}

const O_NONBLOCK: i32 = 0x800;

fn open_kmsg_at_end() -> Result<File> {
    let mut last_error = None;

    for path in ["/dev/kmsg", "/kmsg"] {
        match OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK)
            .open(path)
        {
            Ok(mut file) => {
                file.seek(SeekFrom::End(0))
                    .with_context(|| format!("Cannot seek {path} to end"))?;

                log::info!("Reading kernel log from {path}");
                return Ok(file);
            }
            Err(error) => {
                last_error = Some((path, error));
            }
        }
    }

    match last_error {
        Some((path, error)) => {
            Err(error).with_context(|| format!("Cannot open kernel log device, last tried {path}"))
        }
        None => bail!("No kernel log device candidate"),
    }
}

fn read_new_kmsg(file: &mut File) -> Result<String> {
    let mut output = Vec::new();
    let mut record = [0u8; 8192];

    loop {
        match file.read(&mut record) {
            Ok(0) => break,
            Ok(length) => {
                output.extend_from_slice(&record[..length]);
                output.push(b'\n');
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) => return Err(error).context("Cannot read /dev/kmsg"),
        }
    }

    Ok(String::from_utf8_lossy(&output).into_owned())
}

fn extract_required_vermagic(kmsg: &str) -> Option<String> {
    const PREFIX: &str = "version magic '";
    const SEPARATOR: &str = "' should be '";

    for record in kmsg.lines().rev() {
        let message = record
            .split_once(';')
            .map(|(_, message)| message)
            .unwrap_or(record);

        let Some(prefix_position) = message.find(PREFIX) else {
            continue;
        };
        let after_prefix = &message[prefix_position + PREFIX.len()..];

        let Some(separator_position) = after_prefix.find(SEPARATOR) else {
            continue;
        };
        let required = &after_prefix[separator_position + SEPARATOR.len()..];

        let Some(end_quote) = required.find('\'') else {
            continue;
        };
        let required = &required[..end_quote];

        if !required.is_empty() {
            return Some(required.to_owned());
        }
    }

    None
}

fn align_up(value: usize, alignment: usize) -> Result<usize> {
    let alignment = alignment.max(1);

    if !alignment.is_power_of_two() {
        bail!("Invalid ELF alignment: {alignment}");
    }

    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
        .context("ELF alignment overflow")
}

fn write_elf64_word(
    buffer: &mut [u8],
    offset: usize,
    value: u64,
    little_endian: bool,
) -> Result<()> {
    let end = offset.checked_add(8).context("ELF write overflow")?;
    let destination = buffer
        .get_mut(offset..end)
        .context("ELF write outside module buffer")?;

    let bytes = if little_endian {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    destination.copy_from_slice(&bytes);
    Ok(())
}

fn replace_module_vermagic(buffer: &mut Vec<u8>, required_vermagic: &str) -> Result<()> {
    struct ModinfoLocation {
        offset: usize,
        size: usize,
        section_header_offset: usize,
        alignment: usize,
        little_endian: bool,
    }

    let location = {
        let elf = Elf::parse(buffer)?;

        if !elf.is_64 {
            bail!("Only ELF64 modules are supported");
        }

        let section_table_offset =
            usize::try_from(elf.header.e_shoff).context("Section table offset overflow")?;
        let section_entry_size = usize::from(elf.header.e_shentsize);
        let mut location = None;

        for (index, section) in elf.section_headers.iter().enumerate() {
            let Some(name) = elf.shdr_strtab.get_at(section.sh_name) else {
                continue;
            };
            if name != ".modinfo" {
                continue;
            }

            let offset = usize::try_from(section.sh_offset).context(".modinfo offset overflow")?;
            let size = usize::try_from(section.sh_size).context(".modinfo size overflow")?;
            let end = offset
                .checked_add(size)
                .context(".modinfo range overflow")?;

            if end > buffer.len() {
                bail!(".modinfo is outside module buffer");
            }

            let section_header_offset = section_table_offset
                .checked_add(
                    index
                        .checked_mul(section_entry_size)
                        .context("Section index overflow")?,
                )
                .context("Section header offset overflow")?;

            location = Some(ModinfoLocation {
                offset,
                size,
                section_header_offset,
                alignment: usize::try_from(section.sh_addralign).unwrap_or(1).max(1),
                little_endian: elf.little_endian,
            });
            break;
        }

        location.context("Module has no .modinfo section")?
    };

    let old_modinfo = &buffer[location.offset..location.offset + location.size];
    let replacement = format!("vermagic={required_vermagic}");
    let mut new_modinfo = Vec::with_capacity(old_modinfo.len().max(replacement.len() + 1));
    let mut replaced = false;

    for entry in old_modinfo.split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }

        if entry.starts_with(b"vermagic=") {
            if !replaced {
                new_modinfo.extend_from_slice(replacement.as_bytes());
                new_modinfo.push(0);
                replaced = true;
            }
        } else {
            new_modinfo.extend_from_slice(entry);
            new_modinfo.push(0);
        }
    }

    if !replaced {
        new_modinfo.extend_from_slice(replacement.as_bytes());
        new_modinfo.push(0);
    }

    let new_offset = align_up(buffer.len(), location.alignment)?;
    buffer.resize(new_offset, 0);
    buffer.extend_from_slice(&new_modinfo);

    // Elf64_Shdr: sh_offset at +0x18, sh_size at +0x20.
    write_elf64_word(
        buffer,
        location.section_header_offset + 0x18,
        new_offset as u64,
        location.little_endian,
    )?;
    write_elf64_word(
        buffer,
        location.section_header_offset + 0x20,
        new_modinfo.len() as u64,
        location.little_endian,
    )?;

    log::warn!(
        "Replaced module vermagic with kernel-required value: {:?}",
        required_vermagic
    );
    Ok(())
}

/// Relocate undefined symbols in an ELF kernel module buffer using /proc/kallsyms,
/// then load it via init_module syscall.
pub fn load_module(data: &[u8], params: &CStr) -> Result<()> {
    let mut buffer = data.to_vec();
    let elf = Elf::parse(&buffer)?;
    let ctx = *elf.syms.ctx();

    let mut unresolved_symbols: HashMap<String, (Sym, usize)> = HashMap::new();
    for (index, sym) in elf.syms.iter().enumerate() {
        if index == 0 {
            continue;
        }

        if sym.st_shndx != section_header::SHN_UNDEF as usize {
            continue;
        }

        let Some(name) = elf.strtab.get_at(sym.st_name) else {
            continue;
        };

        let offset = elf.syms.offset() + index * Sym::size_with(elf.syms.ctx());
        unresolved_symbols.insert(name.to_owned(), (sym, offset));
    }

    if !unresolved_symbols.is_empty() {
        for_each_kernel_symbols(|(symbol, addr)| {
            if let Some((mut sym, offset)) = unresolved_symbols.remove(symbol) {
                sym.st_shndx = section_header::SHN_ABS as usize;
                sym.st_value = *addr;
                buffer.pwrite_with(sym, offset, ctx)?;
            }

            Ok(!unresolved_symbols.is_empty())
        })
        .context("Cannot parse kallsyms")?;
    }

    for name in unresolved_symbols.keys() {
        log::warn!("Cannot find symbol: {}", name);
    }

    let mut kmsg = match open_kmsg_at_end() {
        Ok(file) => Some(file),
        Err(error) => {
            log::warn!("Cannot prepare kmsg fallback: {error:#}");
            None
        }
    };

    match init_module(&buffer, params) {
        Ok(()) => Ok(()),
        Err(first_error) => {
            let logs = match kmsg.as_mut() {
                Some(file) => read_new_kmsg(file).unwrap_or_default(),
                None => String::new(),
            };

            let Some(required_vermagic) = extract_required_vermagic(&logs) else {
                log::error!("Kernel module loading log:\n{}", logs);
                return Err(first_error).context("init_module failed without vermagic mismatch");
            };

            log::warn!(
                "Kernel requires vermagic {:?}; replacing and retrying",
                required_vermagic
            );

            replace_module_vermagic(&mut buffer, &required_vermagic)
                .context("Cannot replace module vermagic")?;

            init_module(&buffer, params).context("init_module failed after replacing vermagic")?;
            Ok(())
        }
    }
}

/// UAPI version implemented by this loader. A core module reporting any other
/// value is not ABI-compatible and can never satisfy the payload self-check.
pub const UAPI_VERSION: u32 = 3;

/// `ESU_STATE_READY`: the core module finished its normal initialization.
pub const STATE_READY: u32 = 1 << 0;

const INSTALL_MAGIC1: u32 = 0x45535049; // 'ESPI'
const INSTALL_MAGIC2: u32 = 0x4e495446; // 'NITF'

/// Exact mirror of `struct ksu_get_info_cmd` from `uapi/supercall.h`. Field
/// order, types and size are a hard ABI contract: the ioctl request number is
/// derived from `size_of::<GetInfoCmd>()`, exactly as the C `_IOR` macro
/// derives its size field, so this type cannot silently drift from the header.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GetInfoCmd {
    pub version: u32,
    pub flags: u32,
    pub features: u32,
    pub uapi_version: u32,
    pub state: u32,
    pub generation: [u8; 64],
    pub boot_mode: u32,
}

impl Default for GetInfoCmd {
    fn default() -> Self {
        Self {
            version: 0,
            flags: 0,
            features: 0,
            uapi_version: 0,
            state: 0,
            generation: [0; 64],
            boot_mode: 0,
        }
    }
}

const _: () = assert!(std::mem::size_of::<GetInfoCmd>() == 88);

const fn ioc_read(nr: u32, size: u32) -> u32 {
    (2u32 << 30) | ((size & 0x3fff) << 16) | ((b'E' as u32) << 8) | (nr & 0xff)
}

const fn ioc_write(nr: u32, size: u32) -> u32 {
    (1u32 << 30) | ((size & 0x3fff) << 16) | ((b'E' as u32) << 8) | (nr & 0xff)
}

/// `KSU_IOCTL_GET_INFO` for the v3 structure (`0x80584502`).
pub const IOCTL_GET_INFO: u32 = ioc_read(2, std::mem::size_of::<GetInfoCmd>() as u32);
pub const IOCTL_SET_BOOT_MODE: u32 = ioc_write(20, std::mem::size_of::<u32>() as u32);

const _: () = assert!(IOCTL_GET_INFO == 0x8058_4502);
const _: () = assert!(IOCTL_SET_BOOT_MODE == 0x4004_4514);

impl GetInfoCmd {
    /// The generation reported by the core, when the field is NUL-terminated
    /// valid ASCII. `generation[63]` is always NUL, so a missing terminator
    /// means the core is not speaking this ABI.
    pub fn generation(&self) -> Option<String> {
        let end = self.generation.iter().position(|byte| *byte == 0)?;
        let bytes = &self.generation[..end];

        if !bytes.is_ascii() {
            return None;
        }

        Some(bytes.iter().map(|byte| char::from(*byte)).collect())
    }

    /// Whether the core reported `ESU_STATE_READY`.
    pub fn ready(&self) -> bool {
        self.state & STATE_READY != 0
    }
}

/// Install the esu control fd into this process through the reboot hook.
fn install_driver_fd() -> Result<i32> {
    use syscalls::{Sysno, syscall};

    let mut fd: i32 = -1;
    unsafe {
        let _ = syscall!(
            Sysno::reboot,
            INSTALL_MAGIC1,
            INSTALL_MAGIC2,
            0,
            std::ptr::addr_of_mut!(fd)
        );
    }

    if fd < 0 {
        bail!("esu control fd is unavailable");
    }

    Ok(fd)
}

/// Query the core module identity through the v3 control interface. Fails when
/// the driver fd cannot be installed or the ioctl is rejected; the caller is
/// responsible for ABI-version, generation, readiness, and boot-mode checks.
pub fn query_core_info() -> Result<GetInfoCmd> {
    use syscalls::{Sysno, syscall};

    let fd = install_driver_fd()?;
    let mut cmd = GetInfoCmd::default();
    let result = unsafe {
        syscall!(
            Sysno::ioctl,
            fd,
            IOCTL_GET_INFO,
            std::ptr::addr_of_mut!(cmd)
        )
    };
    unsafe {
        let _ = syscall!(Sysno::close, fd);
    }

    result
        .map_err(|errno| anyhow::anyhow!("errno {}", errno.into_raw()))
        .context("esu v2 get-info ioctl failed")?;

    Ok(cmd)
}

/// Set the validated PID1 boot mode through the esu control interface.
pub fn set_core_boot_mode(mode: u32) -> Result<()> {
    use syscalls::{Sysno, syscall};

    let fd = install_driver_fd()?;
    let result = unsafe {
        syscall!(
            Sysno::ioctl,
            fd,
            IOCTL_SET_BOOT_MODE,
            std::ptr::addr_of!(mode)
        )
    };
    unsafe {
        let _ = syscall!(Sysno::close, fd);
    }

    result
        .map_err(|errno| anyhow::anyhow!("errno {}", errno.into_raw()))
        .context("esu set-boot-mode ioctl failed")?;
    Ok(())
}

/// Whether a core module speaking the current ABI is loaded and initialized.
pub fn has_esu() -> bool {
    match query_core_info() {
        Ok(cmd) => {
            log::info!(
                "esu uapi version: {}, state: {:#x}",
                cmd.uapi_version,
                cmd.state
            );
            cmd.uapi_version == UAPI_VERSION && cmd.ready()
        }
        Err(error) => {
            log::warn!("esu control interface unavailable: {error:#}");
            false
        }
    }
}

/// Whether a core module is loaded at all, regardless of readiness.
pub fn core_loaded() -> bool {
    query_core_info().is_ok()
}
