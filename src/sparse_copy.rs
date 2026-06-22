use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result};

#[cfg(target_os = "macos")]
use std::{ffi::CString, os::unix::ffi::OsStrExt};

const LARGE_SPARSE_IMAGE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Clone `src` to `dst`, preserving sparseness and backing-store sharing.
///
/// This intentionally fails when the filesystem cannot provide a clone. VM
/// images must not silently degrade to independent dense or sparse copies.
pub fn clone_or_copy_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let metadata = fs::metadata(src).with_context(|| format!("stat {}", src.display()))?;
    let len = metadata.len();
    #[cfg(unix)]
    let allocated = metadata.blocks() * 512;
    #[cfg(not(unix))]
    let allocated = len;

    if !clone_is_sparse_safe(len, allocated) {
        anyhow::bail!(
            "source is not sparse-safe to clone: {} len={len} allocated={allocated}",
            src.display()
        );
    }

    #[cfg(target_os = "macos")]
    {
        let _ = fs::remove_file(dst);
        let c_src = CString::new(src.as_os_str().as_bytes())?;
        let c_dst = CString::new(dst.as_os_str().as_bytes())?;
        if unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("clonefile {} to {}", src.display(), dst.display()));
        }
        if cloned_destination_is_sparse_safe(dst, len)? {
            return Ok(());
        }
        fs::remove_file(dst).with_context(|| format!("remove dense clone {}", dst.display()))?;
        anyhow::bail!("clonefile produced dense clone {}", dst.display());
    }

    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        let src_file = fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
        let dst_file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(dst)
            .with_context(|| format!("create {}", dst.display()))?;
        const FICLONE: libc::Ioctl = 0x4004_9409;
        if unsafe { libc::ioctl(dst_file.as_raw_fd(), FICLONE, src_file.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("FICLONE {} to {}", src.display(), dst.display()));
        }
        if cloned_file_is_sparse_safe(&dst_file, len)? {
            return Ok(());
        }
        drop(dst_file);
        fs::remove_file(dst).with_context(|| format!("remove dense clone {}", dst.display()))?;
        anyhow::bail!("FICLONE produced dense clone {}", dst.display());
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    anyhow::bail!("file clone is unsupported on this platform")
}

fn clone_is_sparse_safe(len: u64, allocated: u64) -> bool {
    len < LARGE_SPARSE_IMAGE_BYTES || allocated <= len / 2
}

#[cfg(target_os = "macos")]
fn cloned_destination_is_sparse_safe(path: &Path, len: u64) -> Result<bool> {
    let allocated = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .blocks()
        * 512;
    Ok(clone_is_sparse_safe(len, allocated))
}

#[cfg(target_os = "linux")]
fn cloned_file_is_sparse_safe(file: &fs::File, len: u64) -> Result<bool> {
    let allocated = file.metadata()?.blocks() * 512;
    Ok(clone_is_sparse_safe(len, allocated))
}

#[cfg(test)]
mod tests;
