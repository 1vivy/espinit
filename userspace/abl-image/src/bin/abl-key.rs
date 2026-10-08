//! Host-only extraction of the stock ABL's AVB anchor key.
//!
//! `abl-key <abl.img> [--out <file>]` extracts the LinuxLoader PE and writes its
//! first embedded AVB public key (a raw `AvbRSAPublicKeyHeader` blob) to `--out`
//! or stdout. Stock `AvbValidateVbmetaPublicKey` trusts only that first key. The
//! input image, LinuxLoader and key SHA-256 values go to stderr. Hashing shells
//! out to `openssl dgst`, like `tools/profile-tool`; no key is written when the
//! LinuxLoader or its embedded key is absent.
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn usage() -> ! {
    eprintln!("usage: abl-key <abl.img> [--out <file>]");
    std::process::exit(2)
}

fn read_bounded(path: &Path) -> std::io::Result<Vec<u8>> {
    let limit = u64::try_from(abl_image::MAX_INPUT).unwrap_or(u64::MAX);
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > abl_image::MAX_INPUT {
        return Err(std::io::Error::other("ABL image exceeds 32 MiB"));
    }
    Ok(bytes)
}

fn sha256(data: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
    let mut child = Command::new("openssl")
        .args(["dgst", "-sha256", "-binary"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("missing digest pipe")?;
    stdin.write_all(data)?;
    drop(stdin);
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err("openssl digest failed".into());
    }
    Ok(output
        .stdout
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (input, out) = match args.as_slice() {
        [input] => (PathBuf::from(input), None),
        [input, flag, out] if flag == "--out" => (PathBuf::from(input), Some(PathBuf::from(out))),
        _ => usage(),
    };
    let image = read_bounded(&input)?;
    let loader = abl_image::extract_linuxloader(&image)?;
    let key = abl_image::scan_avb_keys(&loader)
        .into_iter()
        .next()
        .ok_or("ABL LinuxLoader has no embedded AVB public key")?;
    eprintln!("abl_image_sha256={}", sha256(&image)?);
    eprintln!("linuxloader_sha256={}", sha256(&loader)?);
    eprintln!("key_sha256={}", sha256(&key)?);
    match &out {
        Some(path) => std::fs::write(path, &key)?,
        None => std::io::stdout().write_all(&key)?,
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("abl-key failed error={error}");
        std::process::exit(1);
    }
}
