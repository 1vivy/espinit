#![cfg_attr(not(test), no_main)]

/// Same binary is PID1 rdinit, the init-context reconstruction tool, and the
/// root-only ELF module loader tool. No invocation starts another daemon.
///
/// # Safety
/// argc/argv/envp are the C process entry vectors supplied by the runtime.
#[cfg(not(test))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, envp: *const *const u8) -> i32 {
    // no_main bypasses Rust's argv initialization on static bionic. Read the
    // supplied C vectors directly; handoff still uses the original raw bytes.
    if argc < 1 || argv.is_null() {
        return 1;
    }
    let args = (0..argc as usize)
        .map(|index| {
            unsafe { std::ffi::CStr::from_ptr((*argv.add(index)).cast()) }
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    if args.get(1).map(String::as_str) == Some("--load-module") {
        let load = (|| -> anyhow::Result<()> {
            esp_runtime::fsutil::root_only()?;
            anyhow::ensure!(
                args.len() >= 3,
                "usage: esuinit --load-module /absolute/file.ko [key=value ...]"
            );
            let path = std::path::Path::new(&args[2]);
            anyhow::ensure!(path.is_absolute(), "module path must be absolute");
            esp_runtime::fsutil::regular(path)?;
            let entry = esuinit::config::ModuleEntry {
                name: path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("module")
                    .to_owned(),
                path: args[2].clone(),
                params: args[3..].join(" "),
            };
            esuinit::loader::load_managed_module(path, &entry)?;
            Ok(())
        })();
        return match load {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{error:#}");
                1
            }
        };
    }
    match esp_runtime::reconstruction_argument(&args) {
        Ok(Some(descriptor)) => {
            if unsafe { libc::geteuid() } != 0 {
                eprintln!("reconstruction requires root");
                return 1;
            }
            if let Err(error) = esp_runtime::bootstrap::reconstruct(descriptor) {
                esp_runtime::fatal_boot(&format!("reconstruction: {error:#}"));
            }
            return 0;
        }
        Err(error) => {
            eprintln!("{error:#}");
            return 1;
        }
        Ok(None) => {}
    }
    if unsafe { libc::getpid() } != 1 {
        eprintln!("esuinit must run as PID 1");
        return 1;
    }
    if let Err(error) = esuinit::init::run() {
        esp_runtime::fatal_boot(&format!("rdinit: {error:#}"));
    }
    if let Err(error) = unsafe { esuinit::handoff::exec_real_init(argc, argv, envp) } {
        esp_runtime::fatal_boot(&format!("handoff: {error:#}"));
    }
    esp_runtime::fatal_boot("original init unexpectedly returned")
}
