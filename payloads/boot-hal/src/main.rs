#[cfg(target_os = "android")]
mod android;
#[path = "../../../userspace/esu-platform/src/generation.rs"]
mod generation;

fn main() {
    let _ = generation::generation();
    #[cfg(target_os = "android")]
    if let Err(error) = android::run() {
        eprintln!("boot-hal: {error}");
        std::process::exit(1);
    }
    #[cfg(not(target_os = "android"))]
    {
        eprintln!(
            "boot-hal service requires Android Binder; run cargo test for host state/storage tests"
        );
        std::process::exit(1);
    }
}
