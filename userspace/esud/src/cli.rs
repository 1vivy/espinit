use crate::{defs, init_event, ksucalls};
use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(version = defs::FULL_VERSION)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    Early,
    PostFs,
    PostFsData,
    Services,
    BootCompleted,
    Recovery,
    Insmod {
        module: PathBuf,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        params: Vec<String>,
    },
    Core {
        #[command(subcommand)]
        command: Core,
    },
    Platform {
        #[command(subcommand)]
        command: Platform,
    },
    Sepolicy {
        #[command(subcommand)]
        command: Sepolicy,
    },
    /// Lab-only: restart the device when Android never reports boot completion.
    Watchdog {
        seconds: u64,
    },
    #[command(disable_help_flag = true)]
    Resetprop {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        args: Vec<String>,
    },
}
#[derive(clap::Subcommand, Debug)]
enum Core {
    SetBootMode {
        #[arg(value_parser = clap::value_parser!(u32).range(1..=2))]
        mode: u32,
    },
}
#[derive(clap::Subcommand, Debug)]
enum Platform {
    Reload,
}
#[derive(clap::Subcommand, Debug)]
enum Sepolicy {
    Patch { sepolicy: String },
    Apply { file: String },
    Check { sepolicy: String },
}

pub fn run() -> Result<()> {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("esu"),
    );
    ksucalls::setup_sigsys_handler();
    let args: Vec<String> = std::env::args().collect();
    if args.first().is_some_and(|arg| arg.ends_with("resetprop")) {
        crate::resetprop::resetprop_main(&args);
    }
    match Args::parse().command {
        Commands::Early => init_event::on_stage(init_event::Stage::Early),
        Commands::PostFs => init_event::on_stage(init_event::Stage::PostFs),
        Commands::PostFsData => init_event::on_stage(init_event::Stage::PostFsData),
        Commands::Services => init_event::on_stage(init_event::Stage::Service),
        Commands::BootCompleted => init_event::on_stage(init_event::Stage::BootCompleted),
        Commands::Recovery => init_event::on_stage(init_event::Stage::Recovery),
        Commands::Insmod { module, params } => crate::debug::insmod(&module, &params),
        Commands::Core {
            command: Core::SetBootMode { mode },
        } => esuinit::set_core_boot_mode(mode),
        Commands::Platform {
            command: Platform::Reload,
        } => init_event::reload(),
        Commands::Sepolicy { command } => match command {
            Sepolicy::Patch { sepolicy } => crate::sepolicy::apply_strict(&sepolicy),
            Sepolicy::Apply { file } => {
                crate::sepolicy::apply_strict(&std::fs::read_to_string(file)?)
            }
            Sepolicy::Check { sepolicy } => crate::sepolicy::check_rule(&sepolicy),
        },
        Commands::Watchdog { seconds } => crate::watchdog::run(seconds),
        Commands::Resetprop { args } => {
            let mut all = vec!["resetprop".to_owned()];
            all.extend(args);
            crate::resetprop::resetprop_main(&all)
        }
    }
}
