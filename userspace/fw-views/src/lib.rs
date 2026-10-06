// SPDX-License-Identifier: GPL-3.0-only
//! Shared library of the `fw-views` payload helper.
//!
//! The binary in `src/main.rs` performs the device work; [`plan`] derives the
//! per-ROM view names, reserved thin ids and table text from the validated ROM
//! config, so the host tests exercise exactly the values the payload applies.

pub mod plan;
