//! Frozen AIDL V1 dispatch using the NDK C ABI (no vendor QTI libraries).
use gobbl_boot_hal::{
    COMMAND_FAILED,
    service::{Hal, Reply},
};
use std::ffi::{CStr, c_char, c_void};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

type Opaque = c_void;
#[link(name = "binder_ndk")]
unsafe extern "C" {
    fn AIBinder_Class_define(
        name: *const c_char,
        create: unsafe extern "C" fn(*mut Opaque) -> *mut Opaque,
        destroy: unsafe extern "C" fn(*mut Opaque),
        transact: unsafe extern "C" fn(*mut Opaque, u32, *const Opaque, *mut Opaque) -> i32,
    ) -> *mut Opaque;
    fn AIBinder_new(class: *const Opaque, args: *mut Opaque) -> *mut Opaque;
    fn AParcel_readInt32(parcel: *const Opaque, value: *mut i32) -> i32;
    fn AParcel_writeInt32(parcel: *mut Opaque, value: i32) -> i32;
    fn AParcel_writeBool(parcel: *mut Opaque, value: bool) -> i32;
    fn AParcel_writeString(parcel: *mut Opaque, value: *const c_char, length: i32) -> i32;
    fn AParcel_writeStatusHeader(parcel: *mut Opaque, status: *const Opaque) -> i32;
    fn AStatus_newOk() -> *mut Opaque;
    fn AStatus_fromServiceSpecificError(error: i32) -> *mut Opaque;
    fn AStatus_delete(status: *mut Opaque);
}
#[link(name = "dl")]
unsafe extern "C" {
    fn dlopen(name: *const c_char, flags: i32) -> *mut Opaque;
    fn dlsym(handle: *mut Opaque, name: *const c_char) -> *mut Opaque;
}
unsafe extern "C" {
    fn __system_property_get(name: *const c_char, value: *mut c_char) -> i32;
}

// Only this Mutex serializes read-modify-write HAL transactions.
static HAL: OnceLock<Mutex<Hal>> = OnceLock::new();

fn execute(code: u32, input: i32) -> Result<Reply, i32> {
    HAL.get()
        .ok_or(COMMAND_FAILED)?
        .lock()
        .map_err(|_| COMMAND_FAILED)?
        .execute(code, input)
}

unsafe extern "C" fn create(args: *mut Opaque) -> *mut Opaque {
    args
}
unsafe extern "C" fn destroy(_: *mut Opaque) {}

unsafe extern "C" fn transact(
    _: *mut Opaque,
    code: u32,
    input: *const Opaque,
    output: *mut Opaque,
) -> i32 {
    if !(1..=11).contains(&code) && code != 16_777_214 && code != 16_777_215 {
        return -74; // STATUS_UNKNOWN_TRANSACTION
    }
    let mut value = 0;
    if matches!(code, 5..=7 | 9..=11) {
        // SAFETY: libbinder_ndk passes a live incoming parcel and value is writable.
        let status = unsafe { AParcel_readInt32(input, &mut value) };
        if status != 0 {
            return status;
        }
    }
    let result = execute(code, value);
    // SAFETY: constructors return owned AStatus objects; output is the callback's
    // live reply parcel. Each status is deleted after serialization exactly once.
    let status = unsafe {
        let status = match &result {
            Ok(_) => AStatus_newOk(),
            Err(error) => AStatus_fromServiceSpecificError(*error),
        };
        let written = AParcel_writeStatusHeader(output, status);
        AStatus_delete(status);
        written
    };
    if status != 0 {
        return status;
    }
    // SAFETY: output remains live, strings are static NUL-terminated UTF-8,
    // and all serialized values use the NDK declarations' exact C ABI types.
    unsafe {
        match result {
            Ok(Reply::Int(value)) => AParcel_writeInt32(output, value),
            Ok(Reply::Bool(value)) => AParcel_writeBool(output, value),
            Ok(Reply::Text(value)) => {
                AParcel_writeString(output, value.as_ptr(), value.to_bytes().len() as i32)
            }
            Ok(Reply::Void) | Err(_) => 0,
        }
    }
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
        .map_err(|e| e.to_string())
}

pub fn run() -> Result<(), String> {
    let current = match property(c"ro.boot.slot_suffix")?.as_str() {
        "_a" => 0,
        "_b" => 1,
        _ => return Err("invalid ro.boot.slot_suffix".into()),
    };
    let mut hal = Hal::new(
        Path::new("/dev/efivars"),
        Path::new("/dev/block/by-name/misc"),
        current,
    );
    if hal.reconcile().is_err() {
        eprintln!("boot-hal storage unavailable; registering service and retrying on transaction");
    }
    HAL.set(Mutex::new(hal))
        .map_err(|_| "HAL already initialized")?;

    // These stable platform C entrypoints are exported on Android, but excluded
    // from public app-NDK stubs. Resolve them from the already loaded platform
    // library, checking every symbol; do not copy AOSP libraries into the payload.
    // SAFETY: constant soname and symbol names are terminated; RTLD_NOW is 2.
    let handle = unsafe { dlopen(c"libbinder_ndk.so".as_ptr(), 2) };
    if handle.is_null() {
        return Err("platform libbinder_ndk unavailable".into());
    }
    let symbol = |name: &CStr| -> Result<*mut Opaque, String> {
        // SAFETY: handle is a successfully opened library retained for process life.
        let pointer = unsafe { dlsym(handle, name.as_ptr()) };
        if pointer.is_null() {
            Err(format!("missing platform symbol {name:?}"))
        } else {
            Ok(pointer)
        }
    };
    // SAFETY: signatures match binder_manager.h, binder_process.h and
    // binder_stability.h in AOSP's stable platform NDK ABI; all pointers checked.
    let (add_service, join, max_threads, mark_vintf) = unsafe {
        (
            std::mem::transmute::<
                *mut Opaque,
                unsafe extern "C" fn(*mut Opaque, *const c_char) -> i32,
            >(symbol(c"AServiceManager_addService")?),
            std::mem::transmute::<*mut Opaque, unsafe extern "C" fn()>(symbol(
                c"ABinderProcess_joinThreadPool",
            )?),
            std::mem::transmute::<*mut Opaque, unsafe extern "C" fn(u32) -> bool>(symbol(
                c"ABinderProcess_setThreadPoolMaxThreadCount",
            )?),
            std::mem::transmute::<*mut Opaque, unsafe extern "C" fn(*mut Opaque)>(symbol(
                c"AIBinder_markVintfStability",
            )?),
        )
    };
    // SAFETY: descriptor and callbacks are static, userdata is unused; binder and
    // its class remain alive until process exit. libbinder validates the token.
    unsafe {
        let class = AIBinder_Class_define(
            c"android.hardware.boot.IBootControl".as_ptr(),
            create,
            destroy,
            transact,
        );
        if class.is_null() {
            return Err("AIBinder_Class_define failed".into());
        }
        let binder = AIBinder_new(class, std::ptr::null_mut());
        if binder.is_null() {
            return Err("AIBinder_new failed".into());
        }
        mark_vintf(binder);
        if !max_threads(0) {
            return Err("Binder thread pool configuration failed".into());
        }
        let status = add_service(
            binder,
            c"android.hardware.boot.IBootControl/default".as_ptr(),
        );
        if status != 0 {
            return Err(format!("service registration failed: {status}"));
        }
        join();
    }
    Err("Binder thread pool unexpectedly returned".into())
}
