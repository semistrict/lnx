use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result};

#[cfg(target_os = "macos")]
use std::{ffi::CString, os::unix::ffi::OsStrExt};
#[cfg(target_os = "linux")]
use std::{
    io::{Read, Seek, SeekFrom, Write},
    os::fd::AsRawFd,
};

const LARGE_SPARSE_IMAGE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Clone or sparsely copy `src` to `dst`, preserving VM-image sparseness.
///
/// Large VM images must not silently degrade to dense copies. When Linux cannot
/// provide a reflink, the fallback copies only data extents and verifies the
/// result is still sparse-safe.
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
            preserve_file_metadata(&metadata, dst)?;
            return Ok(());
        }
        fs::remove_file(dst).with_context(|| format!("remove dense clone {}", dst.display()))?;
        anyhow::bail!("clonefile produced dense clone {}", dst.display());
    }

    #[cfg(target_os = "linux")]
    {
        let src_file = fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
        let dst_file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(dst)
            .with_context(|| format!("create {}", dst.display()))?;
        const FICLONE: libc::Ioctl = 0x4004_9409;
        if unsafe { libc::ioctl(dst_file.as_raw_fd(), FICLONE, src_file.as_raw_fd()) } != 0 {
            let error = std::io::Error::last_os_error();
            if !ficlone_can_fall_back(&error) {
                return Err(error)
                    .with_context(|| format!("FICLONE {} to {}", src.display(), dst.display()));
            }
            drop(dst_file);
            fs::remove_file(dst)
                .with_context(|| format!("remove partial clone {}", dst.display()))?;
            copy_without_dense_large_image(src, dst, len)
                .with_context(|| format!("sparse-copy {} to {}", src.display(), dst.display()))?;
            preserve_file_metadata(&metadata, dst)?;
            return Ok(());
        }
        if cloned_file_is_sparse_safe(&dst_file, len)? {
            drop(dst_file);
            preserve_file_metadata(&metadata, dst)?;
            return Ok(());
        }
        drop(dst_file);
        fs::remove_file(dst).with_context(|| format!("remove dense clone {}", dst.display()))?;
        anyhow::bail!("FICLONE produced dense clone {}", dst.display());
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    anyhow::bail!("file clone is unsupported on this platform")
}

fn preserve_file_metadata(metadata: &fs::Metadata, dst: &Path) -> Result<()> {
    let file = fs::OpenOptions::new()
        .read(true)
        .open(dst)
        .with_context(|| format!("open cloned file {} for timestamp restore", dst.display()))?;
    let mut times = fs::FileTimes::new();
    if let Ok(accessed) = metadata.accessed() {
        times = times.set_accessed(accessed);
    }
    if let Ok(modified) = metadata.modified() {
        times = times.set_modified(modified);
    }
    file.set_times(times)
        .with_context(|| format!("restore timestamps on {}", dst.display()))?;
    drop(file);
    fs::set_permissions(dst, metadata.permissions())
        .with_context(|| format!("restore permissions on {}", dst.display()))
}

#[cfg(target_os = "linux")]
fn ficlone_can_fall_back(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL | libc::ENOTTY)
    )
}

#[cfg(target_os = "linux")]
fn copy_without_dense_large_image(src: &Path, dst: &Path, len: u64) -> Result<()> {
    if len < LARGE_SPARSE_IMAGE_BYTES {
        fs::copy(src, dst)
            .with_context(|| format!("copy {} to {}", src.display(), dst.display()))?;
        return ensure_copied_file_is_sparse_safe(dst, len);
    }

    copy_sparse_extents(src, dst, len)?;
    ensure_copied_file_is_sparse_safe(dst, len)
}

#[cfg(target_os = "linux")]
fn copy_sparse_extents(src: &Path, dst: &Path, len: u64) -> Result<()> {
    let mut src_file = fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
    let mut dst_file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(dst)
        .with_context(|| format!("create {}", dst.display()))?;
    dst_file
        .set_len(len)
        .with_context(|| format!("set length {}", dst.display()))?;

    let src_fd = src_file.as_raw_fd();
    let mut offset = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    while offset < len {
        let data = seek_data_or_hole(src_fd, offset, libc::SEEK_DATA, src)?;
        let Some(data) = data else {
            break;
        };
        let hole = seek_data_or_hole(src_fd, data, libc::SEEK_HOLE, src)?.unwrap_or(len);
        let end = hole.min(len);
        copy_range(&mut src_file, &mut dst_file, data, end, &mut buffer)?;
        offset = end;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn seek_data_or_hole(fd: i32, offset: u64, whence: i32, src: &Path) -> Result<Option<u64>> {
    let result = unsafe { libc::lseek(fd, offset as libc::off_t, whence) };
    if result >= 0 {
        return Ok(Some(result as u64));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENXIO) {
        return Ok(None);
    }
    Err(error).with_context(|| format!("seek sparse extent in {}", src.display()))
}

#[cfg(target_os = "linux")]
fn copy_range(
    src: &mut fs::File,
    dst: &mut fs::File,
    start: u64,
    end: u64,
    buffer: &mut [u8],
) -> Result<()> {
    src.seek(SeekFrom::Start(start))?;
    dst.seek(SeekFrom::Start(start))?;
    let mut remaining = end - start;
    while remaining > 0 {
        let limit = remaining.min(buffer.len() as u64) as usize;
        src.read_exact(&mut buffer[..limit])?;
        dst.write_all(&buffer[..limit])?;
        remaining -= limit as u64;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn ensure_copied_file_is_sparse_safe(path: &Path, len: u64) -> Result<()> {
    let allocated = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .blocks()
        * 512;
    if clone_is_sparse_safe(len, allocated) {
        return Ok(());
    }
    fs::remove_file(path).with_context(|| format!("remove dense copy {}", path.display()))?;
    anyhow::bail!("copy produced dense VM image {}", path.display())
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
