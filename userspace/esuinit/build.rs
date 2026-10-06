fn main() {
    // Resolve compiler_builtins getauxval after the musl libc link inputs.
    let target = std::env::var("TARGET").unwrap();
    if target == "aarch64-unknown-linux-musl" || target == "x86_64-unknown-linux-musl" {
        println!("cargo:rustc-link-arg=-lc");
    }
}
