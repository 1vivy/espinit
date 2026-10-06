#[cfg(target_os = "android")]
mod android {
    use const_format::concatcp;
    use std::time::Duration;

    pub const BOOT_STAGE_TIMEOUT: Duration = Duration::from_secs(35);
    pub const EMULATED_SOFT_REBOOT_TIMEOUT: Duration = Duration::from_secs(5);
    pub const WAITSYS_READY_TIMEOUT: Duration = Duration::from_secs(2);
    pub const WAITSYS_STOP_TIMEOUT: Duration = Duration::from_secs(5);
    pub const BOOTLOG_TIMEOUT: &str = "30s";

    // Persistent, writable esu state root. The read-only ESP payload is
    // delivered separately under /debug_ramdisk/esp/esu.
    pub const WORKING_DIR: &str = "/metadata/esu/";
    pub const BINARY_DIR: &str = concatcp!(WORKING_DIR, "bin/");
    pub const LOG_DIR: &str = concatcp!(WORKING_DIR, "log/");
    // Boot-window logcat/dmesg captures are megabytes per boot. They live on
    // /data, never on the managed /metadata volume, which must keep room to
    // stage the next payload generation beside the current one.
    pub const BOOTLOG_DIR: &str = "/data/adb/esu/log/";

    pub const DAEMON_PATH: &str = concatcp!(WORKING_DIR, "esud");
    pub const DAEMON_LINK_PATH: &str = concatcp!(BINARY_DIR, "esud");

    pub const MODULE_DIR: &str = concatcp!(WORKING_DIR, "modules/");
    pub const MODULE_UPDATE_DIR: &str = concatcp!(WORKING_DIR, "modules_update/");
    pub const METAMODULE_DIR: &str = concatcp!(WORKING_DIR, "metamodule/");

    // modules.rc is spliced into init.rc by the kernel read hook on the next boot.
    pub const PREINIT_DIR: &str = concatcp!(WORKING_DIR, "initrc/");
    pub const MODULES_RC_FILE: &str = "modules.rc";
    pub const MODULES_RC_TMP_FILE: &str = ".modules.rc.tmp";

    pub const MODULE_WEB_DIR: &str = "webroot";
    pub const MODULE_ACTION_SH: &str = "action.sh";
    pub const DISABLE_FILE_NAME: &str = "disable";
    pub const UPDATE_FILE_NAME: &str = "update";
    pub const REMOVE_FILE_NAME: &str = "remove";
    pub const MODULE_INIT_RC_DIR: &str = "initrc";

    // Module config system
    pub const MODULE_CONFIG_DIR: &str = concatcp!(WORKING_DIR, "module_configs/");
    pub const PERSIST_CONFIG_NAME: &str = "persist.config";
    pub const TEMP_CONFIG_NAME: &str = "tmp.config";

    // Metamodule support
    pub const METAMODULE_MOUNT_SCRIPT: &str = "metamount.sh";
    pub const METAMODULE_METAINSTALL_SCRIPT: &str = "metainstall.sh";
    pub const METAMODULE_METAUNINSTALL_SCRIPT: &str = "metauninstall.sh";
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
