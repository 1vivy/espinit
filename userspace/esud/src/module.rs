//! KernelSU scripts from the immutable ESP module tree.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]
use anyhow::{Context, Result, bail, ensure};
use std::fmt;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(target_os = "android")]
pub fn manifest() -> Result<esuinit::config::Manifest> {
    esuinit::config::parse_manifest(&std::fs::read_to_string("/dev/esp/esu/manifest.toml")?)
        .map_err(anyhow::Error::msg)
}

/// Admission policy of one ESP module, from the same markers PID 1 uses:
/// `disable`/`remove` skip it, recovery admits only `recovery-ok` modules, and
/// only an admitted module's `critical` marker escalates its failures.
///
/// The caller applies safe mode first, so ordering is safe mode, skip markers,
/// recovery filter, and only then module identity. Returns the module's
/// criticality, or `None` when it must be left alone; a critical module that
/// cannot be identified escalates instead of being silently dropped.
pub fn admission(directory: &Path, id: &str, recovery: bool) -> Result<Option<bool>> {
    let critical = match esuinit::platform::module_policy(directory, recovery) {
        Ok(esuinit::platform::ModulePolicy::Admitted { critical }) => critical,
        Ok(esuinit::platform::ModulePolicy::Skipped) => return Ok(None),
        Err(failure) => {
            // A malformed marker never turns optional work into a boot stop.
            log::warn!(
                "ignoring module {}: {}: {}",
                directory.display(),
                failure.error,
                failure.detail
            );
            return Ok(None);
        }
    };
    if let Err(error) = identify(directory, id) {
        if critical && !recovery {
            return Err(CriticalModuleError::new(error).into());
        }
        // An optional module that cannot be identified contributes nothing, so
        // invalid input never reaches scripts, policy or overlays.
        log::warn!("{error:#}");
        return Ok(None);
    }
    Ok(Some(critical))
}

/// Identify one admitted module: `module.prop` must declare exactly its own id,
/// the same check PID 1 performs before the handoff. The read is bounded so an
/// oversized module can never be pulled into memory.
fn identify(directory: &Path, id: &str) -> Result<()> {
    let path = directory.join("module.prop");
    let mut prop = String::new();
    std::fs::File::open(&path)
        .and_then(|file| file.take(65537).read_to_string(&mut prop))
        .with_context(|| format!("module {id} module.prop: {}", path.display()))?;
    let mut ids = prop
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| *key == "id")
        .map(|(_, value)| value);
    ensure!(
        prop.len() <= 65536 && ids.next() == Some(id) && ids.next().is_none(),
        "module {id} module.prop id mismatch"
    );
    Ok(())
}

/// A failure of a module marked `critical`. Normal Android boot must not
/// continue past it; recovery and optional modules only log it.
#[derive(Debug)]
pub struct CriticalModuleError(anyhow::Error);

impl CriticalModuleError {
    pub fn new(error: impl Into<anyhow::Error>) -> Self {
        Self(error.into())
    }
}

impl fmt::Display for CriticalModuleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "critical module failed: {:#}", self.0)
    }
}

impl std::error::Error for CriticalModuleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

/// True when the error came from a module marked `critical`, anywhere in its
/// context chain. The daemon escalates these to a boot-stopping failure in
/// normal Android; everything else is logged.
pub fn is_critical_failure(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(<dyn std::error::Error>::is::<CriticalModuleError>)
}

#[derive(Clone, Copy)]
enum ScriptWait {
    Until(Instant),
    /// `service`/`boot-completed` daemons outlive the stage, as upstream.
    Detached,
}

