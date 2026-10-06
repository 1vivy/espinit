use anyhow::{Context, Ok, Result};
use std::ffi::CString;
use std::fs;
use std::path::Path;

pub fn insmod(module: &Path, params: &[String]) -> Result<()> {
    let module = module
        .canonicalize()
        .with_context(|| format!("resolve module path failed: {}", module.display()))?;
    let module_data =
        fs::read(&module).with_context(|| format!("read module failed: {}", module.display()))?;
    let cparams = CString::new(params.join(" "))?;

    esuinit::load_module(&module_data, &cparams)
        .with_context(|| format!("load module failed: {}", module.display()))?;

    println!("Loaded kernel module: {}", module.display());
    Ok(())
}
