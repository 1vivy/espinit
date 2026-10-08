//! Frozen V1 NDK binder transport; registration admission belongs to the binary.
#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "android")]
pub use android::run;
