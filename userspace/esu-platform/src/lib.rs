// SPDX-License-Identifier: GPL-3.0-only
//! Shared ESP identity and safe file access.
pub mod bcb;
pub mod block;
pub mod core;
pub mod efivars;
pub mod stage;

use anyhow::{Context, Result, bail, ensure};
use std::ffi::CString;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Component, Path};

pub fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 64
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "invalid identifier {value:?}"
    );
    Ok(())
}

pub fn relative(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 4096,
        "invalid path length"
    );
    for part in value.split('/') {
        identifier(part).with_context(|| format!("unsafe relative path {value:?}"))?;
    }
    Ok(())
}

/// Open every component without following symlinks, including root ancestors.
pub fn open_root(path: &Path) -> Result<File> {
    ensure!(path.is_absolute(), "root must be absolute");
    let mut directory = File::open("/")?;
    for component in path.components() {
        match component {
            Component::RootDir => (),
            Component::Normal(name) => {
                directory = open_at(&directory, name.to_str().context("non-UTF8 root")?, true)?;
            }
            _ => bail!("non-normal root component"),
        }
    }
    Ok(directory)
}

fn open_at(directory: &File, name: &str, is_dir: bool) -> Result<File> {
    let name = CString::new(name)?;
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if is_dir { libc::O_DIRECTORY } else { 0 };
    // SAFETY: live dirfd and NUL-terminated one-component name.
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    ensure!(
        fd >= 0,
        "openat failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: successful openat transfers ownership.
    let file = unsafe { File::from_raw_fd(fd) };
    ensure!(
        if is_dir {
            file.metadata()?.is_dir()
        } else {
            file.metadata()?.is_file()
        },
        "unexpected inode type"
    );
    Ok(file)
}

pub fn open_file(root: &File, path: &str) -> Result<File> {
    relative(path)?;
    let mut directory = root.try_clone()?;
    let mut components = path.split('/').peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            return open_at(&directory, component, false).with_context(|| path.to_owned());
        }
        directory = open_at(&directory, component, true)?;
    }
    bail!("empty path")
}
