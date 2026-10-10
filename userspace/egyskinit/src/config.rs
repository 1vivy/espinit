//! Generic bootstrap configuration. ROM/LVM/EFI/BCB schemas live downstream.
pub use egysk_runtime::context::{BootstrapConfig, KernelModule as ModuleEntry};
pub fn parse_bootstrap_config(text: &str) -> anyhow::Result<BootstrapConfig> {
    egysk_runtime::parse_bootstrap_config(text)
}