impl ScriptWait {
    fn for_stage(stage: &str, timeout: Duration) -> Self {
        match stage {
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
    recovery: bool,
) -> Result<()> {
    scripts_in(
        Path::new(crate::defs::MODULE_DIR),
        order,
        stage,
        &[crate::defs::BUSYBOX, "sh"],
        rom.map(|rom| (rom.config.id.as_str(), rom.number)),
        Duration::from_secs(35),
        recovery,
    )
}

fn scripts_in(
    root: &Path,
    order: &[String],
    stage: &str,
    interpreter: &[&str],
    rom: Option<(&str, u32)>,
    timeout: Duration,
    recovery: bool,
) -> Result<()> {
    let wait = ScriptWait::for_stage(stage, timeout);
    for id in order {
        let directory = root.join(id);
        // The shared admission markers decide which modules run at all: a
        // disabled/removed module and, in recovery, a module PID 1 did not
        // admit do no work here either, and an admitted module must still be
        // identifiable.
        let Some(critical) = admission(&directory, id, recovery)? else {
            continue;
        };
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
                    "/debug_ramdisk/esu/bin:{}",
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
        match execute(&mut command, wait)
            .with_context(|| format!("module {id} {stage}: {}", script.display()))
        {
            Ok(()) => {}
            // Ordinary KernelSU lifecycle scripts are best effort: their
            // failures must not suppress later modules or fail Android init.
            // A critical module is the exception, and only in normal Android.
            // Later detached exits are not observed here.
            Err(error) if recovery || !critical => log::warn!("{error:#}"),
            Err(error) => return Err(CriticalModuleError::new(error).into()),
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
        fs::write(module.join("module.prop"), format!("id={id}\n")).unwrap();
        fs::write(module.join(format!("{stage}.sh")), body).unwrap();
    }

    fn mark(root: &Path, id: &str, name: &str) {
        fs::write(root.join(id).join(name), b"").unwrap();
    }

    fn run(root: &Path, stage: &str, timeout: Duration, recovery: bool) -> Result<()> {
        scripts_in(
            root,
            &["first".into(), "second".into()],
            stage,
            &["/bin/sh"],
            Some(("rom2", 2)),
            timeout,
            recovery,
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
    fn optional_failure_is_best_effort_and_later_modules_run() {
        let root = tempfile::tempdir().unwrap();
        script(root.path(), "first", "early", "exit 17\n");
        script(root.path(), "second", "early", "echo ran > ../later\n");
        run(root.path(), "early", Duration::from_secs(1), false).unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("later")).unwrap(),
            "ran\n"
        );
    }

    #[test]
    fn critical_failure_stops_normal_android_and_classifies_the_error() {
        let root = tempfile::tempdir().unwrap();
        script(root.path(), "first", "early", "exit 17\n");
        script(root.path(), "second", "early", "echo ran > ../later\n");
        mark(root.path(), "first", "critical");
        let error = run(root.path(), "early", Duration::from_secs(1), false).unwrap_err();
        assert!(is_critical_failure(&error));
        assert!(is_critical_failure(&error.context("lifecycle caller")));
        assert!(!root.path().join("later").exists());
    }

    #[test]
    fn recovery_logs_critical_failures_and_keeps_running_modules() {
        let root = tempfile::tempdir().unwrap();
        script(root.path(), "first", "early", "exit 17\n");
        script(root.path(), "second", "early", "echo ran > ../later\n");
        for id in ["first", "second"] {
            mark(root.path(), id, "recovery-ok");
        }
        mark(root.path(), "first", "critical");
        run(root.path(), "early", Duration::from_secs(1), true).unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("later")).unwrap(),
            "ran\n"
        );
    }

    #[test]
    fn an_unidentifiable_module_runs_no_script() {
        for prop in ["id=other\n", ""] {
            let root = tempfile::tempdir().unwrap();
            for id in ["first", "second"] {
                script(
                    root.path(),
                    id,
                    "early",
                    &format!("echo {id} > ../{id}-ran\n"),
                );
            }
            fs::write(root.path().join("first/module.prop"), prop).unwrap();
            // Optional: reported and skipped while the next module still runs.
            run(root.path(), "early", Duration::from_secs(1), false).unwrap();
            assert!(!root.path().join("first-ran").exists(), "{prop:?}");
            assert!(root.path().join("second-ran").exists(), "{prop:?}");

            // Critical: normal Android stops instead.
            mark(root.path(), "first", "critical");
            fs::remove_file(root.path().join("second-ran")).unwrap();
            let error = run(root.path(), "early", Duration::from_secs(1), false).unwrap_err();
            assert!(is_critical_failure(&error), "{prop:?}");
            assert!(!root.path().join("first-ran").exists(), "{prop:?}");
            for id in ["first", "second"] {
                mark(root.path(), id, "recovery-ok");
            }
            run(root.path(), "early", Duration::from_secs(1), true).unwrap();
            assert!(!root.path().join("first-ran").exists(), "{prop:?}");
            assert_eq!(await_file(&root.path().join("second-ran")), "second\n");
        }
    }

    #[test]
    fn disable_and_remove_markers_skip_module_scripts() {
        let root = tempfile::tempdir().unwrap();
        script(root.path(), "first", "early", "echo first > ../first-ran\n");
        script(
            root.path(),
            "second",
            "early",
            "echo second > ../second-ran\n",
        );
        mark(root.path(), "first", "disable");
        mark(root.path(), "second", "remove");
        run(root.path(), "early", Duration::from_secs(1), false).unwrap();
        assert!(!root.path().join("first-ran").exists());
        assert!(!root.path().join("second-ran").exists());
    }

    #[test]
    fn recovery_only_runs_modules_admitted_by_recovery_ok() {
        let root = tempfile::tempdir().unwrap();
        script(root.path(), "first", "early", "echo first > ../first-ran\n");
        script(
            root.path(),
            "second",
            "early",
            "echo second > ../second-ran\n",
        );
        mark(root.path(), "second", "recovery-ok");
        run(root.path(), "early", Duration::from_secs(1), true).unwrap();
        assert!(!root.path().join("first-ran").exists());
        assert_eq!(
            fs::read_to_string(root.path().join("second-ran")).unwrap(),
            "second\n"
        );
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
            run(root.path(), "post-fs-data", Duration::from_secs(1), false).unwrap();
        }
        assert_eq!(
            fs::read_to_string(root.path().join("later")).unwrap(),
            "second:rom2:2\nsecond:rom2:2\n"
        );
    }

