//! esu boot-control HAL: a thin consumer of the vendored generic-bootctl core.
//!
//! `backend` adapts esu's project efivarfs records and misc VAB mirror to the
//! shared `generic_bootctl_core::Backend`, and `wire` owns those native byte
//! layouts. Slot health policy, the frozen AIDL V1 dispatch and the Binder
//! transport are provided by `generic-bootctl-core` and `generic-bootctl-aidl`
//! (`vendor/generic-bootctl`, see `PROVENANCE.md`).
pub mod backend;
pub mod wire;
