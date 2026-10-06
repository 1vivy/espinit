// SPDX-License-Identifier: GPL-3.0-only
//! Host smoke test for one external-origin thin view.
//!
//! The crate's own ioctls build everything: two loop devices for a blank
//! `thin-pool` (metadata and data) and one for the physical origin, the pool
//! device, the view's thin id through a real pool message, and the view itself.
//! It then proves the three properties the firmware views depend on:
//!
//! * an unwritten block reads the origin's exact bytes (read-through);
//! * a 4 KiB write is visible on the view and absent from the origin;
//! * `delete <id>` followed by `create_thin <id>` restores the origin bytes.
//!
//! Device-mapper and loop-control need root, so the test skips itself when it is
//! not privileged: `cargo test` stays usable for any developer, and the lane
//! runs the same binary with `sudo -n` to get the real evidence.

use dm::{DeviceMapper, DeviceNumber, Mapper, MessageError, Target};
use fw_views::plan;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const BLOCK: usize = 4096;
const ORIGIN_SECTORS: u64 = 16_384; // 8 MiB
const META_SECTORS: u64 = 8_192; // 4 MiB
const DATA_SECTORS: u64 = 65_536; // 32 MiB
const CHUNK_SECTORS: u64 = 128;
const LOW_WATER_MARK: u64 = 8;
const THIN_ID: u32 = (2 << 16) | 1; // ROM 2, first view: the reserved range
const POOL: &str = "fwviews-smoke-pool";
const VIEW: &str = "fwviews-smoke-view";
const LOOP_CTL_GET_FREE: libc::c_ulong = 0x4c82;
const LOOP_SET_FD: libc::c_ulong = 0x4c00;
const LOOP_CLR_FD: libc::c_ulong = 0x4c01;

/// An attached loop device; detaching it in `Drop` keeps the test re-runnable.
struct Loop {
    node: PathBuf,
    device: File,
    _backing: File,
}

impl Drop for Loop {
    fn drop(&mut self) {
        // SAFETY: `device` is the open loop device this test attached and the
        // request takes no pointer argument.
        unsafe { libc::ioctl(self.device.as_raw_fd(), LOOP_CLR_FD as _, 0) };
    }
}

fn privileged() -> bool {
    // SAFETY: `geteuid` has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

fn attach_loop(path: &Path) -> io::Result<Loop> {
    let control = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/loop-control")?;
    // SAFETY: the request takes no pointer argument and `control` is the
    // loop-control device.
    let number = unsafe { libc::ioctl(control.as_raw_fd(), LOOP_CTL_GET_FREE as _, 0) };

    if number < 0 {
        return Err(io::Error::last_os_error());
    }

    let node = PathBuf::from(format!("/dev/loop{number}"));
    let device = OpenOptions::new().read(true).write(true).open(&node)?;
    // A read-only backing file makes the kernel mark the loop device read-only,
    // which the pool's metadata and data devices can never be.
    let backing = OpenOptions::new().read(true).write(true).open(path)?;

    // SAFETY: the argument is the live backing file descriptor, which the
    // kernel copies; both descriptors outlive the call.
    let attached = unsafe {
        libc::ioctl(
            device.as_raw_fd(),
            LOOP_SET_FD as _,
            backing.as_raw_fd() as libc::c_ulong,
        )
    };

    if attached < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(Loop {
        node,
        device,
        _backing: backing,
    })
}

/// Device number of one block node, from sysfs.
fn loop_number(node: &Path) -> DeviceNumber {
    let name = node.file_name().unwrap().to_string_lossy().into_owned();
    let text = fs::read_to_string(format!("/sys/class/block/{name}/dev")).unwrap();

    parse_dev(&text)
}

fn parse_dev(text: &str) -> DeviceNumber {
    let (major, minor) = text.trim().split_once(':').unwrap();

    DeviceNumber {
        major: major.parse().unwrap(),
        minor: minor.parse().unwrap(),
    }
}

/// Open the view through its own device node, creating one only when udev has
/// not (the payload needs no `/dev/mapper` node either).
fn open_view(name: &str, directory: &Path) -> File {
    let mapped = Path::new("/dev/mapper").join(name);

    if let Ok(file) = OpenOptions::new().read(true).write(true).open(&mapped) {
        return file;
    }

    let number = DeviceMapper::device_number(name).unwrap();
    let node = directory.join(format!("{name}.node"));
    let _ = fs::remove_file(&node);
    let path = std::ffi::CString::new(node.as_os_str().to_str().unwrap()).unwrap();

    // SAFETY: the path is NUL terminated, the node is created below a private
    // temporary directory, and the device number came from sysfs.
    let created = unsafe {
        libc::mknod(
            path.as_ptr(),
            libc::S_IFBLK | 0o600,
            libc::makedev(number.major, number.minor),
        )
    };

    assert_eq!(created, 0, "cannot create the view node");
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&node)
        .unwrap()
}

