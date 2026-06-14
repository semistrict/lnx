use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result};

#[cfg(target_os = "macos")]
use std::{ffi::CString, os::unix::ffi::OsStrExt};

const LARGE_SPARSE_IMAGE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Copy `src` to `dst`, preserving sparseness.
///
/// Tries an in-kernel reflink clone first, then a SEEK_DATA/SEEK_HOLE extent
/// walk using copy_file_range per data extent. A whole-file copy_file_range
/// is deliberately avoided: on filesystems without reflink support it
/// materializes source holes as allocated zeros. Per-extent copy_file_range
/// matters on virtiofs, which reports the whole file as one data extent but
/// services FUSE_COPY_FILE_RANGE server-side instead of streaming bytes
/// through the guest.
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

    #[cfg(target_os = "macos")]
    {
        if clone_is_sparse_safe(len, allocated) {
            let _ = fs::remove_file(dst);
            let c_src = CString::new(src.as_os_str().as_bytes())?;
            let c_dst = CString::new(dst.as_os_str().as_bytes())?;
            if unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) } == 0 {
                if cloned_destination_is_sparse_safe(dst, len)? {
                    return Ok(());
                }
                fs::remove_file(dst)
                    .with_context(|| format!("remove dense clone {}", dst.display()))?;
            }
        }
    }

    let mut src_file = fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
    let mut dst_file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(dst)
        .with_context(|| format!("create {}", dst.display()))?;

    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        const FICLONE: libc::Ioctl = 0x4004_9409;
        if clone_is_sparse_safe(len, allocated)
            && unsafe { libc::ioctl(dst_file.as_raw_fd(), FICLONE, src_file.as_raw_fd()) } == 0
        {
            if cloned_file_is_sparse_safe(&dst_file, len)? {
                return Ok(());
            }
            drop(dst_file);
            fs::remove_file(dst)
                .with_context(|| format!("remove dense clone {}", dst.display()))?;
            dst_file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(dst)
                .with_context(|| format!("create {}", dst.display()))?;
        }
    }

    dst_file
        .set_len(len)
        .with_context(|| format!("truncate {}", dst.display()))?;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if copy_extents(
            &mut src_file,
            &mut dst_file,
            src,
            dst,
            len,
            is_large_sparse_image(len, allocated),
        )
        .is_ok()
            && cloned_file_is_sparse_safe(&dst_file, len)?
        {
            return Ok(());
        }
    }

    #[cfg(target_os = "macos")]
    {
        if len >= LARGE_SPARSE_IMAGE_BYTES {
            anyhow::bail!(
                "sparse extent copy unavailable for large image {} -> {}; refusing dense copy",
                src.display(),
                dst.display()
            );
        }
    }

    #[cfg(target_os = "linux")]
    {
        // SEEK_DATA may be unsupported; restart with a dense scan.
        dst_file
            .set_len(0)
            .with_context(|| format!("truncate {}", dst.display()))?;
        dst_file
            .set_len(len)
            .with_context(|| format!("truncate {}", dst.display()))?;
    }

    copy_chunks(&mut src_file, &mut dst_file, src, dst, 0, len)
}

fn clone_is_sparse_safe(len: u64, allocated: u64) -> bool {
    len < LARGE_SPARSE_IMAGE_BYTES || allocated <= len / 2
}

fn is_large_sparse_image(len: u64, allocated: u64) -> bool {
    len >= LARGE_SPARSE_IMAGE_BYTES && allocated <= len / 2
}

#[cfg(target_os = "macos")]
fn cloned_destination_is_sparse_safe(path: &Path, len: u64) -> Result<bool> {
    let allocated = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .blocks()
        * 512;
    Ok(clone_is_sparse_safe(len, allocated))
}

