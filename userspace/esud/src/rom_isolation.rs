//! Per-ROM Android properties and shared credential-store isolation.
//! Init recreates the ESP after PID-1 detach; esud owns its RO bind view.

// Everything below the decision tables runs on Android only; the host build
// compiles this module for its tests, which use just a few of these items.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use esuinit::config::RomConfig;

#[cfg(target_os = "android")]
use crate::overlay::label;
#[cfg(target_os = "android")]
use anyhow::{Context, Result, ensure};
#[cfg(target_os = "android")]
use log::{error, info};
#[cfg(target_os = "android")]
use std::ffi::CString;
#[cfg(target_os = "android")]
use std::fs::{self, OpenOptions};
#[cfg(target_os = "android")]
use std::io::Write;
#[cfg(target_os = "android")]
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt, chown};
#[cfg(target_os = "android")]
use std::path::Path;

/// Projected name of the shared physical credential store.
pub const SHARED_PARTITION: &str = "metadata_shared";

/// Mount point of the shared physical credential store.
pub const SHARED_MOUNT: &str = "/metadata/shared";

/// AOSP's password slot directory inside the shared credential store.
pub const SLOT_DIR: &str = "password_slots";

/// Android-visible credential directory the shared slot directory is bound over.
pub const SLOT_MOUNT: &str = "/metadata/password_slots";

/// Directory holding the private device nodes this module creates.
pub const DEVICE_DIR: &str = "/dev/esu";

/// Owner of the AOSP password slot directory contract.
pub const SYSTEM_UID: u32 = 1000;

/// Mode of the shared and Android-visible password slot directories.
pub const SLOT_MODE: u32 = 0o771;

/// SELinux type of any password slot metadata directory.
pub const SLOT_LABEL: &str = "u:object_r:password_slot_metadata_file:s0";

/// GSI/DSU marker. `PasswordSlotManager` reads it as an integer, so each ROM
/// gets its own `gsi<N>` password slot label, while every boolean consumer
/// parses `"<N>"` for N >= 2 as false: VAB merges keep running and no DSU UI
/// appears. ROM 1 and unmanaged boots leave init's `0` (`host`) untouched.
pub const IMAGE_RUNNING_PROP: &str = "ro.gsid.image_running";

/// vold's metadata key deletion switch. A fresh per-ROM `metadata` volume must
/// never trigger KeyMint `deleteAllKeys` on the shared credential state.
pub const DELETE_ALL_KEYS_PROP: &str = "ro.crypto.metadata_init_delete_all_keys.enabled";

/// Shared credential store flags: writable state that can never execute and
/// carries no device nodes.
pub const SHARED_FLAGS: libc::c_ulong =
    libc::MS_NOATIME | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;

/// f2fs mount data of the shared credential store.
pub const SHARED_DATA: &str = "discard";

/// Log file, inside the state root, of non-fatal isolation failures.
pub const ROM_LOG: &str = "rom-isolation.log";

