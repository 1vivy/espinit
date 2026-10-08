//! Standalone entrypoint for exercising the production esu relocating loader in a VM.
use std::{ffi::CString, path::PathBuf};

fn run() -> anyhow::Result<()> {
    let mut args = std::env::args();
    let _ = args.next();
    let module = PathBuf::from(
        args.next()
            .ok_or_else(|| anyhow::anyhow!("usage: relocating-insmod MODULE [PARAMETERS...]"))?,
    );
    let params = CString::new(args.collect::<Vec<_>>().join(" "))?;
    esuinit::load_module(&std::fs::read(module)?, &params)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("relocating-insmod: {error:#}");
        std::process::exit(1);
    }
}