    #[test]
    fn early_and_post_fs_data_share_one_bounded_stage_deadline() {
        for stage in ["early", "post-fs-data"] {
            let root = tempfile::tempdir().unwrap();
            script(
                root.path(),
                "first",
                stage,
                "echo $$ > ../pid\nwhile :; do sleep 1; done\n",
            );
            script(root.path(), "second", stage, "echo unexpected > ../later\n");
            let start = Instant::now();
            run(root.path(), stage, Duration::from_millis(200), false).unwrap();
            assert!(start.elapsed() < Duration::from_secs(2), "{stage}");
            assert!(!root.path().join("later").exists(), "{stage}");
            let pid: i32 = await_file(&root.path().join("pid")).trim().parse().unwrap();
            // The timed-out shell has been killed and reaped, not abandoned.
            // SAFETY: this is our fixture child's PID; a null status pointer is permitted.
            assert_eq!(
                unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
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
            run(root.path(), stage, Duration::from_millis(50), false).unwrap();
            assert!(start.elapsed() < Duration::from_millis(750));
            let pid: i32 = await_file(&root.path().join("pid")).trim().parse().unwrap();
            // SAFETY: getpgid takes only the fixture child's process ID.
            assert_eq!(unsafe { libc::getpgid(pid) }, pid);
            assert_eq!(await_file(&root.path().join("later")), "launched\n");
            assert_eq!(await_file(&root.path().join("finished")), "finished\n");
            for name in ["pid", "second-pid"] {
                let pid: i32 = await_file(&root.path().join(name)).trim().parse().unwrap();
                // SAFETY: these are our child PIDs; waitpid permits a null status pointer.
                assert_eq!(unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) }, pid);
            }
        }
    }
}
