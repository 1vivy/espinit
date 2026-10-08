// SPDX-License-Identifier: GPL-3.0-only
//! Shared library of the `ota-stage` PID-1 payload helper.
//!
//! The binary in `src/main.rs` performs the device work; [`plan`] derives the
//! per-base switch device names, tables and access from the declared bases and
//! the `ESU_STAGE` value, so the host tests exercise exactly the values the
//! payload publishes at PID 1.

pub mod plan;
