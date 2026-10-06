//! Payload self-checks.
//!
//! The core module is checked through the esu control interface (UAPI v3):
//! ABI compatibility, exact generation equality, and completed initialization.
//! Every later module self-reports through its read-only sysfs parameters
//! `generation` and `ready`, so a successful load alone is never treated as
//! proof of compatibility.

use std::fs;
use std::path::Path;

use crate::config::PartitionModes;
use crate::loader;
use crate::receipt::{Failure, Stage};

/// Verify the core module through the v3 control interface.
///
/// The readiness bit is derived from the kernel's own module state, so it is
/// already set once `finit_module`/`init_module` returns success: a single
/// ioctl is sufficient and there is no polling loop.
pub fn check_core(generation: &str) -> Result<(), Failure> {
    let info = crate::query_core_info().map_err(|error| {
        Failure::at(
            Stage::ModuleCheck,
            Some("kernelesp"),
            "CoreInfoUnavailable",
            format!("{error:#}"),
        )
    })?;

    if info.uapi_version != crate::UAPI_VERSION {
        return Err(Failure::at(
            Stage::ModuleCheck,
            Some("kernelesp"),
            "CoreAbiMismatch",
            format!(
                "core reports UAPI version {}, expected {}",
                info.uapi_version,
                crate::UAPI_VERSION
            ),
        ));
    }

    let reported = info.generation().ok_or_else(|| {
        Failure::at(
            Stage::Generation,
            Some("kernelesp"),
            "CoreGenerationInvalid",
            "core generation is not NUL-terminated ASCII",
        )
    })?;

    if reported != generation {
        return Err(Failure::at(
            Stage::Generation,
            Some("kernelesp"),
            "CoreGenerationMismatch",
            format!("core generation {reported} does not match payload generation {generation}"),
        ));
    }

    if !info.ready() {
        return Err(Failure::at(
            Stage::ModuleCheck,
            Some("kernelesp"),
            "CoreNotReady",
            format!("core state {:#x} does not report readiness", info.state),
        ));
    }

    log::info!("Core module esu generation {generation} is ready");
    Ok(())
}

/// Verify one later module through its `generation`/`ready` sysfs parameters.
pub fn check_module(name: &str, generation: &str) -> Result<(), Failure> {
    let base = module_base(name)?;
    check_generation_at(&base, name, generation)?;
    check_readiness_at(&base, name, Stage::ModuleCheck, "ModuleNotReady", None)?;
    log::info!("Module {name} generation {generation} is ready");
    Ok(())
}

/// Verify a module's identity before a consequential activation step. `gpt`
/// uses this immediately after load and before APPLY can publish any view.
pub fn check_module_generation(name: &str, generation: &str) -> Result<(), Failure> {
    let base = module_base(name)?;
    check_generation_at(&base, name, generation)?;
    log::info!("Module {name} generation {generation} matches the payload");
    Ok(())
}

/// Verify projection readiness after APPLY. Generation was already checked
/// before APPLY, so this check observes only activation state.
pub fn check_projection_ready(name: &str, modes: PartitionModes) -> Result<(), Failure> {
    let base = module_base(name)?;
    check_readiness_at(
        &base,
        name,
        Stage::Projection,
        "ProjectionNotReady",
        Some(modes),
    )?;
    log::info!("Projection module {name} is ready");
    Ok(())
}

fn module_base(name: &str) -> Result<std::path::PathBuf, Failure> {
    loader::module_sysfs_path(name).ok_or_else(|| {
        Failure::at(
            Stage::ModuleCheck,
            Some(name),
            "ModuleSelfCheckMissing",
            format!("/sys/module/{name} is absent after loading"),
        )
    })
}

fn check_generation_at(base: &Path, name: &str, generation: &str) -> Result<(), Failure> {
    let reported = read_parameter(base, "generation", name)?;

    if reported != generation {
        return Err(Failure::at(
            Stage::Generation,
            Some(name),
            "ModuleGenerationMismatch",
            format!("module generation {reported} does not match payload generation {generation}"),
        ));
    }

    Ok(())
}

fn check_readiness_at(
    base: &Path,
    name: &str,
    readiness_stage: Stage,
    readiness_error: &'static str,
    modes: Option<PartitionModes>,
) -> Result<(), Failure> {
    let ready = read_parameter(base, "ready", name)?;

    if !is_ready(&ready) {
        let requested = match modes {
            Some(modes) => format!(" ({modes})"),
            None => String::new(),
        };

        return Err(Failure::at(
            readiness_stage,
            Some(name),
            readiness_error,
            format!("module readiness parameter is {ready:?}{requested}"),
        ));
    }

    Ok(())
}

/// Read one sysfs module parameter, rejecting a missing parameter.
fn read_parameter(base: &Path, parameter: &str, name: &str) -> Result<String, Failure> {
    let path = base.join("parameters").join(parameter);

    let value = fs::read_to_string(&path).map_err(|error| {
        Failure::at(
            Stage::ModuleCheck,
            Some(name),
            "ModuleSelfCheckMissing",
            format!("cannot read {}: {error}", path.display()),
        )
    })?;

    Ok(value
        .trim_matches(|character: char| character.is_whitespace() || character == '\0')
        .to_owned())
}

/// Readiness accepts the documented `Y`/`1` spellings.
fn is_ready(value: &str) -> bool {
    matches!(value, "Y" | "y" | "1")
}
