//! esu boot-control HAL layer over the generic-bootctl submodule.
//!
//! `backend` adapts esu's project efivarfs records and misc VAB mirror to the
//! shared `generic_bootctl_core::Backend`, `wire` owns those native byte
//! layouts and `txn` is the OTA transaction the same mutations drive: staging
//! sets for a managed ROM, the takeover payload for ROM 1, and the promote that
//! copies the staged bytes into the ROM's base images. Slot health policy, the
//! frozen AIDL V1 dispatch and every Binder transport are provided by
//! `generic-bootctl-core` and the shared `bootctl-unified` process
//! (`../generic-bootctl`, a pinned submodule).
pub mod android;
pub mod backend;
pub mod platform;
pub mod stock;
pub mod txn;
pub mod wire;