/// The Android-side isolation decisions for one ROM.
#[derive(Debug, PartialEq, Eq)]
pub struct Session {
    /// Property overrides, in apply order.
    pub properties: Vec<(&'static str, String)>,
    /// Whether the shared physical credential store is projected by this ROM
    /// and must therefore be mounted and bound over [`SLOT_MOUNT`].
    pub share_credential_store: bool,
}

/// Decide the isolation session from the ROM configuration alone.
///
/// Managed ROMs always stop vold from deleting the shared key state; only ROM 2
/// and above mark themselves as a numbered GSI image (ROM 1 must stay the
/// `host` owner of the shared password slot map), and only a ROM that projects
/// `metadata_shared` shares the store.
pub fn session(rom: &RomConfig, number: u32) -> Session {
    let mut properties = Vec::new();

    if rom.managed {
        if number >= 2 {
            properties.push((IMAGE_RUNNING_PROP, number.to_string()));
        }
        properties.push((DELETE_ALL_KEYS_PROP, "false".to_owned()));
    }

    Session {
        properties,
        share_credential_store: rom.managed
            && rom
                .partitions
                .iter()
                .any(|entry| entry.name == SHARED_PARTITION),
    }
}

#[cfg(target_os = "android")]
pub struct RuntimeRom {
    pub config: RomConfig,
    pub number: u32,
}

/// Missing identity is unmanaged; a selected identity requires a valid Slot.
#[cfg(target_os = "android")]
pub fn runtime_rom() -> Result<Option<RuntimeRom>> {
    let vars = Path::new("/dev/efivars");
    let Some(id) = esu_platform::efivars::booted_rom(vars)? else {
        info!("no bdsvars ROM identity: unmanaged boot");
        return Ok(None);
    };
    let number = esu_platform::efivars::rom_number(vars, &id)?;
    let text = fs::read_to_string(format!("/dev/esp/esu/roms/{id}.toml"))?;
    let config = esuinit::config::parse_selected_rom(&text, &id).map_err(anyhow::Error::msg)?;
    esuinit::config::validate_rom(&config, number).map_err(anyhow::Error::msg)?;
    Ok(Some(RuntimeRom { config, number }))
}

/// Override Android properties and share the projected credential store.
///
/// Errors reach init's `reboot_on_failure` service and stop the boot in
/// Android; recovery's core RC omits that property and only logs them. There is
/// no partial-isolation boot.
#[cfg(target_os = "android")]
pub fn early(rom: &RomConfig, number: u32) -> Result<()> {
    let session = session(rom, number);

    for (name, value) in &session.properties {
        crate::resetprop::set_property(name, value)
            .with_context(|| format!("cannot set {name}"))?;
    }

    if session.share_credential_store {
        mount_shared_credential_store()?;
    } else if rom.managed {
        info!("credential store not shared: {SHARED_PARTITION} is not projected");
    }
    if rom.managed && number >= 2 {
        deny_ufs_bsg_writes()?;
    }

    Ok(())
}

/// Deny UFS boot-LUN writes to the boot-control HALs of ROMs without physical
/// firmware authority. Only the `hal_bootctl` attribute loses `write`/`ioctl`:
/// the TEE listener reaches RPMB through the same node (stock policy grants it
/// `tee_*`), so a wildcard deny would cut KeyMint/Weaver/DeviceInfo storage.
/// A missing node or an unreadable label is recorded as an explicit gap; a
/// failed policy update is fatal, rather than booting under a false seal.
#[cfg(target_os = "android")]
fn deny_ufs_bsg_writes() -> Result<()> {
    let node = Path::new("/dev/ufs-bsg0");
    let label = match extattr::lgetxattr(node, "security.selinux")
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    {
        Ok(label) => label,
        Err(error) => {
            report(&format!(
                "UFS BSG write deny skipped for {}: {error}",
                node.display()
            ));
            return Ok(());
        }
    };
    let mut fields = label.trim_end_matches('\0').split(':');
    let (Some("u"), Some("object_r"), Some(kind), Some(_level), None) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        report(&format!(
            "UFS BSG write deny skipped: invalid SELinux label {label:?}"
        ));
        return Ok(());
    };
    if kind.is_empty()
        || !kind
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        report(&format!(
            "UFS BSG write deny skipped: invalid SELinux type {kind:?}"
        ));
        return Ok(());
    }
    crate::sepolicy::apply_strict(&format!(
        "deny hal_bootctl {kind} chr_file {{ write ioctl }}"
    ))
    .context("cannot deny UFS BSG write/ioctl to the boot HAL of a secondary ROM")?;
    info!("UFS BSG write/ioctl denied to hal_bootctl for SELinux type {kind}");
    Ok(())
}

/// Post-fs-data stage: give this ROM a gatekeeper first-boot marker, so its
/// first boot does not clear the TEE-backed credential state it does not own
/// yet. Managed ROMs only; the caller records failures instead of failing boot.
#[cfg(target_os = "android")]
pub fn post_fs_data(rom: &RomConfig) -> Result<()> {
    if !rom.managed {
        return Ok(());
    }

    gatekeeper_first_boot()
}

/// Record a non-fatal isolation failure on kmsg and in the state root.
#[cfg(target_os = "android")]
pub fn report(message: &str) {
    error!("{message}");

    if let Ok(mut kmsg) = OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = writeln!(kmsg, "<3>esud: {message}");
    }

    let path = Path::new(crate::defs::LOG_DIR).join(ROM_LOG);
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut log) => {
            let _ = writeln!(log, "{message}");
        }
        Err(open) => error!("cannot append to {}: {open}", path.display()),
    }
}

