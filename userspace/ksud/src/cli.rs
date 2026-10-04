use anyhow::{Context, Ok, Result};
use clap::Parser;
use std::path::PathBuf;

use android_logger::Config;
use log::{LevelFilter, info};

use crate::module::regenerate_preinit_rc;
use crate::{assets, debug, defs, init_event, ksucalls, module, module_config, utils};

/// espinit userspace cli
#[derive(Parser, Debug)]
#[command(author, version = defs::FULL_VERSION, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Manage espinit modules
    Module {
        #[command(subcommand)]
        command: Module,
    },
    /// Run `early` module scripts when the Android dynamic runtime is available
    Early,

    /// Run `post-fs` module scripts without reporting Android boot events
    PostFs,

    /// Trigger `post-fs-data` event
    PostFsData,

    /// Trigger `service` event
    Services,

    /// Trigger `boot-completed` event
    BootCompleted,

    /// Run `recovery` module scripts without reporting Android boot events
    Recovery,

    /// Emulate system reboot
    SoftReboot,

    /// Load a kernel module with kallsyms access
    Insmod {
        /// kernel module path
        module: PathBuf,
        /// module load parameters (e.g. key=val key2=val2)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        params: Vec<String>,
    },

    /// Install the espinit userspace component
    Install,

    /// Unload the espinit kernel module
    Unload,

    /// Uninstall espinit modules and the userspace component
    Uninstall,

    /// SELinux policy Patch tool
    Sepolicy {
        #[command(subcommand)]
        command: Sepolicy,
    },

    /// Manage kernel features
    Feature {
        #[command(subcommand)]
        command: Feature,
    },

    /// For developers
    Debug {
        #[command(subcommand)]
        command: Debug,
    },
    /// Kernel interface
    Kernel {
        #[command(subcommand)]
        command: Kernel,
    },

    /// Resetprop - Magisk-compatible system property tool
    #[command(disable_help_flag = true)]
    Resetprop {
        /// Arguments passed to resetprop
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        args: Vec<String>,
    },

    /// Manage initrc injection
    Initrc {
        #[command(subcommand)]
        command: Initrc,
    },
}

#[derive(clap::Subcommand, Debug)]
enum Debug {
    /// Get kernel version
    Version,

    /// For testing
    Test,

    /// Extract an embedded binary to a specified path
    ExtractBinary {
        /// binary name (e.g. busybox)
        name: String,
        /// destination file path
        path: PathBuf,
    },

    /// Process mark management
    Mark {
        #[command(subcommand)]
        command: MarkCommand,
    },

    /// Get kernel info
    Info,
}

#[derive(clap::Subcommand, Debug)]
enum MarkCommand {
    /// Get mark status for a process (or all)
    Get {
        /// target pid (0 for total count)
        #[arg(default_value = "0")]
        pid: i32,
    },

    /// Mark a process
    Mark {
        /// target pid (0 for all processes)
        #[arg(default_value = "0")]
        pid: i32,
    },

    /// Unmark a process
    Unmark {
        /// target pid (0 for all processes)
        #[arg(default_value = "0")]
        pid: i32,
    },

    /// Refresh mark for all running processes
    Refresh,
}

#[derive(clap::Subcommand, Debug)]
enum Sepolicy {
    /// Patch sepolicy
    Patch {
        /// sepolicy statements
        sepolicy: String,
    },

    /// Apply sepolicy from file
    Apply {
        /// sepolicy file path
        file: String,
    },

    /// Check if sepolicy statement is supported/valid
    Check {
        /// sepolicy statements
        sepolicy: String,
    },
}

#[derive(clap::Subcommand, Debug)]
enum Module {
    /// Install module <ZIP>
    Install {
        /// module zip file path
        zip: String,
    },

    /// Undo module uninstall mark <id>
    UndoUninstall {
        /// module id
        id: String,
    },

    /// Uninstall module <id>
    Uninstall {
        /// module id
        id: String,
    },

    /// enable module <id>
    Enable {
        /// module id
        id: String,
    },

    /// disable module <id>
    Disable {
        // module id
        id: String,
    },

    /// run action for module <id>
    Action {
        // module id
        id: String,
    },

    /// list all modules
    List,

    /// manage module configuration
    Config {
        /// target internal module name (resolved as internal.<name>)
        #[arg(long)]
        internal: Option<String>,
        #[command(subcommand)]
        command: ModuleConfigCmd,
    },
}

#[derive(clap::Subcommand, Debug)]
enum ModuleConfigCmd {
    /// Get a config value
    Get {
        /// config key
        key: String,
    },

    /// Set a config value
    Set {
        /// config key
        key: String,
        /// config value (omit to read from stdin)
        value: Option<String>,
        /// read value from stdin (default if value not provided)
        #[arg(long)]
        stdin: bool,
        /// use temporary config (cleared on reboot)
        #[arg(short, long)]
        temp: bool,
    },