fn write_file(path: &Path, size: u64, fill: impl Fn(u64) -> u8) {
    let file = File::create(path).unwrap();
    file.set_len(size).unwrap();
    let mut offset = 0;
    while offset < size {
        let block = vec![fill(offset / BLOCK as u64); BLOCK];
        file.write_all_at(&block, offset).unwrap();
        offset += BLOCK as u64;
    }
    file.sync_all().unwrap();
}

fn read_block(file: &mut File, offset: u64) -> Vec<u8> {
    let mut buffer = vec![0; BLOCK];
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.read_exact(&mut buffer).unwrap();
    buffer
}

#[test]
fn external_origin_view_reads_through_isolates_writes_and_forgets_on_delete() {
    if !privileged() {
        eprintln!("skipping the thin-pool smoke: it needs root for dm and loop ioctls");
        return;
    }

    let directory = std::env::temp_dir().join(format!("fwviews-smoke-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();

    let origin_path = directory.join("origin.img");
    let meta_path = directory.join("meta.img");
    let data_path = directory.join("data.img");
    write_file(&origin_path, ORIGIN_SECTORS * 512, |block| {
        (block as u8) ^ 0x5a
    });
    write_file(&meta_path, META_SECTORS * 512, |_| 0);
    write_file(&data_path, DATA_SECTORS * 512, |_| 0);

    let origin = attach_loop(&origin_path).unwrap();
    let meta = attach_loop(&meta_path).unwrap();
    let data = attach_loop(&data_path).unwrap();
    let (origin_dev, meta_dev, data_dev) = (
        loop_number(&origin.node),
        loop_number(&meta.node),
        loop_number(&data.node),
    );

    let mut mapper = DeviceMapper::open().unwrap();

    let pool = mapper
        .activate(
            POOL,
            &[Target {
                start: 0,
                length: META_SECTORS,
                kind: "thin-pool".to_owned(),
                params: format!(
                    "{}:{} {}:{} {CHUNK_SECTORS} {LOW_WATER_MARK} 1 skip_block_zeroing",
                    meta_dev.major, meta_dev.minor, data_dev.major, data_dev.minor
                ),
            }],
        )
        .unwrap();
    assert_eq!(pool, DeviceMapper::device_number(POOL).unwrap());

    mapper
        .message(POOL, 0, &plan::create_thin(THIN_ID))
        .unwrap();
    mapper
        .activate(
            VIEW,
            &[Target {
                start: 0,
                length: ORIGIN_SECTORS,
                kind: "thin".to_owned(),
                params: format!(
                    "{}:{} {THIN_ID} {}:{}",
                    pool.major, pool.minor, origin_dev.major, origin_dev.minor
                ),
            }],
        )
        .unwrap();

    // Read-through: the unwritten view serves the origin's exact bytes.
    let mut view = open_view(VIEW, &directory);
    let expected = read_block(&mut File::open(&origin_path).unwrap(), 0);
    assert_eq!(read_block(&mut view, 0), expected, "read-through failed");

    // Isolated write: visible on the view, absent from the origin.
    let mut written = vec![0xaa; BLOCK];
    written[..4].copy_from_slice(b"COW!");
    view.write_all_at(&written, 0).unwrap();
    view.sync_all().unwrap();
    assert_eq!(read_block(&mut view, 0), written, "view write not visible");
    assert_eq!(
        read_block(&mut File::open(&origin_path).unwrap(), 0),
        expected,
        "the origin changed"
    );

    // Delete and recreate: the view forgets every provisioned block and reads
    // the origin again.
    drop(view);
    mapper.remove(VIEW);

    match mapper.message(POOL, 0, &plan::delete_thin(THIN_ID)) {
        Ok(()) => {}
        Err(error) => panic!("delete {THIN_ID} failed: {error:?}"),
    }

    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        match mapper.message(POOL, 0, &plan::create_thin(THIN_ID)) {
            Ok(()) => break,
            Err(MessageError::AlreadyExists) if Instant::now() < deadline => {
                // The pool worker commits the deletion asynchronously.
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("create_thin {THIN_ID} failed after delete: {error:?}"),
        }
    }

    mapper
        .activate(
            VIEW,
            &[Target {
                start: 0,
                length: ORIGIN_SECTORS,
                kind: "thin".to_owned(),
                params: format!(
                    "{}:{} {THIN_ID} {}:{}",
                    pool.major, pool.minor, origin_dev.major, origin_dev.minor
                ),
            }],
        )
        .unwrap();

    let mut view = open_view(VIEW, &directory);
    assert_eq!(
        read_block(&mut view, 0),
        expected,
        "a recreated view did not restore the origin bytes"
    );

    // Leaving the devices behind would break the next run; the committed mapper
    // is torn down explicitly because the smoke owns the whole stack.
    drop(view);
    mapper.remove(VIEW);
    mapper.remove(POOL);
    fs::remove_dir_all(&directory).unwrap();
}
