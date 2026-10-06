use std::path::{Path, PathBuf};

/// Longest generation accepted here: the value must survive as a
/// NUL-terminated ASCII string in the UAPI `generation[64]` field, so it may
/// occupy at most 63 bytes before the terminator. The manifest accepts up to
/// 64 bytes per the layout specification; a 64-byte manifest generation can
/// therefore never match a build generation and is reported as a mismatch at
/// the generation stage instead of being silently truncated.
const MAX_GENERATION_BYTES: usize = 63;

fn main() {
    // Fix getauxval linking issue for aarch64-unknown-linux-musl
    // The compiler_builtins crate needs getauxval from libc, but due to link order
    // issues, we need to link libc again at the end
    let target = std::env::var("TARGET").unwrap();

    if target == "aarch64-unknown-linux-musl" || target == "x86_64-unknown-linux-musl" {
        // Link libc at the end to resolve symbols from compiler_builtins
        println!("cargo:rustc-link-arg=-lc");
    }

    emit_generation();
}

/// Derive the payload generation from `ESU_GENERATION` when set, otherwise
/// from the repository's full 40-byte lowercase Git HEAD hash, and export it to
/// the crate as `env!("ESU_GENERATION")`. Both the kernel build and the
/// userspace crates must derive the same value; a release pipeline sets the
/// variable explicitly later.
fn emit_generation() {
    println!("cargo:rerun-if-env-changed=ESU_GENERATION");

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    track_git_inputs(&manifest_dir);

    let generation = match std::env::var("ESU_GENERATION") {
        Ok(value) if !value.is_empty() => value,
        Ok(_) => {
            panic!("ESU_GENERATION is set but empty; unset it to derive the full Git HEAD hash")
        }
        Err(_) => git_head(&manifest_dir),
    };

    if let Err(reason) = validate_generation(&generation) {
        panic!("invalid generation {generation:?}: {reason}");
    }

    println!("cargo:rustc-env=ESU_GENERATION={generation}");
}

/// Emit the Git inputs that change when HEAD moves, so a new commit rebuilds
/// the crate even though no Rust source changed.
fn track_git_inputs(manifest_dir: &Path) {
    let Some(git_dir) = find_git_dir(manifest_dir) else {
        return;
    };

    let head = git_dir.join("HEAD");
    println!("cargo:rerun-if-changed={}", head.display());

    if let Ok(head) = std::fs::read_to_string(&head)
        && let Some(reference) = head.strip_prefix("ref:")
    {
        let reference = reference.trim();
        let reference_path = git_dir.join(reference);
        println!("cargo:rerun-if-changed={}", reference_path.display());
        if let Some(parent) = reference_path.parent() {
            println!("cargo:rerun-if-changed={}", parent.display());
        }
    }

    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("packed-refs").display()
    );
}

/// Locate the `.git` directory for the workspace containing the crate,
/// handling a worktree where `.git` is a file containing `gitdir: <path>`.
fn find_git_dir(manifest_dir: &Path) -> Option<PathBuf> {
    let mut current = manifest_dir;

    loop {
        let candidate = current.join(".git");

        if candidate.is_dir() {
            return Some(candidate);
        }

        if candidate.is_file() {
            let contents = std::fs::read_to_string(&candidate).ok()?;
            let path = contents.strip_prefix("gitdir:")?.trim();
            let resolved = current.join(path);
            return Some(resolved);
        }

        current = current.parent()?;
    }
}

fn git_head(manifest_dir: &Path) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(manifest_dir)
        .args(["rev-parse", "HEAD"])
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let head = String::from_utf8_lossy(&output.stdout).trim().to_owned();
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
        _ => panic!(
            "cannot derive a generation: ESU_GENERATION is unset and `git rev-parse HEAD` failed; set ESU_GENERATION explicitly when building from a source tarball"
        ),
    }
}

/// Mirror of the manifest generation rule: nonempty ASCII letters/digits plus
/// `.`, `_`, `-`, bounded so it fits the NUL-terminated UAPI field.
fn validate_generation(generation: &str) -> Result<(), &'static str> {
    if generation.is_empty() {
        return Err("generation is empty");
    }

    if generation.len() > MAX_GENERATION_BYTES {
        return Err("generation is longer than 63 bytes");
    }

    if !generation.is_ascii() {
        return Err("generation is not ASCII");
    }

    for byte in generation.bytes() {
        if !byte.is_ascii_alphanumeric() && !matches!(byte, b'.' | b'_' | b'-') {
            return Err("generation contains a character outside [A-Za-z0-9._-]");
        }
    }

    Ok(())
}
