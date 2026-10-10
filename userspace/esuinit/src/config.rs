//! Generic bootstrap configuration. ROM/LVM/EFI/BCB schemas live downstream.
pub use esp_runtime::context::{BootstrapConfig, KernelModule as ModuleEntry};
pub fn parse_bootstrap_config(text: &str) -> anyhow::Result<BootstrapConfig> {
    esp_runtime::parse_bootstrap_config(text)
}
