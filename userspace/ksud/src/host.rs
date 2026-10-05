//! Host artifact builder. Deliberately separate from the Android daemon CLI.
#[cfg(target_os = "linux")]
mod boot_patch;

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use clap::Parser;

    #[derive(Parser)]
    #[command(
        name = "ksud",
        version,
        about = "Build espinit host artifacts; never access a device"
    )]
    struct Args {
        #[command(subcommand)]
        command: Command,
    }

    #[derive(clap::Subcommand)]
    enum Command {
        /// Build canonical CPIO and a complete ESP tree, optionally patch a test boot image
        BootPatch(boot_patch::BootPatchArgs),
    }

    match Args::parse().command {
        Command::BootPatch(args) => boot_patch::patch(&args),
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("ksud boot-patch requires a Linux host; espinitd is the Android daemon")
}
