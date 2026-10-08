//! esu boot-control HAL layer over the generic-bootctl submodule.
//!
//! `backend` adapts esu's project efivarfs records and misc VAB mirror to the
//! shared `generic_bootctl_core::Backend`, and `wire` owns those native byte
//! layouts. Slot health policy, the frozen AIDL V1 dispatch and every Binder
//! transport are provided by `generic-bootctl-core` and the shared `bootctl-unified` process
//! (`../generic-bootctl`, a pinned submodule).
pub mod backend;
pub mod stock;
pub mod wire;
