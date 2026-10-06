use std::env;
use std::path::Path;
use std::process::Command;

/// Maximum length of the generation string, excluding its NUL terminator. It
/// must stay in sync with the `generation[64]` field of `struct
/// ksu_get_info_cmd` in uapi/supercall.h and with the validation in
/// kernel/Kbuild.
const GENERATION_MAX_LEN: usize = 63;

fn get_git_version() -> Result<(u32, String), std::io::Error> {
    let output = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .output()?;

    let output = output.stdout;
    let version_code = String::from_utf8(output).expect("Failed to read git count stdout");
    let version_code: u32 = version_code
        .trim()
        .parse()
        .map_err(|_| std::io::Error::other("Failed to parse git count"))?;
    let version_code = 30000 + version_code;

    let version_name = String::from_utf8(
        Command::new("git")
            .args(["describe", "--tags", "--always"])
            .output()?
            .stdout,
    )
    .map_err(|_| std::io::Error::other("Failed to read git describe stdout"))?;
    let version_name = version_name.trim_start_matches('v').to_string();
    Ok((version_code, version_name))
}

/// Run a git command in the esu repository and return its trimmed stdout
/// when the command succeeded and produced something.
fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let stdout = stdout.trim().to_string();
    (!stdout.is_empty()).then_some(stdout)
}

fn is_generation_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')
}

/// Reject a generation that must not be shipped. The value is embedded into a
/// fixed-width C field and into matching kernel/module builds, so a bad value
/// must fail the build instead of being truncated or mangled.
fn validate_generation(value: &str, source: &str) {
    if value.is_empty() {
        panic!("esu {source} is empty, but the generation must not be empty");
    }
    if value.len() > GENERATION_MAX_LEN {
        panic!(
            "esu {source} is {} bytes long, but a generation may hold at most \
             {GENERATION_MAX_LEN} bytes; refusing to truncate it",
            value.len()
        );
    }
    if let Some(ch) = value.chars().find(|ch| !is_generation_char(*ch)) {
        panic!(
            "esu {source} contains {ch:?}, but a generation must be ASCII \
             letters/digits or one of . _ -"
        );
    }
}

/// The generation shared by the PID-1 stage, the core kernel module, every ESP
/// module and this daemon. `ESU_GENERATION` wins when set, otherwise the
/// full 40-byte lowercase Git HEAD hash of the esu repository is used.
/// Keep this logic in sync with kernel/Kbuild and userspace/esuinit/build.rs.
fn build_generation() -> String {
    match env::var("ESU_GENERATION") {
        Ok(value) if !value.is_empty() => {
            validate_generation(&value, "ESU_GENERATION");
            value
        }
        Ok(_) => {
            panic!("ESU_GENERATION is set but empty; unset it to derive the full Git HEAD hash")
        }
        Err(_) => {
            let head = git_output(&["rev-parse", "HEAD"]).unwrap_or_else(|| {
                panic!(
                    "cannot derive the esu generation: ESU_GENERATION is unset and \
                     `git rev-parse HEAD` failed or returned no usable output. Set ESU_GENERATION explicitly when building from a source tarball."
                )
            });
            if head.len() != 40
                || !head
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            {
                panic!(
                    "git rev-parse HEAD did not return 40 lowercase hex bytes; set ESU_GENERATION explicitly when building from a source tarball"
                );
            }
            head
        }
    }
}

/// Files whose content changes the generation or the generated UAPI bindings.
/// Cargo reruns this script when one of them changes; without this the daemon
/// could keep a stale generation after a checkout or a UAPI edit.
fn generation_inputs() -> Vec<String> {
    let mut inputs = vec!["src/ksu_uapi.h".to_string(), "../../uapi".to_string()];
    if let Some(git_dir) = git_output(&["rev-parse", "--absolute-git-dir"]) {
        let git_dir = Path::new(&git_dir);
        inputs.push(git_dir.join("HEAD").display().to_string());
        inputs.push(git_dir.join("refs").join("heads").display().to_string());
    }
    inputs
}

fn configure_bindgen() {
    // The bindgen::Builder is the main entry point
    // to bindgen, and lets you build up options for
    // the resulting bindings.
    let mut builder = bindgen::Builder::default()
        // The input header we would like to generate
        // bindings for.
        .header("src/ksu_uapi.h")
        .clang_args(["-x", "c++", "-I../../"])
        // Tell cargo to invalidate the built crate whenever any of the
        // included header files changed.
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("riscv64") {
        // libc does not yet expose Android's RISC-V signal context. Generate
        // it from the target NDK rather than assuming another libc's layout.
        builder = builder.header_contents("ksu_signal_context.h", "#include <sys/ucontext.h>");
    }
    let bindings = builder
        // Finish the builder and generate the bindings.
        .generate()
        // Unwrap the Result and panic on failure.
        .expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/bindings.rs file.
    let out_path = std::path::PathBuf::from(env::var("OUT_DIR").unwrap());
    // for debug, uncomment below
    // let out_path = std::path::PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}

fn main() {
    let (code, name) = match get_git_version() {
        Ok((code, name)) => (code, name),
        Err(_) => {
            // show warning if git is not installed
            println!("cargo:warning=Failed to get git version, using 0.0.0");
            (0, "0.0.0".to_string())
        }
    };
    println!("cargo:rustc-env=VERSION_CODE={code}");
    println!("cargo:rustc-env=VERSION_NAME={name}");

    let generation = build_generation();
    println!("cargo:rustc-env=ESU_GENERATION={generation}");
    println!("cargo:rerun-if-env-changed=ESU_GENERATION");
    for input in generation_inputs() {
        println!("cargo:rerun-if-changed={input}");
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS not set");
    if target_os == "android" {
        configure_bindgen();
    }
}
