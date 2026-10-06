#[cfg(target_os = "android")]
mod android;

fn main() {
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
