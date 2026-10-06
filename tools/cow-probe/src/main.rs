//! cow-probe: prove data-backed copy-on-write on a thin projected `/data`.
//!
//! Device-only diagnostic from the multi-ROM isolation plan (step 7). It is
//! never shipped in the ESP and never run by espinit: an operator runs it once
//! from a root shell against a booted managed ROM.
//!
//! It writes exactly one preallocated, pinned file and reads/writes exactly two
//! raw block devices: the `espinit-gpt*` projection behind `/data` and the
//! `sda15` LVM physical volume that projection must diverge from. Raw writes
//! never leave the extents of its own file.
//!
//! `cow-probe [DIR] [SIZE_MIB]`
//!
//! * `DIR` - probe directory, default `/data/gsi/espinit-cow-probe`.
//! * `SIZE_MIB` - probe file size in MiB, default 256.
//!
//! With one argument, a plain integer is `SIZE_MIB`, anything else is `DIR`.
//! Exit code 0 means every check passed; 1 names the failing check on stderr.

mod extents;
mod ioctl;
mod mountinfo;
mod pattern;
mod sysfs;

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::raw::c_void;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use extents::Extent;

const DEFAULT_DIR: &str = "/data/gsi/espinit-cow-probe";
const DEFAULT_SIZE_MIB: u64 = 256;
const MAX_SIZE_MIB: u64 = 16 * 1024;

/// The LVM physical volume the projected `/data` must diverge from: LU0
/// `sda15`, whose `dm-thin` holder keeps it writable. The name is fixed by the
/// phone layout; the probe refuses to substitute another device.
const PV_NAME: &str = "sda15";
const PV_DEV: &str = "/dev/block/sda15";

/// `_IOW(F2FS_IOCTL_MAGIC, 13, __u32)` from `linux/f2fs.h`.
const F2FS_IOC_SET_PIN_FILE: u64 = ioctl::ioc(ioctl::IOC_WRITE, 0xf5, 13, 4);

const NONCE_PLAINTEXT_1: u32 = 0x1a2b_3c4d;
const NONCE_PLAINTEXT_2: u32 = 0x5e6f_7a8b;
const NONCE_RAW: u32 = 0x9c0d_1e2f;

const USAGE: &str = "usage: cow-probe [DIR] [SIZE_MIB]";

