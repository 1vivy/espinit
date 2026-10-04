//! Payload self-checks.
//!
//! The core module is checked through the espinit control interface (UAPI v2):
//! ABI compatibility, exact generation equality, and completed initialization.
//! Every later module self-reports through its read-only sysfs parameters
//! `generation` and `ready`, so a successful load alone is never treated as
//! proof of compatibility.

use std::fs;
use std::path::Path;

use crate::config::PartitionModes;
use crate::loader;
use crate::receipt::{Failure, Stage};

/// Verify the core module through the v2 control interface.
///
/// The readiness bit is derived from the kernel's own module state, so it is
/// already set once `finit_module`/`init_module` returns success: a single
/// ioctl is sufficient and there is no polling loop.
pub fn check_core(generation: &str) -> Result<(), Failure> {
    let info = crate::query_core_info().map_err(|error| {
        Failure::at(
            Stage::ModuleCheck,
            Some("espinit"),
            "CoreInfoUnavailable",
            format!("{error:#}"),
        )
    })?;

    if info.uapi_version != crate::UAPI_VERSION {
        return Err(Failure::at(
            Stage::ModuleCheck,
            Some("espinit"),
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
            Some("espinit"),
            "CoreGenerationInvalid",
            "core generation is not NUL-terminated ASCII",
        )
    })?;

    if reported != generation {
        return Err(Failure::at(
            Stage::Generation,
            Some("espinit"),
            "CoreGenerationMismatch",
            format!("core generation {reported} does not match payload generation {generation}"),
        ));
    }

    if !info.ready() {
        return Err(Failure::at(
            Stage::ModuleCheck,
            Some("espinit"),
            "CoreNotReady",
            format!("core state {:#x} does not report readiness", info.state),
        ));
    }

    log::info!("Core module espinit generation {generation} is ready");
    Ok(())
}

/// Verify one later module through its `generation`/`ready` sysfs parameters.
pub fn check_module(name: &str, generation: &str) -> Result<(), Failure> {
    check_module_with(name, generation, Stage::ModuleCheck, "ModuleNotReady", None)
}

/// Verify the projection module; an unready projection is its own failure
/// stage because a partially published view must never reach Android. The
/// requested access-mode counts are carried into readiness diagnostics.
pub fn check_projection(
    name: &str,
    generation: &str,
    modes: PartitionModes,
) -> Result<(), Failure> {
    check_module_with(
        name,
        generation,
        Stage::Projection,
        "ProjectionNotReady",
        Some(modes),
    )
}

fn check_module_with(
    name: &str,
    generation: &str,
    readiness_stage: Stage,
    readiness_error: &'static str,
    modes: Option<PartitionModes>,
) -> Result<(), Failure> {
    let base = loader::module_sysfs_path(name).ok_or_else(|| {
        Failure::at(
            Stage::ModuleCheck,
            Some(name),
            "ModuleSelfCheckMissing",
            format!("/sys/module/{name} is absent after loading"),
        )
    })?;

    let reported = read_parameter(&base, "generation", name)?;

    if reported != generation {
        return Err(Failure::at(
            Stage::Generation,
            Some(name),
            "ModuleGenerationMismatch",
            format!("module generation {reported} does not match payload generation {generation}"),
        ));
    }

    let ready = read_parameter(&base, "ready", name)?;

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

    log::info!("Module {name} generation {generation} is ready");
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