/// Mount the shared physical credential store and bind its slot directory over
/// the Android-visible one, so every ROM uses one AOSP-managed slot map whose
/// entries stay per-ROM (`host`, `gsi2`, ...).
#[cfg(target_os = "android")]
fn mount_shared_credential_store() -> Result<()> {
    let device = esu_platform::block::partition_by_name(SHARED_PARTITION)
        .context("cannot resolve the shared credential partition")?;

    prepare_directory(DEVICE_DIR, 0o700, 0, 0)?;
    let node = format!("{DEVICE_DIR}/{SHARED_PARTITION}");
    create_block_node(&node, device)?;

    prepare_directory(SHARED_MOUNT, 0o700, 0, 0)
        .context("cannot prepare the shared credential mount point")?;
    if !crate::overlay::mounted(SHARED_MOUNT, "f2fs")? {
        mount(&node, SHARED_MOUNT, "f2fs", SHARED_FLAGS, SHARED_DATA)
            .context("cannot mount the shared credential store")?;
    }

    let slots = format!("{SHARED_MOUNT}/{SLOT_DIR}");
    prepare_directory(&slots, SLOT_MODE, 0, SYSTEM_UID)?;
    label_directory(&slots)?;
    prepare_directory(SLOT_MOUNT, SLOT_MODE, 0, SYSTEM_UID)?;
    label_directory(SLOT_MOUNT)?;
    if !crate::overlay::mounted(SLOT_MOUNT, "f2fs")? {
        bind(&slots, SLOT_MOUNT)?;
    }

    info!("shared credential store {slots} bound over {SLOT_MOUNT}");
    Ok(())
}

/// Give this ROM's gatekeeper data directory a first-boot marker. The marker
/// exists from now on, so gatekeeperd never treats this ROM as a cold boot and
/// never deletes the users of the credential state it shares.
#[cfg(target_os = "android")]
fn gatekeeper_first_boot() -> Result<()> {
    prepare_directory(GATEKEEPER_DIR, 0o700, SYSTEM_UID, SYSTEM_UID)?;

    let marker = format!("{GATEKEEPER_DIR}/{COLD_BOOT_MARKER}");
    match fs::symlink_metadata(&marker) {
        Ok(metadata) => {
            ensure!(metadata.is_file(), "{marker} is not a regular file");
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&marker)
                .with_context(|| format!("cannot create {marker}"))?;
            label(Path::new(&marker), GATEKEEPER_LABEL)
                .with_context(|| format!("cannot label {marker}"))?;
            fs::set_permissions(&marker, fs::Permissions::from_mode(0o600))?;
            chown(&marker, Some(SYSTEM_UID), Some(0))
                .with_context(|| format!("cannot set the owner of {marker}"))?;
            file.sync_all()?;
            info!("gatekeeper first-boot marker created at {marker}");
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Gatekeeper's per-ROM data directory and first-boot marker.
const GATEKEEPER_DIR: &str = "/data/misc/gatekeeper";
const COLD_BOOT_MARKER: &str = ".coldboot";
const GATEKEEPER_LABEL: &str = "u:object_r:gatekeeper_data_file:s0";

/// Create or adopt a directory with exact mode and ownership. A symlink or any
/// other non-directory already at the path fails closed; nothing is followed.
#[cfg(target_os = "android")]
fn prepare_directory(path: &str, mode: u32, uid: u32, gid: u32) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(metadata.is_dir(), "{path} is not a directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).with_context(|| format!("cannot create {path}"))?;
        }
        Err(error) => return Err(error.into()),
    }

    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("cannot set the mode of {path}"))?;
    chown(path, Some(uid), Some(gid)).with_context(|| format!("cannot set the owner of {path}"))?;

    Ok(())
}

/// Apply a SELinux type to an existing directory inode.
#[cfg(target_os = "android")]
fn label_directory(path: &str) -> Result<()> {
    label(Path::new(path), SLOT_LABEL).with_context(|| format!("cannot label {path}"))
}

/// Create the private block node of a sysfs-resolved device, replacing any
/// leftover node. The node name is built here from numeric fields, never from a
/// device-supplied string.
#[cfg(target_os = "android")]
fn create_block_node(path: &str, device: libc::dev_t) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let node = CString::new(path)?;
    // SAFETY: the path is NUL-free and lives for the call; the mode carries no
    // setuid or setgid bit and the device number comes from sysfs.
    let result = unsafe { libc::mknod(node.as_ptr(), libc::S_IFBLK | 0o600, device) };
    ensure!(
        result == 0,
        "cannot create {path}: {}",
        std::io::Error::last_os_error()
    );

    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_block_device() && metadata.rdev() == device,
        "unusable block node {path}"
    );

    Ok(())
}

