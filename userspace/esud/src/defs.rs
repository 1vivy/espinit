#[cfg(target_os = "android")]
mod android {
    use const_format::concatcp;

    // Writable state is logs only, created after /data is available.
    pub const WORKING_DIR: &str = "/data/adb/esu/";
    pub const LOG_DIR: &str = concatcp!(WORKING_DIR, "log/");
    pub const BOOTLOG_DIR: &str = LOG_DIR;
    pub const BUSYBOX: &str = "/dev/esp/esu/bin/busybox";
    pub const MODULE_DIR: &str = "/dev/esp/esu/modules";
}

#[allow(unused)]
pub const VERSION_CODE: &str = env!("VERSION_CODE");
#[allow(unused)]
pub const VERSION_NAME: &str = env!("VERSION_NAME");
#[cfg(target_os = "android")]
pub const FULL_VERSION: &str = const_format::formatcp!(
    "{VERSION_NAME} (uapi: {})",
    crate::ksu_uapi::ESU_UAPI_VERSION
);

#[cfg(target_os = "android")]
pub use android::*;
