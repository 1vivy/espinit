//! Android entry: generic-bootctl's `serve` (AIDL first, HIDL only if refused; the service
//! managers arbitrate what may register) over the esu `Service`: efivarfs-backed GBS1/GBM1
//! records, an always-writable gate and the preserve-nonzero success policy.
#[cfg(target_os = "android")]
mod entry {
    use generic_bootctl_core::Service;
    use gobbl_boot_hal::backend::EsuBackend;
    use std::ffi::{CStr, c_char};
    use std::io;
    use std::path::Path;

    unsafe extern "C" {
        fn __system_property_get(name: *const c_char, value: *mut c_char) -> i32;
    }

    fn property(name: &CStr) -> Result<String, String> {
        let mut value = [0u8; 92]; // PROP_VALUE_MAX from NDK sys/system_properties.h.
        // SAFETY: name is terminated; bionic writes at most PROP_VALUE_MAX bytes.
        let length = unsafe { __system_property_get(name.as_ptr(), value.as_mut_ptr().cast()) };
        if length <= 0 || length as usize >= value.len() {
            return Err(format!("missing property {name:?}"));
        }
        std::str::from_utf8(&value[..length as usize])
            .map(str::to_owned)
            .map_err(|error| error.to_string())
    }

    pub fn service() -> io::Result<Service> {
        let current: u8 = match property(c"ro.boot.slot_suffix")
            .map_err(io::Error::other)?
            .as_str()
        {
            "_a" => 0,
            "_b" => 1,
            _ => return Err(io::Error::other("invalid ro.boot.slot_suffix")),
        };
        let mut backend = EsuBackend::open(
            Path::new("/dev/efivars"),
            Path::new("/dev/block/by-name/misc"),
            current,
        );
        // Best effort: storage failures never gate registration, and the first
        // state-dependent transaction retries the same reconciliation.
        if let Err(error) = backend.reconcile() {
            eprintln!(
                "boot-hal storage unavailable; registering service and retrying on transaction: {error}"
            );
        }
        // The esu HAL is always writable. Its writes are gated by the install
        // decision, the read-only /vendor overlay and the ROM-isolation layer
        // outside this process; `persist.generic_bootctl.rw` belongs to the
        // generic same-path substitution, which the esu module does not use.
        Ok(Service::new(
            Box::new(backend),
            u32::from(current),
            Box::new(|| true),
        ))
    }
}

fn main() {
    // `--stop-stock` is the module's `on post-fs` exec: it disables the stock boot HAL
    // before `class_start early_hal`. Init answers control messages while an exec runs,
    // but not while its main thread sits in `mount_all` at `late-fs` (where vold waits on
    // IBootControl), so the serving process must never wait on `ctl.stop` itself.
    #[cfg(target_os = "android")]
    if std::env::args().nth(1).as_deref() == Some("--stop-stock") {
        gobbl_boot_hal::stock::stop_stock();
        return;
    }
    #[cfg(target_os = "android")]
    if let Err(error) = entry::service()
        .map_err(Into::into)
        .and_then(bootctl_unified::serve)
    {
        eprintln!("boot-hal: {error}");
        std::process::exit(1);
    }
    #[cfg(not(target_os = "android"))]
    {
        eprintln!(
            "boot-hal service requires Android Binder; run cargo test for host state/storage tests"
        );
        std::process::exit(1);
    }
}
