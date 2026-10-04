//! Projection control through `/dev/gptctl`.
//!
//! `gpt.ko` publishes one control node and one APPLY/QUERY ioctl pair. This
//! module builds the APPLY payload from the validated configuration, issues the
//! single atomic APPLY, then QUERYs the applied state and requires an exact
//! match. There is no partial projection and no retry with a different payload:
//! a rejected APPLY or a mismatched QUERY stops the managed boot.
//!
//! The caller keeps every resolved backend alive until APPLY has returned, so
//! the kernel's lower opens find the backing devices, and the loop attachments
//! are CLOEXEC plus autoclear so they survive until Android opens them and are
//! torn down afterwards.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;

use rustix::fs::{major, minor};
use syscalls::{Sysno, syscall};

use crate::block::ResolvedBackend;
use crate::config::PartitionEntry;
use crate::gpt_uapi::{GptApply, GptDevice, GptQuery};
use crate::receipt::{Failure, Stage};

/// Control node published by the loaded `gpt` module.
const GPT_CONTROL: &str = "/dev/gptctl";

/// Build the exact APPLY payload from the validated configuration. Every bound
/// of the ABI is enforced here, the reserved fields and the unused array slots
/// stay zero, and the payload is never built from an unvalidated source.
pub fn build_apply(
    partitions: &[PartitionEntry],
    backends: &[ResolvedBackend],
    hide: &[GptDevice],
) -> Result<GptApply, String> {
    if partitions.len() != backends.len() {
        return Err(format!(
            "{} projections but {} resolved backends",
            partitions.len(),
            backends.len()
        ));
    }

    if partitions.is_empty() {
        return Err("an APPLY requires at least one projection".to_owned());
    }

    if partitions.len() > crate::gpt_uapi::GPT_MAX_PROJECTIONS {
        return Err(format!(
            "{} projections exceed the limit {}",
            partitions.len(),
            crate::gpt_uapi::GPT_MAX_PROJECTIONS
        ));
    }

    if hide.len() > crate::gpt_uapi::GPT_MAX_HIDDEN {
        return Err(format!(
            "{} hidden partitions exceed the limit {}",
            hide.len(),
            crate::gpt_uapi::GPT_MAX_HIDDEN
        ));
    }

    let mut apply = GptApply {
        version: crate::gpt_uapi::GPT_ABI_VERSION,
        count: partitions.len() as u32,
        hide_count: hide.len() as u32,
        ..GptApply::default()
    };

    for (index, (partition, backend)) in partitions.iter().zip(backends).enumerate() {
        let Some(name) = crate::gpt_uapi::label_bytes(&partition.name) else {
            return Err(format!(
                "projected label {:?} is not a valid ABI label",
                partition.name
            ));
        };

        apply.projections[index] = crate::gpt_uapi::GptProjection {
            major: major(backend.rdev),
            minor: minor(backend.rdev),
            read_only: u8::from(partition.read_only),
            name,
            ..crate::gpt_uapi::GptProjection::default()
        };
    }

    apply.hide[..hide.len()].copy_from_slice(hide);

    Ok(apply)
}

/// Verify the reply of a successful APPLY: the exact ABI version, an active
/// view and the exact projection count. A mixed view, a stale view and a
/// partially published view are all rejected.
pub fn verify_query(query: &GptQuery, expected: usize) -> Result<(), String> {
    if query.version != crate::gpt_uapi::GPT_ABI_VERSION {
        return Err(format!(
            "gpt reports ABI version {}, expected {}",
            query.version,
            crate::gpt_uapi::GPT_ABI_VERSION
        ));
    }

    if query.active != 1 {
        return Err(format!(
            "gpt view is not active (active = {})",
            query.active
        ));
    }

    if query.count as usize != expected {
        return Err(format!(
            "gpt view projection count {} does not match {expected}",
            query.count
        ));
    }

    Ok(())
}

/// Issue one APPLY, then QUERY the applied state. The exact APPLY payload is
/// copied by the kernel before it can return, so the applied view is either
/// complete or absent.
pub fn apply_payload(payload: &GptApply) -> Result<GptQuery, Failure> {
    let control = control()?;

    apply_ioctl(&control, payload)?;

    let mut query = GptQuery::default();

    query_ioctl(&control, &mut query)?;

    Ok(query)
}

/// Open the projection control node.
fn control() -> Result<File, Failure> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(GPT_CONTROL)
        .map_err(control_error)
}

/// Classify an unavailable control node.
fn control_error(error: io::Error) -> Failure {
    Failure::new(
        Stage::Projection,
        "GptControlUnavailable",
        format!("cannot open {GPT_CONTROL}: {error}"),
    )
}

/// Issue the single atomic APPLY ioctl.
fn apply_ioctl(control: &File, apply: &GptApply) -> Result<(), Failure> {
    let result = unsafe {
        syscall!(
            Sysno::ioctl,
            control.as_raw_fd(),
            crate::gpt_uapi::GPT_IOCTL_APPLY,
            apply as *const GptApply
        )
    };

    match result {
        Ok(_) => Ok(()),
        Err(errno) => Err(Failure::new(
            Stage::Projection,
            "ProjectionApplyRejected",
            format!("{GPT_CONTROL} APPLY failed: errno {}", errno.into_raw()),
        )),
    }
}

