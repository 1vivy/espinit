//! Port of esud's process-group runner and esuinit's 35-second foreground bound.
use crate::context::*;
use anyhow::{Context, Result, bail, ensure};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Native blocks signals before worker creation; never pass that mask to scripts.
pub fn child_signals(command: &mut Command) {
    // SAFETY: this pre-exec callback uses only async-signal-safe libc operations.
    unsafe {
        command.pre_exec(|| {
            let mut empty = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
            libc::sigemptyset(empty.as_mut_ptr());
            if libc::sigprocmask(libc::SIG_SETMASK, empty.as_ptr(), std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub fn wait_until(child: &mut Child, deadline: Instant, kill_descendants: bool) -> Result<()> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if kill_descendants {
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                }
                ensure!(status.success(), "script exited {status}");
                return Ok(());
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let pid = i32::try_from(child.id()).context("script PID overflow")?;
                // Own process group; the foreground child has not been reaped.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
                child.kill().ok();
                child.wait().context("reap failed/timed-out child")?;
                if let Err(error) = result {
                    return Err(error).context("wait for script");
                }
                bail!("script exceeded 35-second stage deadline");
            }
        }
    }
}

/// Every phase is a blocking barrier, including late stages. Modules may launch
/// their own daemons, but owning scripts must succeed before RC services start.
pub fn run_one(
    module: &Module,
    directory: &Path,
    script_name: &str,
    mode: BootMode,
    backing_root: &str,
    deadline: Instant,
) -> Result<()> {
    let script = directory.join(format!("{script_name}.sh"));
    match std::fs::symlink_metadata(&script) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("stat module script"),
        Ok(metadata) => ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() <= 1024 * 1024,
            "invalid module script {}",
            script.display()
        ),
    }
    ensure!(
        Instant::now() < deadline,
        "stage deadline exhausted before {}",
        module.id
    );
    let mut command = Command::new(format!("{BIN}/busybox"));
    child_signals(&mut command);
    command
        .arg("sh")
        .arg(&script)
        .current_dir(directory)
        .process_group(0)
        .stdin(Stdio::null())
        .env("ASH_STANDALONE", "1")
        .env("KSU", "true")
        .env(
            "KSU_MODE",
            match mode {
                BootMode::Normal => "normal",
                BootMode::Recovery => "recovery",
                BootMode::Charger => "charger",
            },
        )
        .env(
            "KSU_STAGE",
            script_name.split('.').next().unwrap_or(script_name),
        )
        .env(
            "KSU_PHASE",
            script_name.split_once('.').map_or("", |(_, phase)| phase),
        )
        .env("KSU_MODULE", &module.id)
        .env("MODDIR", directory)
        .env("KSU_MODULE_STATE", format!("{ROOT}/state/{}", module.id))
        .env("KSU_BACKING_ROOT", backing_root)
        .env(
            "PATH",
            format!("{ROOT}/rd/bin:{BIN}:/system/bin:/system/xbin:/vendor/bin"),
        );
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn {} {script_name}", module.id))?;
    wait_until(
        &mut child,
        deadline,
        script_name.starts_with("rdinit") || script_name == "uninstall",
    )
}

/// Barrier ordering is phase outermost, module order innermost. An optional
/// failure removes the module from subsequent phases and all downstream work.
pub fn dispatch(descriptor: &mut Descriptor, stage: &str) -> Result<Vec<Module>> {
    let mut phases = vec![stage.to_owned()];
    if let Some(extra) = descriptor.config.phases.get(stage) {
        phases.extend(extra.iter().map(|p| format!("{stage}.{p}")));
    }
    let deadline = Instant::now() + Duration::from_secs(35);
    let mut removed = Vec::new();
    for phase in phases {
        let mut admitted = Vec::new();
        for module in std::mem::take(&mut descriptor.modules) {
            let directory = Path::new(MODULES).join(&module.id);
            match run_one(
                &module,
                &directory,
                &phase,
                descriptor.mode,
                &descriptor.config.backing.mount_at,
                deadline,
            ) {
                Ok(()) => admitted.push(module),
                Err(error) if module.critical => {
                    return Err(error)
                        .with_context(|| format!("critical module {} {phase}", module.id));
                }
                Err(error) => {
                    log::warn!(
                        "removing optional module {} after {phase}: {error:#}",
                        module.id
                    );
                    removed.push(module);
                }
            }
        }
        descriptor.modules = admitted;
    }
    Ok(removed)
}
