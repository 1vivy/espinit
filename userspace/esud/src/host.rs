//! Linux host artifact builder; never accesses a device.
use crate::boot_patch;

#[cfg(target_os = "linux")]
pub fn run() -> anyhow::Result<()> {
    use clap::Parser;

    #[derive(Parser)]
    #[command(
        name = "esud",
        version,
        about = "Build esu host artifacts; never access a device"
    )]
    struct Args {
        #[command(subcommand)]
        command: Command,
    }

    #[derive(clap::Subcommand)]
    enum Command {
        /// Build the takeover CPIO, its module set and a complete ESP tree
        BootPatch(boot_patch::BootPatchArgs),
    }

    match Args::parse().command {
        Command::BootPatch(args) => boot_patch::patch(&args),
    }
}