    /// List all config entries
    List,

    /// Delete a config entry
    Delete {
        /// config key
        key: String,
        /// delete from temporary config
        #[arg(short, long)]
        temp: bool,
    },

    /// Clear all config entries
    Clear {
        /// clear temporary config
        #[arg(short, long)]
        temp: bool,
    },
}

#[derive(clap::Subcommand, Debug)]
enum Feature {
    /// Get feature value and support status
    Get {
        /// Feature ID or name (kernel_umount, selinux_hide)
        id: String,
        /// Read from config file
        #[arg(long, default_value_t = false)]
        config: bool,
    },

    /// Set feature value
    Set {
        /// Feature ID or name
        id: String,
        /// Feature value (0=disable, 1=enable)
        value: u64,
    },

    /// List all available features
    List,

    /// Check feature status (supported/unsupported/managed)
    Check {
        /// Feature ID or name (kernel_umount, selinux_hide)
        id: String,
    },

    /// Load configuration from file and apply to kernel
    Load,

    /// Save current kernel feature states to file
    Save,
}

#[derive(clap::Subcommand, Debug)]
enum Kernel {
    /// Nuke ext4 sysfs
    NukeExt4Sysfs {
        /// mount point
        mnt: String,
    },
    /// Manage umount list
    Umount {
        #[command(subcommand)]
        command: UmountOp,
    },
    /// Notify that module is mounted
    NotifyModuleMounted,
}

#[derive(clap::Subcommand, Debug)]
enum UmountOp {
    /// Add mount point to umount list
    Add {
        /// mount point path
        mnt: String,
        /// umount flags (default: 0, MNT_DETACH: 2)
        #[arg(short, long, default_value = "0")]
        flags: u32,
    },
    /// Delete mount point from umount list
    Del {
        /// mount point path
        mnt: String,
    },
    /// Wipe all entries from umount list
    Wipe,
}

#[derive(clap::Subcommand, Debug)]
enum Initrc {
    /// Regenerate preinit rc file
    Refresh,
}