#[cfg(unix)]
fn cloned_file_is_sparse_safe(file: &fs::File, len: u64) -> Result<bool> {
    let allocated = file.metadata()?.blocks() * 512;
    Ok(clone_is_sparse_safe(len, allocated))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn copy_extents(
    src_file: &mut fs::File,
    dst_file: &mut fs::File,
    src: &Path,
    dst: &Path,
    len: u64,
    source_is_large_sparse: bool,
) -> Result<()> {
    use std::os::fd::AsRawFd;

    let mut offset = 0u64;
    while offset < len {
        let data =
            unsafe { libc::lseek(src_file.as_raw_fd(), offset as libc::off_t, libc::SEEK_DATA) };
        if data < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENXIO) {
                return Ok(());
            }
            return Err(err).with_context(|| format!("seek data {}", src.display()));
        }
        let hole = unsafe { libc::lseek(src_file.as_raw_fd(), data, libc::SEEK_HOLE) };
        if hole < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("seek hole {}", src.display()));
        }
        let end = (hole as u64).min(len);
        if source_is_large_sparse && data as u64 == 0 && end == len {
            anyhow::bail!("filesystem reports one full-file data extent for sparse source");
        }
        copy_range(src_file, dst_file, src, dst, data as u64, end)?;
        offset = end;
    }
    Ok(())
}

/// Copy one data range, preferring in-kernel copy_file_range and falling
/// back to a userspace read/write loop where it is unsupported.
#[cfg(target_os = "linux")]
fn copy_range(
    src_file: &mut fs::File,
    dst_file: &mut fs::File,
    src: &Path,
    dst: &Path,
    start: u64,
    end: u64,
) -> Result<()> {
    use std::os::fd::AsRawFd;

    let mut offset = start;
    while offset < end {
        let want = (end - offset).min(128 * 1024 * 1024) as libc::size_t;
        let mut off_in = offset as libc::loff_t;
        let mut off_out = offset as libc::loff_t;
        let copied = unsafe {
            libc::copy_file_range(
                src_file.as_raw_fd(),
                &mut off_in,
                dst_file.as_raw_fd(),
                &mut off_out,
                want,
                0,
            )
        };
        if copied > 0 {
            offset += copied as u64;
            continue;
        }
        return copy_chunks(src_file, dst_file, src, dst, offset, end);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn copy_range(
    src_file: &mut fs::File,
    dst_file: &mut fs::File,
    src: &Path,
    dst: &Path,
    start: u64,
    end: u64,
) -> Result<()> {
    copy_chunks(src_file, dst_file, src, dst, start, end)
}

fn copy_chunks(
    src_file: &mut fs::File,
    dst_file: &mut fs::File,
    src: &Path,
    dst: &Path,
    start: u64,
    end: u64,
) -> Result<()> {
    src_file
        .seek(SeekFrom::Start(start))
        .with_context(|| format!("seek {}", src.display()))?;
    let mut buf = vec![0u8; 8 * 1024 * 1024];
    let mut offset = start;
    while offset < end {
        let want = (end - offset).min(buf.len() as u64) as usize;
        src_file
            .read_exact(&mut buf[..want])
            .with_context(|| format!("read {} at {offset}", src.display()))?;
        if buf[..want].iter().any(|byte| *byte != 0) {
            dst_file
                .seek(SeekFrom::Start(offset))
                .with_context(|| format!("seek {}", dst.display()))?;
            dst_file
                .write_all(&buf[..want])
                .with_context(|| format!("write {} at {offset}", dst.display()))?;
        }
        offset += want as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("lnx-{name}-{}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn allocated_bytes(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).expect("stat").blocks() * 512
    }

    #[test]
    fn copies_sparse_file_without_materializing_holes() {
        let dir = TempDir::new("sparse-copy");
        let src = dir.path.join("src.img");
        let dst = dir.path.join("dst.img");

        let mut file = fs::File::create(&src).expect("create src");
        file.set_len(64 * 1024 * 1024).expect("set_len");
        file.seek(SeekFrom::Start(9 * 1024 * 1024)).expect("seek");
        file.write_all(b"data-in-the-middle").expect("write");
        file.sync_all().expect("sync");
        drop(file);

        clone_or_copy_file(&src, &dst).expect("copy");

        let src_bytes = fs::read(&src).expect("read src");
        let dst_bytes = fs::read(&dst).expect("read dst");
        assert_eq!(src_bytes, dst_bytes, "contents must match");
        assert!(
            allocated_bytes(&dst) < 16 * 1024 * 1024,
            "destination must stay sparse, allocated {} bytes",
            allocated_bytes(&dst)
        );
    }
}
