//! KernelSU scripts from the immutable ESP module tree.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]
use anyhow::{Context, Result, bail, ensure};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(target_os = "android")]
pub fn manifest() -> Result<esuinit::config::Manifest> {
    esuinit::config::parse_manifest(&std::fs::read_to_string("/dev/esp/esu/manifest.toml")?)
        .map_err(anyhow::Error::msg)
}

#[derive(Clone, Copy)]
enum ScriptWait {
    Strict,
    Until(Instant),
    Detached,
}

impl ScriptWait {
    fn for_stage(stage: &str, timeout: Duration) -> Self {
        match stage {
            "early" => Self::Strict,
            "service" | "boot-completed" => Self::Detached,
            _ => Self::Until(Instant::now() + timeout),
        }
    }
}

#[cfg(target_os = "android")]
pub fn scripts(
    order: &[String],
    stage: &str,
    rom: Option<&crate::rom_isolation::RuntimeRom>,
) -> Result<()> {
    scripts_in(
        Path::new(crate::defs::MODULE_DIR),
        order,
        stage,
        &[crate::defs::BUSYBOX, "sh"],
        rom.map(|rom| (rom.config.id.as_str(), rom.number)),
        Duration::from_secs(35),
    )
}

fn scripts_in(
    root: &Path,
    order: &[String],
    stage: &str,
    interpreter: &[&str],
    rom: Option<(&str, u32)>,
    timeout: Duration,
) -> Result<()> {
    let wait = ScriptWait::for_stage(stage, timeout);
    for id in order {
        let directory = root.join(id);
        if stage == "recovery" && !directory.join("recovery-ok").is_file() {
            continue;
        }
        let script = directory.join(format!("{stage}.sh"));
        if !script.exists() {
            continue;
        }
        if matches!(wait, ScriptWait::Until(deadline) if Instant::now() >= deadline) {
            log::warn!("{stage} script deadline exhausted; skipping remaining modules");
            break;
        }
        let mut command = Command::new(interpreter[0]);
        command
            .args(&interpreter[1..])
            .arg(&script)
            .current_dir(&directory)
            .env("ASH_STANDALONE", "1")
            .env("ESU", "true")
            .env("ESU_MODULE", id)
            .env(
                "PATH",
                format!(
                    "/dev/esp/esu/bin:{}",
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
        if let Some((id, number)) = rom {
            command
                .env("ESU_ROM", id)
                .env("ESU_ROM_NUMBER", number.to_string());
        } else {
            command.env_remove("ESU_ROM").env_remove("ESU_ROM_NUMBER");
        }
        if let Err(error) = execute(&mut command, wait)
            .with_context(|| format!("module {id} {stage}: {}", script.display()))
        {
            if matches!(wait, ScriptWait::Strict) {
                return Err(error);
            }
            // Ordinary KernelSU lifecycle scripts are best effort. A failed
            // module must not suppress later modules or fail Android init.
            log::warn!("{error:#}");
        }
    }
    Ok(())
}

fn execute(command: &mut Command, wait: ScriptWait) -> Result<()> {
    command.process_group(0).stdin(Stdio::null());
    if matches!(wait, ScriptWait::Detached) {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    #[cfg(target_os = "android")]
    // SAFETY: the single-threaded stage process uses the existing KernelSU
    // cgroup escape before exec, so init cannot reap its scripts/daemons.
    unsafe {
        command.pre_exec(|| {
            crate::utils::switch_cgroups();
            Ok(())
        });
    }
    let mut child = command.spawn().context("spawn module script")?;
    match wait {
        ScriptWait::Detached => Ok(()),
        ScriptWait::Strict => {
            let status = child.wait()?;
            ensure!(status.success(), "script failed: {status}");
            Ok(())
        }
        ScriptWait::Until(deadline) => wait_until(&mut child, deadline),
    }
}

fn wait_until(child: &mut Child, deadline: Instant) -> Result<()> {
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "script failed: {status}");
            return Ok(());
        }
        let now = Instant::now();
        if now >= deadline {
            // Each script has its own process group. Terminate a timed-out
            // foreground tree, then reap the shell instead of leaving zombies.
            let pid = i32::try_from(child.id()).context("script pid overflow")?;
            // SAFETY: child has not been reaped, so this owned group ID cannot
            // have been reused by an unrelated process.
            if unsafe { libc::kill(-pid, libc::SIGKILL) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    child.kill().context("kill timed-out module script")?;
                }
            }
            child.wait().context("reap timed-out module script")?;
            bail!("script exceeded stage deadline");
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn script(root: &Path, id: &str, stage: &str, body: &str) {
        let module = root.join(id);
        fs::create_dir_all(&module).unwrap();
        fs::write(module.join(format!("{stage}.sh")), body).unwrap();
    }

    fn run(root: &Path, stage: &str, timeout: Duration) -> Result<()> {
        scripts_in(
            root,
            &["first".into(), "second".into()],
            stage,
            &["/bin/sh"],
            Some(("rom2", 2)),
            timeout,
        )
    }

    fn await_file(path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = fs::read_to_string(path)
                && !text.is_empty()
            {
                return text;
            }
            assert!(Instant::now() < deadline, "missing {}", path.display());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn early_failure_is_fatal_and_does_not_run_later_modules() {
        let root = tempfile::tempdir().unwrap();
        script(root.path(), "first", "early", "exit 17\n");
        script(
            root.path(),
            "second",
            "early",
            "echo unexpected > ../later\n",
        );
        let error = run(root.path(), "early", Duration::from_secs(1)).unwrap_err();
        assert!(format!("{error:#}").contains("script failed"));
        assert!(!root.path().join("later").exists());
    }

    #[test]
    fn post_fs_data_continues_after_failure_and_is_rerunnable() {
        let root = tempfile::tempdir().unwrap();
        script(root.path(), "first", "post-fs-data", "exit 17\n");
        script(
            root.path(),
            "second",
            "post-fs-data",
            "echo \"$ESU_MODULE:$ESU_ROM:$ESU_ROM_NUMBER\" >> ../later\n",
        );
        for _ in 0..2 {
            run(root.path(), "post-fs-data", Duration::from_secs(1)).unwrap();
        }
        assert_eq!(
            fs::read_to_string(root.path().join("later")).unwrap(),
            "second:rom2:2\nsecond:rom2:2\n"
        );
    }

    #[test]
    fn post_fs_data_has_one_bounded_stage_deadline() {
        let root = tempfile::tempdir().unwrap();
        script(
            root.path(),
            "first",
            "post-fs-data",
            "echo $$ > ../pid\nwhile :; do sleep 1; done\n",
        );
        script(
            root.path(),
            "second",
            "post-fs-data",
            "echo unexpected > ../later\n",
        );
        let start = Instant::now();
        run(root.path(), "post-fs-data", Duration::from_millis(200)).unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(!root.path().join("later").exists());
        let pid: i32 = await_file(&root.path().join("pid")).trim().parse().unwrap();
        // The timed-out shell has been killed and reaped, not merely abandoned.
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn service_and_boot_completed_outlive_the_stage_and_launch_all_modules() {
        for stage in ["service", "boot-completed"] {
            let root = tempfile::tempdir().unwrap();
            script(
                root.path(),
                "first",
                stage,
                "echo $$ > ../pid\nsleep 1\necho finished > ../finished\n",
            );
            script(
                root.path(),
                "second",
                stage,
                "echo $$ > ../second-pid\necho launched > ../later\n",
            );
            let start = Instant::now();
            run(root.path(), stage, Duration::from_millis(50)).unwrap();
            assert!(start.elapsed() < Duration::from_millis(750));
            let pid: i32 = await_file(&root.path().join("pid")).trim().parse().unwrap();
            assert_eq!(unsafe { libc::getpgid(pid) }, pid);
            assert_eq!(await_file(&root.path().join("later")), "launched\n");
            assert_eq!(await_file(&root.path().join("finished")), "finished\n");
            for name in ["pid", "second-pid"] {
                let pid: i32 = await_file(&root.path().join(name)).trim().parse().unwrap();
                assert_eq!(unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) }, pid);
            }
        }
    }
}