/// Mount `source` at `target` with exactly these flags and data string. There is
/// no retry and no alternative source.
#[cfg(target_os = "android")]
fn mount(
    source: &str,
    target: &str,
    filesystem: &str,
    flags: libc::c_ulong,
    data: &str,
) -> Result<()> {
    let source_pointer = CString::new(source)?;
    let target_pointer = CString::new(target)?;
    let filesystem_pointer = CString::new(filesystem)?;
    let data_pointer = CString::new(data)?;

    // SAFETY: every pointer refers to a live NUL-terminated string for the
    // duration of the call, and the kernel copies anything it retains.
    let result = unsafe {
        libc::mount(
            source_pointer.as_ptr(),
            target_pointer.as_ptr(),
            filesystem_pointer.as_ptr(),
            flags,
            data_pointer.as_ptr().cast(),
        )
    };
    ensure!(
        result == 0,
        "cannot mount {source} at {target} as {filesystem}: {}",
        std::io::Error::last_os_error()
    );

    Ok(())
}

/// Bind `source` over `target`; a bind mount takes no filesystem type or data.
#[cfg(target_os = "android")]
fn bind(source: &str, target: &str) -> Result<()> {
    let source_pointer = CString::new(source)?;
    let target_pointer = CString::new(target)?;

    // SAFETY: both pointers refer to live NUL-terminated strings for the
    // duration of the call.
    let result = unsafe {
        libc::mount(
            source_pointer.as_ptr(),
            target_pointer.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    ensure!(
        result == 0,
        "cannot bind {source} over {target}: {}",
        std::io::Error::last_os_error()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rom(managed: bool, partitions: &[&str]) -> RomConfig {
        use std::fmt::Write as _;

        let mut projections = String::new();
        for name in partitions {
            write!(
                projections,
                "[[partitions]]\nname = \"{name}\"\n\
                 backend = \"/dev/block/by-name/{name}\"\nread_only = false\n"
            )
            .unwrap();
        }
        let text = format!("schema_version = 1\nid = \"rom1\"\nmanaged = {managed}\n{projections}");

        esuinit::config::parse_rom(&text).unwrap()
    }

    #[test]
    fn the_property_table_follows_the_rom_number() {
        let nothing: Vec<(&'static str, String)> = Vec::new();
        assert_eq!(session(&rom(false, &[]), 1).properties, nothing);

        assert_eq!(
            session(&rom(true, &["metadata"]), 1).properties,
            vec![(DELETE_ALL_KEYS_PROP, "false".to_owned())]
        );

        for number in 2..=esuinit::config::MAX_ROM_NUMBER {
            assert_eq!(
                session(&rom(true, &["metadata"]), number).properties,
                vec![
                    (IMAGE_RUNNING_PROP, number.to_string()),
                    (DELETE_ALL_KEYS_PROP, "false".to_owned()),
                ],
                "ROM {number}"
            );
        }
    }

    #[test]
    fn the_credential_store_is_shared_exactly_when_it_is_projected() {
        assert!(session(&rom(true, &["metadata", SHARED_PARTITION]), 1).share_credential_store);
        assert!(session(&rom(true, &[SHARED_PARTITION]), 3).share_credential_store);
        assert!(!session(&rom(true, &["metadata", "userdata"]), 1).share_credential_store);
        assert!(!session(&rom(false, &[]), 1).share_credential_store);
    }

    #[test]
    fn mount_options_and_labels_pin_the_session_contract() {
        assert_eq!(
            SHARED_FLAGS,
            libc::MS_NOATIME | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC
        );
        assert_eq!(SHARED_FLAGS & libc::MS_RDONLY, 0);
        assert_eq!(SHARED_DATA, "discard");

        assert_eq!(
            (SHARED_PARTITION, SHARED_MOUNT),
            ("metadata_shared", "/metadata/shared")
        );
        assert_eq!(
            (SLOT_DIR, SLOT_MOUNT, SLOT_MODE, SYSTEM_UID),
            ("password_slots", "/metadata/password_slots", 0o771, 1000)
        );
        assert_eq!(SLOT_LABEL, "u:object_r:password_slot_metadata_file:s0");
        assert_eq!(GATEKEEPER_LABEL, "u:object_r:gatekeeper_data_file:s0");
    }
}