pub fn run() -> Result<()> {
    android_logger::init_once(
        Config::default()
            .with_max_level(crate::debug_select!(LevelFilter::Trace, LevelFilter::Info))
            .with_tag("espinit"),
    );

    ksucalls::setup_sigsys_handler();

    // the daemon is also reachable through a `resetprop` symlink
    let arg0 = std::env::args().next().unwrap_or_default();
    if arg0.ends_with("resetprop") {
        let all_args: Vec<String> = std::env::args().collect();
        crate::resetprop::resetprop_main(&all_args);
    }

    let cli = Args::parse();

    log::info!("command: {:?}", cli.command);

    let result = match cli.command {
        Commands::Early => init_event::on_stage(init_event::Stage::Early),
        Commands::PostFs => init_event::on_stage(init_event::Stage::PostFs),
        Commands::PostFsData => init_event::on_stage(init_event::Stage::PostFsData),
        Commands::BootCompleted => init_event::on_stage(init_event::Stage::BootCompleted),
        Commands::Recovery => init_event::on_stage(init_event::Stage::Recovery),

        Commands::SoftReboot => crate::soft_reboot::soft_reboot(),

        Commands::Insmod { module, params } => debug::insmod(&module, &params),

        Commands::Module { command } => {
            utils::switch_mnt_ns(1)?;
            match command {
                Module::Install { zip } => module::install_module(&zip),
                Module::UndoUninstall { id } => module::undo_uninstall_module(&id),
                Module::Uninstall { id } => module::uninstall_module(&id),
                Module::Enable { id } => module::enable_module(&id),
                Module::Disable { id } => module::disable_module(&id),
                Module::Action { id } => module::run_action(&id),
                Module::List => module::list_modules(),
                Module::Config { internal, command } => {
                    let module_id = match internal {
                        Some(internal_name) => format!("internal.{internal_name}"),
                        None => std::env::var("ESPINIT_MODULE").map_err(|_| {
                            anyhow::anyhow!(
                                "This command must be run in the context of a module or passed --internal <name>"
                            )
                        })?,
                    };
                    crate::module::validate_module_id(&module_id)?;

                    match command {
                        ModuleConfigCmd::Get { key } => {
                            // Use merge_configs to respect priority (temp overrides persist)
                            let config = module_config::merge_configs(&module_id)?;
                            match config.get(&key) {
                                Some(value) => {
                                    println!("{value}");
                                    Ok(())
                                }
                                None => anyhow::bail!("Key '{key}' not found"),
                            }
                        }
                        ModuleConfigCmd::Set {
                            key,
                            value,
                            stdin,
                            temp,
                        } => {
                            // Validate key at CLI layer for better user experience
                            module_config::validate_config_key(&key)?;

                            // Read value from stdin or argument
                            let value_str = match value {
                                Some(v) if !stdin => v,
                                _ => {
                                    // Read from stdin
                                    use std::io::Read;
                                    let mut buffer = String::new();
                                    std::io::stdin()
                                        .read_to_string(&mut buffer)
                                        .context("Failed to read from stdin")?;
                                    buffer
                                }
                            };

                            // Validate value
                            module_config::validate_config_value(&value_str)?;

                            let config_type = if temp {
                                module_config::ConfigType::Temp
                            } else {
                                module_config::ConfigType::Persist
                            };
                            module_config::set_config_value(
                                &module_id,
                                &key,
                                &value_str,
                                config_type,
                            )
                        }
                        ModuleConfigCmd::List => {
                            let config = module_config::merge_configs(&module_id)?;
                            if config.is_empty() {
                                println!("No config entries found");
                            } else {
                                for (key, value) in config {
                                    println!("{key}={value}");
                                }
                            }
                            Ok(())
                        }
                        ModuleConfigCmd::Delete { key, temp } => {
                            let config_type = if temp {
                                module_config::ConfigType::Temp
                            } else {
                                module_config::ConfigType::Persist
                            };
                            module_config::delete_config_value(&module_id, &key, config_type)
                        }
                        ModuleConfigCmd::Clear { temp } => {
                            let config_type = if temp {
                                module_config::ConfigType::Temp
                            } else {
                                module_config::ConfigType::Persist
                            };
                            module_config::clear_config(&module_id, config_type)
                        }
                    }
                }
            }
        }
        Commands::Install => utils::install(),
        Commands::Unload => crate::unload::unload(),
        Commands::Uninstall => utils::uninstall(),
        Commands::Sepolicy { command } => match command {
            Sepolicy::Patch { sepolicy } => crate::sepolicy::live_patch(&sepolicy),
            Sepolicy::Apply { file } => crate::sepolicy::apply_file(file),
            Sepolicy::Check { sepolicy } => crate::sepolicy::check_rule(&sepolicy),
        },
        Commands::Services => {
            if ksucalls::get_version() <= 0 {
                info!("espinit not available, exiting services");
                std::process::exit(0);
            }
            init_event::on_stage(init_event::Stage::Service)
        }
        Commands::Feature { command } => match command {
            Feature::Get { id, config } => {
                if config {
                    crate::feature::get_feature_config(&id)
                } else {
                    crate::feature::get_feature(&id)
                }
            }
            Feature::Set { id, value } => crate::feature::set_feature(&id, value),
            Feature::List => {
                crate::feature::list_features();
                Ok(())
            }
            Feature::Check { id } => crate::feature::check_feature(&id),
            Feature::Load => crate::feature::load_config_and_apply(),
            Feature::Save => crate::feature::save_config(),
        },

        Commands::Debug { command } => match command {
            Debug::Version => {
                println!("Kernel Version: {}", ksucalls::get_version());
                Ok(())
            }
            Debug::Test => assets::ensure_binaries(false),
            Debug::ExtractBinary { name, path } => {
                let data = assets::get_asset_data(&name)?;
                utils::ensure_binary(&path, &data, false)
            }
            Debug::Mark { command } => match command {
                MarkCommand::Get { pid } => debug::mark_get(pid),
                MarkCommand::Mark { pid } => debug::mark_set(pid),
                MarkCommand::Unmark { pid } => debug::mark_unset(pid),
                MarkCommand::Refresh => debug::mark_refresh(),
            },
            Debug::Info => {
                let info = ksucalls::get_info();
                println!("version: {}", info.version);
                println!("flags: 0x{:x}", info.flags);
                println!("uapi_version: {}", info.uapi_version);
                println!("features: 0x{:x}", info.features);
                println!("lkm: {}", ksucalls::is_lkm());
                println!("runtime_mode: {}", ksucalls::runtime_mode());
                Ok(())
            }
        },

        Commands::Resetprop { args } => {
            let mut full_args = vec!["resetprop".to_string()];
            full_args.extend(args);
            crate::resetprop::resetprop_main(&full_args)
        }

        Commands::Kernel { command } => match command {
            Kernel::NukeExt4Sysfs { mnt } => ksucalls::nuke_ext4_sysfs(&mnt),
            Kernel::Umount { command } => match command {
                UmountOp::Add { mnt, flags } => ksucalls::umount_list_add(&mnt, flags),
                UmountOp::Del { mnt } => ksucalls::umount_list_del(&mnt),
                UmountOp::Wipe => ksucalls::umount_list_wipe(),
            },
            Kernel::NotifyModuleMounted => {
                ksucalls::report_module_mounted();
                Ok(())
            }
        },
        Commands::Initrc { command } => match command {
            Initrc::Refresh => regenerate_preinit_rc(),
        },
    };

    if let Err(e) = &result {
        log::error!("Error: {e:?}");
    }
    result
}