/// Query the applied state into `query`.
fn query_ioctl(control: &File, query: &mut GptQuery) -> Result<(), Failure> {
    let result = unsafe {
        syscall!(
            Sysno::ioctl,
            control.as_raw_fd(),
            crate::gpt_uapi::GPT_IOCTL_QUERY,
            query as *mut GptQuery
        )
    };

    match result {
        Ok(_) => Ok(()),
        Err(errno) => Err(Failure::new(
            Stage::Projection,
            "ProjectionQueryRejected",
            format!("{GPT_CONTROL} QUERY failed: errno {}", errno.into_raw()),
        )),
    }
}

/// Build, apply and verify the complete projection as one atomic step. Any
/// failure leaves the boot stopped: there is no fallback projection and no
/// retry with a reduced payload.
pub fn project(
    partitions: &[PartitionEntry],
    backends: &[ResolvedBackend],
    hide: &[GptDevice],
) -> Result<(), Failure> {
    let payload = match build_apply(partitions, backends, hide) {
        Ok(payload) => payload,
        Err(detail) => {
            return Err(Failure::new(
                Stage::Projection,
                "ProjectionPayloadInvalid",
                detail,
            ));
        }
    };

    let query = apply_payload(&payload)?;

    if let Err(detail) = verify_query(&query, partitions.len()) {
        return Err(Failure::new(
            Stage::Projection,
            "ProjectionQueryMismatch",
            detail,
        ));
    }

    log::info!(
        "Applied {} projections with {} hidden physical partitions",
        partitions.len(),
        hide.len()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn partition(name: &str, read_only: bool) -> PartitionEntry {
        PartitionEntry {
            name: name.to_owned(),
            backend: "/dev/block/by-name/system".to_owned(),
            read_only,
        }
    }

    fn backend(path: &str, rdev: u64) -> ResolvedBackend {
        ResolvedBackend {
            path: path.to_owned(),
            rdev,
            guard: None,
        }
    }

    #[test]
    fn apply_payload_carries_exact_devices_labels_and_zero_tail() {
        let partitions = [partition("super", false), partition("vbmeta_system", true)];
        let backends = [
            backend("/dev/espinit/backends/sda1", 0x0801),
            backend("/dev/loop3", 0x0703),
        ];
        let hide = [
            GptDevice { major: 8, minor: 2 },
            GptDevice { major: 8, minor: 1 },
        ];

        let apply = build_apply(&partitions, &backends, &hide).unwrap();

        assert_eq!(apply.version, crate::gpt_uapi::GPT_ABI_VERSION);
        assert_eq!(apply.count, 2);
        assert_eq!(apply.hide_count, 2);
        assert_eq!(apply.flags, 0);
        assert_eq!(apply.projections[0].major, 8);
        assert_eq!(apply.projections[0].minor, 1);
        assert_eq!(apply.projections[0].read_only, 0);
        assert_eq!(
            crate::gpt_uapi::projection_label(&apply.projections[0]).as_deref(),
            Some("super")
        );
        assert_eq!(apply.projections[1].major, 7);
        assert_eq!(apply.projections[1].minor, 3);
        assert_eq!(apply.projections[1].read_only, 1);
        assert_eq!(
            crate::gpt_uapi::projection_label(&apply.projections[1]).as_deref(),
            Some("vbmeta_system")
        );
        for (slot, device) in apply.hide[..hide.len()].iter().zip(&hide) {
            assert_eq!(slot, device);
        }

        // Every unused slot must stay all-zero: the kernel validates the whole
        // payload, not only the active prefix.
        for projection in &apply.projections[2..] {
            assert_eq!(*projection, crate::gpt_uapi::GptProjection::default());
        }
        for device in &apply.hide[2..] {
            assert_eq!(*device, GptDevice::default());
        }
    }

    #[test]
    fn apply_payload_rejects_mismatched_or_oversized_inputs() {
        let partitions = [partition("system", true)];
        let backends = [
            backend("/dev/espinit/backends/sda1", 0x0801),
            backend("/dev/espinit/backends/sda2", 0x0802),
        ];

        assert!(build_apply(&partitions, &backends, &[]).is_err());
        assert!(build_apply(&[], &[], &[]).is_err());

        let many: Vec<PartitionEntry> = (0..=crate::gpt_uapi::GPT_MAX_PROJECTIONS)
            .map(|index| partition(&format!("p{index}"), true))
            .collect();
        let many_backends: Vec<ResolvedBackend> = (0..=crate::gpt_uapi::GPT_MAX_PROJECTIONS)
            .map(|index| backend("/dev/loop0", 0x0700 + index as u64))
            .collect();
        assert!(build_apply(&many, &many_backends, &[]).is_err());

        let hide = vec![GptDevice::default(); crate::gpt_uapi::GPT_MAX_HIDDEN + 1];
        assert!(build_apply(&partitions, &backends[..1], &hide).is_err());

        // A label outside the ABI is rejected before any ioctl is attempted.
        let long = [partition(
            &"p".repeat(crate::gpt_uapi::GPT_LABEL_BYTES + 1),
            true,
        )];
        let long_backend = [backend("/dev/loop0", 0x0700)];
        assert!(build_apply(&long, &long_backend, &[]).is_err());
    }

    #[test]
    fn query_must_report_the_exact_abi_active_and_count() {
        let good = GptQuery {
            version: crate::gpt_uapi::GPT_ABI_VERSION,
            active: 1,
            count: 2,
            reserved: 0,
        };
        verify_query(&good, 2).unwrap();

        for query in [
            GptQuery { version: 2, ..good },
            GptQuery { active: 0, ..good },
            GptQuery { active: 2, ..good },
            GptQuery { count: 1, ..good },
            GptQuery { count: 3, ..good },
        ] {
            assert!(verify_query(&query, 2).is_err());
        }
    }
}