fn main() -> ExitCode {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(reason) => {
            eprintln!("cow-probe: {reason}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let mut checks = Checks::default();
    let mut report = Report {
        size: args.size,
        ..Report::default()
    };
    if let Err(failure) = probe(&args, &mut checks, &mut report) {
        checks.failed(failure.check, failure.detail);
    }
    cleanup(&args, &mut checks);

    let pass = checks.all_pass();
    println!("{}", render(&report, &checks, pass));
    if pass {
        ExitCode::SUCCESS
    } else {
        for check in &checks.items {
            if !check.pass {
                eprintln!("cow-probe: check {} failed: {}", check.name, check.detail);
            }
        }
        ExitCode::from(1)
    }
}

struct Args {
    dir: PathBuf,
    size: u64,
}

impl Args {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let arguments: Vec<String> = arguments.collect();
        if arguments.len() > 2 {
            return Err(format!(
                "expected at most 2 arguments, got {}",
                arguments.len()
            ));
        }
        let (dir, size_mib) = if arguments.is_empty() {
            (PathBuf::from(DEFAULT_DIR), DEFAULT_SIZE_MIB)
        } else if let [only] = arguments.as_slice() {
            match only.parse::<u64>() {
                Ok(size_mib) => (PathBuf::from(DEFAULT_DIR), size_mib),
                Err(_) => (PathBuf::from(only), DEFAULT_SIZE_MIB),
            }
        } else {
            let [dir, size] = arguments.as_slice() else {
                return Err(format!(
                    "expected at most 2 arguments, got {}",
                    arguments.len()
                ));
            };
            (
                PathBuf::from(dir),
                size.parse::<u64>()
                    .map_err(|error| format!("invalid SIZE_MIB {size:?}: {error}"))?,
            )
        };
        if size_mib == 0 || size_mib > MAX_SIZE_MIB {
            return Err(format!(
                "SIZE_MIB must be between 1 and {MAX_SIZE_MIB}, got {size_mib}"
            ));
        }
        let size = size_mib
            .checked_mul(1024 * 1024)
            .ok_or_else(|| format!("SIZE_MIB {size_mib} overflows a byte count"))?;
        Ok(Args { dir, size })
    }
}

/// A named check failure; the name is the check reported in the JSON and on
/// stderr.
struct Failure {
    check: &'static str,
    detail: String,
}

type Res<T> = Result<T, Failure>;

fn failure(check: &'static str, detail: impl Into<String>) -> Failure {
    Failure {
        check,
        detail: detail.into(),
    }
}

#[derive(Default)]
struct Checks {
    items: Vec<Check>,
}

struct Check {
    name: &'static str,
    pass: bool,
    detail: String,
}

impl Checks {
    fn ok(&mut self, name: &'static str, detail: impl Into<String>) {
        self.items.push(Check {
            name,
            pass: true,
            detail: detail.into(),
        });
    }

    fn failed(&mut self, name: &'static str, detail: impl Into<String>) {
        self.items.push(Check {
            name,
            pass: false,
            detail: detail.into(),
        });
    }

    fn all_pass(&self) -> bool {
        self.items.iter().all(|check| check.pass)
    }
}

#[derive(Default)]
struct Report {
    size: u64,
    extents: Vec<Extent>,
    fs_bdev: Option<String>,
    slave: Option<String>,
    partname: Option<String>,
    pv_bdev: Option<String>,
    pv_partname: Option<String>,
}

fn probe(args: &Args, checks: &mut Checks, report: &mut Report) -> Res<()> {
    fs::create_dir_all(&args.dir)
        .map_err(|error| failure("mkdir", format!("{}: {error}", args.dir.display())))?;
    let image = args.dir.join("probe.img");
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&image)
        .map_err(|error| failure("create", format!("{}: {error}", image.display())))?;

    set_pin_file(&file).map_err(|error| failure("pin_file", error.to_string()))?;
    checks.ok("pin_file", "F2FS_IOC_SET_PIN_FILE=1 accepted");

    allocate(&file, args.size).map_err(|error| failure("fallocate", error.to_string()))?;
    checks.ok("fallocate", format!("{} bytes allocated", args.size));

    file.sync_all()
        .map_err(|error| failure("fsync_allocated", error.to_string()))?;
    checks.ok("fsync_allocated", "allocation flushed");

    let extents =
        extents::fiemap(&file, args.size).map_err(|error| failure("fiemap", error.to_string()))?;
    extents::validate(&extents, args.size)
        .map_err(|detail| failure("extent_validation", detail))?;
    checks.ok(
        "extent_validation",
        format!("{} extents, 4 KiB aligned, fully allocated", extents.len()),
    );
    report.extents = extents.clone();

    let mountinfo_text = fs::read_to_string("/proc/self/mountinfo")
        .map_err(|error| failure("mountinfo", error.to_string()))?;
    let entries = mountinfo::parse(&mountinfo_text);
    let data = mountinfo::data_mount(&entries)
        .ok_or_else(|| failure("data_mount", "no /data mount in /proc/self/mountinfo"))?;
    let fs_bdev = mountinfo::resolve_bdev(data)
        .ok_or_else(|| failure("data_mount", "/data has no resolvable block device"))?;
    let dm = fs_bdev
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    report.fs_bdev = Some(fs_bdev.display().to_string());
    if !dm.starts_with("dm-") {
        return Err(failure(
            "data_bdev_is_dm",
            format!("expected /dev/block/dm-N, got {}", fs_bdev.display()),
        ));
    }
    checks.ok("data_bdev_is_dm", fs_bdev.display().to_string());

    let slaves_dir = Path::new("/sys/block").join(&dm).join("slaves");
    let slaves = sysfs::entries(&slaves_dir)
        .map_err(|error| failure("slaves", format!("{}: {error}", slaves_dir.display())))?;
    if slaves.len() != 1 {
        return Err(failure(
            "slaves",
            format!(
                "{dm} has {} slaves {slaves:?}, expected exactly one projection",
                slaves.len()
            ),
        ));
    }
    let slave = slaves[0].clone();
    report.slave = Some(slave.clone());
    if !slave.starts_with("espinit-gpt") {
        return Err(failure(
            "slave_is_projection",
            format!("expected an espinit-gpt projection, got {slave}"),
        ));
    }
    checks.ok("slave_is_projection", slave.clone());

    let uevent_path = Path::new("/sys/class/block").join(&slave).join("uevent");
    let uevent = sysfs::uevent(&uevent_path).map_err(|error| {
        failure(
            "projection_uevent",
            format!("{}: {error}", uevent_path.display()),
        )
    })?;
    let partname = sysfs::partname(&uevent).map(str::to_string);
    report.partname = partname.clone();
    if partname.as_deref() != Some("userdata") {
        return Err(failure(
            "projection_partname",
            format!("expected PARTNAME=userdata, got {partname:?}"),
        ));
    }
    checks.ok("projection_partname", "PARTNAME=userdata");

    report.pv_bdev = Some(PV_DEV.to_string());
    let pv_uevent_path = Path::new("/sys/class/block").join(PV_NAME).join("uevent");
    let pv_uevent = sysfs::uevent(&pv_uevent_path).map_err(|error| {
        failure(
            "pv_uevent",
            format!("{}: {error}", pv_uevent_path.display()),
        )
    })?;
    let pv_partname = sysfs::partname(&pv_uevent).map(str::to_string);
    report.pv_partname = pv_partname.clone();
    if pv_partname.is_some() {
        return Err(failure(
            "pv_partname_absent",
            format!("{PV_NAME} still exposes PARTNAME={pv_partname:?}"),
        ));
    }
    checks.ok("pv_partname_absent", format!("{PV_NAME} has no PARTNAME"));

    let projection = Path::new("/dev/block").join(&slave);

    write_pattern(&file, &extents, NONCE_PLAINTEXT_1)
        .map_err(|error| failure("write_plaintext_1", error.to_string()))?;
    file.sync_all()
        .map_err(|error| failure("write_plaintext_1", error.to_string()))?;
    let ciphertext_1 = read_raw(&projection, &extents).map_err(|error| {
        failure(
            "read_ciphertext_1",
            format!("{}: {error}", projection.display()),
        )
    })?;

    write_pattern(&file, &extents, NONCE_PLAINTEXT_2)
        .map_err(|error| failure("write_plaintext_2", error.to_string()))?;
    file.sync_all()
        .map_err(|error| failure("write_plaintext_2", error.to_string()))?;
    let ciphertext_2 = read_raw(&projection, &extents).map_err(|error| {
        failure(
            "read_ciphertext_2",
            format!("{}: {error}", projection.display()),
        )
    })?;

    if ciphertext_1 == ciphertext_2 {
        return Err(failure(
            "ciphertext_changes",
            "the raw bytes at the file extents did not change after two different plaintext writes",
        ));
    }
    checks.ok(
        "ciphertext_changes",
        format!(
            "{} bytes at the pinned extents changed with the plaintext",
            ciphertext_1.len()
        ),
    );

    let raw_pattern = pattern_bytes(&extents, NONCE_RAW);
    write_raw(&projection, &extents, NONCE_RAW)
        .map_err(|error| failure("raw_write", format!("{}: {error}", projection.display())))?;
    let raw_readback = read_raw(&projection, &extents)
        .map_err(|error| failure("raw_readback", format!("{}: {error}", projection.display())))?;
    if raw_readback != raw_pattern {
        return Err(failure(
            "raw_write_visible",
            "a raw write through the projection did not read back byte-exact",
        ));
    }
    checks.ok(
        "raw_write_visible",
        "projection returns the raw bytes written",
    );

    let pv_readback = read_raw(Path::new(PV_DEV), &extents)
        .map_err(|error| failure("pv_readback", format!("{PV_DEV}: {error}")))?;
    if pv_readback == raw_pattern {
        return Err(failure(
            "pv_diverges",
            format!("{PV_DEV} returned the projection's bytes at the same offsets"),
        ));
    }
    checks.ok(
        "pv_diverges",
        format!("{PV_DEV} holds different bytes at the same offsets"),
    );

    let fs_readback =
        read_file(&file, &extents).map_err(|error| failure("fs_readback", error.to_string()))?;
    if fs_readback == raw_pattern {
        return Err(failure(
            "fs_reads_ciphertext",
            "the filesystem returned the raw bytes instead of ciphertext",
        ));
    }
    checks.ok(
        "fs_reads_ciphertext",
        "the file view differs from the raw projection bytes",
    );

    Ok(())
}

fn set_pin_file(file: &File) -> io::Result<()> {
    let pin: u32 = 1;
    // SAFETY: `file` is an open regular file and the pin ioctl reads exactly
    // one `__u32` through the pointer, which outlives the call.
    let result = unsafe {
        ioctl::call(
            file.as_raw_fd(),
            F2FS_IOC_SET_PIN_FILE,
            std::ptr::from_ref(&pin).cast::<c_void>().cast_mut(),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn allocate(file: &File, length: u64) -> io::Result<()> {
    // SAFETY: `file` is an open regular file and `length` is bounded by
    // `Args::parse` well below `i64::MAX`.
    let result = unsafe { ioctl::allocate(file.as_raw_fd(), length) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn write_pattern(file: &File, extents: &[Extent], nonce: u32) -> io::Result<()> {
    for extent in extents {
        let mut offset = 0;
        while offset < extent.length {
            let block = (extent.logical + offset) / pattern::BLOCK;
            file.write_all_at(&pattern::block_bytes(block, nonce), extent.logical + offset)?;
            offset += pattern::BLOCK;
        }
    }
    Ok(())
}

fn write_raw(device: &Path, extents: &[Extent], nonce: u32) -> io::Result<()> {
    let file = OpenOptions::new().read(true).write(true).open(device)?;
    for extent in extents {
        let mut offset = 0;
        while offset < extent.length {
            let block = (extent.logical + offset) / pattern::BLOCK;
            file.write_all_at(
                &pattern::block_bytes(block, nonce),
                extent.physical + offset,
            )?;
            offset += pattern::BLOCK;
        }
    }
    file.sync_all()
}

fn read_raw(device: &Path, extents: &[Extent]) -> io::Result<Vec<u8>> {
    let file = File::open(device)?;
    let mut bytes = Vec::new();
    for extent in extents {
        let mut buffer = vec![0u8; extent.length as usize];
        file.read_exact_at(&mut buffer, extent.physical)?;
        bytes.extend_from_slice(&buffer);
    }
    Ok(bytes)
}

fn read_file(file: &File, extents: &[Extent]) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for extent in extents {
        let mut buffer = vec![0u8; extent.length as usize];
        file.read_exact_at(&mut buffer, extent.logical)?;
        bytes.extend_from_slice(&buffer);
    }
    Ok(bytes)
}

fn pattern_bytes(extents: &[Extent], nonce: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    for extent in extents {
        let mut offset = 0;
        while offset < extent.length {
            let block = (extent.logical + offset) / pattern::BLOCK;
            bytes.extend_from_slice(&pattern::block_bytes(block, nonce));
            offset += pattern::BLOCK;
        }
    }
    bytes
}

fn cleanup(args: &Args, checks: &mut Checks) {
    match fs::remove_file(args.dir.join("probe.img")) {
        Ok(()) => match File::open(&args.dir).and_then(|dir| dir.sync_all()) {
            Ok(()) => checks.ok("unlink", "probe.img removed and the directory synced"),
            Err(error) => checks.failed("unlink", format!("directory fsync failed: {error}")),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => checks.failed("unlink", error.to_string()),
    }
}

fn render(report: &Report, checks: &Checks, pass: bool) -> String {
    let extents = report
        .extents
        .iter()
        .map(|extent| {
            format!(
                "{{\"logical\":{},\"physical\":{},\"length\":{},\"flags\":{}}}",
                extent.logical, extent.physical, extent.length, extent.flags
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let checks_json = checks
        .items
        .iter()
        .map(|check| {
            format!(
                "\"{}\":{{\"pass\":{},\"detail\":\"{}\"}}",
                escape(check.name),
                check.pass,
                escape(&check.detail)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!("  \"size\": {},\n", report.size));
    out.push_str(&format!(
        "  \"fs_bdev\": {},\n",
        json_string(report.fs_bdev.as_deref())
    ));
    out.push_str(&format!(
        "  \"slave\": {},\n",
        json_string(report.slave.as_deref())
    ));
    out.push_str(&format!(
        "  \"partname\": {},\n",
        json_string(report.partname.as_deref())
    ));
    out.push_str(&format!(
        "  \"pv_bdev\": {},\n",
        json_string(report.pv_bdev.as_deref())
    ));
    out.push_str(&format!(
        "  \"pv_partname\": {},\n",
        json_string(report.pv_partname.as_deref())
    ));
    out.push_str(&format!("  \"extents\": [{extents}],\n"));
    out.push_str(&format!("  \"checks\": {{{checks_json}}},\n"));
    out.push_str(&format!(
        "  \"result\": \"{}\"\n",
        if pass { "pass" } else { "fail" }
    ));
    out.push('}');
    out
}

fn json_string(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("\"{}\"", escape(value)),
        None => "null".to_string(),
    }
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if control.is_control() => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<Args, String> {
        Args::parse(arguments.iter().map(|argument| (*argument).to_string()))
    }

    #[test]
    fn defaults_apply_without_arguments() {
        let args = parse(&[]).expect("defaults");
        assert_eq!(args.dir, PathBuf::from(DEFAULT_DIR));
        assert_eq!(args.size, DEFAULT_SIZE_MIB * 1024 * 1024);
    }

    #[test]
    fn one_integer_argument_is_the_size() {
        let args = parse(&["64"]).expect("size only");
        assert_eq!(args.dir, PathBuf::from(DEFAULT_DIR));
        assert_eq!(args.size, 64 * 1024 * 1024);
    }

    #[test]
    fn one_non_integer_argument_is_the_directory() {
        let args = parse(&["/data/gsi/other"]).expect("dir only");
        assert_eq!(args.dir, PathBuf::from("/data/gsi/other"));
        assert_eq!(args.size, DEFAULT_SIZE_MIB * 1024 * 1024);
    }

    #[test]
    fn two_arguments_set_both() {
        let args = parse(&["/data/gsi/other", "16"]).expect("dir and size");
        assert_eq!(args.dir, PathBuf::from("/data/gsi/other"));
        assert_eq!(args.size, 16 * 1024 * 1024);
    }

    #[test]
    fn rejects_out_of_range_or_extra_arguments() {
        let too_big = (MAX_SIZE_MIB + 1).to_string();
        assert!(parse(&["0"]).is_err());
        assert!(parse(&[&too_big]).is_err());
        assert!(parse(&["/tmp/x", "not-a-number"]).is_err());
        assert!(parse(&["a", "b", "c"]).is_err());
    }

    #[test]
    fn f2fs_pin_ioctl_number_matches_the_header() {
        assert_eq!(F2FS_IOC_SET_PIN_FILE, 0x4004_f50d);
    }

    #[test]
    fn escape_quotes_json_metacharacters() {
        assert_eq!(escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
