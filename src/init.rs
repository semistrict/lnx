use std::{
    env, fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[cfg(target_os = "macos")]
use std::ffi::CString;

use anyhow::{Context, Result, bail};

use crate::paths::Layout;

const DEFAULT_IMAGE_VERSION: &str = "images-v0.4.0";
const RELEASE_BASE: &str = "https://github.com/semistrict/lnx/releases/download";
const DEFAULT_ROOTFS_SIZE: u64 = 64 * 1024 * 1024 * 1024;
const REQUIRED_EXT4_BLOCK_SIZE: u64 = 16 * 1024;
const EXT4_SUPERBLOCK_OFFSET: u64 = 1024;
const EXT4_SUPERBLOCK_LEN: usize = 1024;
const ZERO_SCAN_BLOCK: usize = 16 * 1024 * 1024;

pub fn run(layout: &Layout, kernel: Option<&Path>, rootfs: Option<&Path>) -> Result<()> {
    eprintln!("init: base {}", layout.base.display());
    create_dir(&layout.base)?;
    let default_rootfs = default_rootfs(layout);
    create_dir(
        default_rootfs
            .parent()
            .context("rootfs path has no parent directory")?,
    )?;

    if let Some(kernel) = kernel {
        copy_if_needed(kernel, &layout.kernel, "kernel")?;
    } else {
        download_kernel(&layout.kernel)?;
    }

    let initialized_rootfs = if let Some(rootfs) = rootfs {
        copy_if_needed(rootfs, &layout.rootfs, "rootfs")?;
        &layout.rootfs
    } else {
        download_release(&default_rootfs, "rootfs.ext4.zst")?;
        &default_rootfs
    };

    if rootfs.is_none() {
        ensure_rootfs_min_size(&default_rootfs, DEFAULT_ROOTFS_SIZE)?;
        validate_managed_rootfs(&default_rootfs, DEFAULT_ROOTFS_SIZE)?;
    }

    let image = match rootfs {
        Some(rootfs) => format!("file:{}", rootfs.display()),
        None => format!("release:{DEFAULT_IMAGE_VERSION}"),
    };
    crate::descriptor::ensure_identity(layout, &image)?;

    eprintln!("init: kernel {}", layout.kernel.display());
    eprintln!("init: rootfs {}", initialized_rootfs.display());
    eprintln!("init: complete");
    Ok(())
}

pub fn ensure_instance(layout: &Layout) -> Result<()> {
    create_dir(&layout.instance_dir)?;
    create_dir(
        layout
            .rootfs
            .parent()
            .context("rootfs path has no parent directory")?,
    )?;
    if layout.rootfs.exists() {
        return Ok(());
    }
    let default_rootfs = default_rootfs(layout);
    if !default_rootfs.exists() {
        bail!("missing default rootfs: {}", default_rootfs.display());
    }
    crate::descriptor::ensure_identity(layout, &format!("clone:{}", default_rootfs.display()))?;
    eprintln!(
        "init: clone rootfs {} -> {}",
        default_rootfs.display(),
        layout.rootfs.display()
    );
    clone_or_copy(&default_rootfs, &layout.rootfs)
}

pub fn ensure_kernel(layout: &Layout) -> Result<()> {
    download_kernel(&layout.kernel)
}

/// Installs a caller-supplied kernel image instead of downloading one.
pub fn install_kernel(layout: &Layout, kernel: &Path) -> Result<()> {
    copy_if_needed(kernel, &layout.kernel, "kernel")
}

fn create_dir(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    eprintln!("init: create directory {}", path.display());
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))
}

fn copy_if_needed(src: &Path, dest: &Path, label: &str) -> Result<()> {
    if dest.exists() {
        eprintln!("init: {label} exists, skipping {}", dest.display());
        return Ok(());
    }
    eprintln!("init: copy {label} {} -> {}", src.display(), dest.display());
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::copy(src, dest).with_context(|| format!("copy {} to {}", src.display(), dest.display()))?;
    Ok(())
}

fn default_rootfs(layout: &Layout) -> PathBuf {
    layout.base.join("cache").join("rootfs.ext4")
}

fn clone_or_copy(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    match clone_file(src, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("init: reflink clone unavailable ({e}), copying rootfs");
            fs::copy(src, dest)
                .with_context(|| format!("copy {} to {}", src.display(), dest.display()))?;
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
fn clone_file(src: &Path, dest: &Path) -> Result<()> {
    unsafe extern "C" {
        fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> i32;
    }

    let src = CString::new(src.to_string_lossy().as_bytes())?;
    let dest = CString::new(dest.to_string_lossy().as_bytes())?;
    if unsafe { clonefile(src.as_ptr(), dest.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).context("clonefile")
    }
}

#[cfg(not(target_os = "macos"))]
fn clone_file(src: &Path, dest: &Path) -> Result<()> {
    crate::sparse_copy::clone_or_copy_file(src, dest)
}

fn download_kernel(dest: &Path) -> Result<()> {
    if dest.exists() {
        eprintln!("init: kernel exists, skipping {}", dest.display());
        return Ok(());
    }

    let mut errors = Vec::new();
    for asset in ["vmlinuz.gz", "kernel.Image"] {
        match download_release(dest, asset) {
            Ok(()) => return Ok(()),
            Err(e) => errors.push(format!("{asset}: {e:#}")),
        }
    }
    bail!("{}", errors.join("; "))
}

fn download_release(dest: &Path, asset: &str) -> Result<()> {
    if dest.exists() {
        eprintln!("init: {asset} exists, skipping {}", dest.display());
        return Ok(());
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let url = format!("{RELEASE_BASE}/{DEFAULT_IMAGE_VERSION}/{asset}");
    let download_tmp = dest.with_extension(format!(
        "{}download",
        dest.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| format!("{ext}."))
            .unwrap_or_default()
    ));
    let output_tmp = dest.with_extension(format!(
        "{}tmp",
        dest.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| format!("{ext}."))
            .unwrap_or_default()
    ));
    let _ = fs::remove_file(&download_tmp);
    let _ = fs::remove_file(&output_tmp);

    eprintln!("init: download {url}");
    run_status(
        Command::new("curl")
            .arg("--fail")
            .arg("--location")
            .arg("--progress-bar")
            .arg("--output")
            .arg(&download_tmp)
            .arg(&url),
        "curl",
    )?;

    match Path::new(asset).extension().and_then(|ext| ext.to_str()) {
        Some("zst") => {
            eprintln!("init: decompress {asset}");
            run_status(
                Command::new("zstd")
                    .arg("-d")
                    .arg("--force")
                    .arg("--sparse")
                    .arg("--progress")
                    .arg("-o")
                    .arg(&output_tmp)
                    .arg(&download_tmp)
                    .stdout(Stdio::inherit()),
                "zstd",
            )?;
            eprintln!("init: sparsify {}", output_tmp.display());
            punch_holes(&output_tmp, ZERO_SCAN_BLOCK)?;
        }
        Some("gz") => {
            eprintln!("init: decompress {asset}");
            run_status(
                Command::new("gzip")
                    .arg("-dc")
                    .arg(&download_tmp)
                    .stdout(Stdio::from(
                        fs::File::create(&output_tmp)
                            .with_context(|| format!("create {}", output_tmp.display()))?,
                    )),
                "gzip",
            )?;
        }
        _ => {
            fs::rename(&download_tmp, &output_tmp).with_context(|| {
                format!(
                    "rename {} to {}",
                    download_tmp.display(),
                    output_tmp.display()
                )
            })?;
        }
    }

    fs::rename(&output_tmp, dest)
        .with_context(|| format!("rename {} to {}", output_tmp.display(), dest.display()))?;
    let _ = fs::remove_file(&download_tmp);
    eprintln!("init: installed {asset} -> {}", dest.display());
    Ok(())
}

fn run_status(command: &mut Command, label: &str) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("run {label}; is it installed?"))?;
    if !status.success() {
        bail!("{label} failed with {status}");
    }
    Ok(())
}

fn ensure_rootfs_min_size(path: &Path, min_size: u64) -> Result<()> {
    let size = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    if size >= min_size {
        eprintln!(
            "init: rootfs size {} already >= {}",
            format_size(size),
            format_size(min_size)
        );
        return Ok(());
    }

    eprintln!(
        "init: grow rootfs {} from {} to {}",
        path.display(),
        format_size(size),
        format_size(min_size)
    );
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?
        .set_len(min_size)
        .with_context(|| format!("resize backing file {}", path.display()))?;

    let e2fsck = find_tool("e2fsck")?;
    let status = Command::new(&e2fsck)
        .arg("-fy")
        .arg(path)
        .status()
        .with_context(|| format!("run {}", e2fsck.display()))?;
    match status.code() {
        Some(code) if code & !3 == 0 => {}
        _ => bail!("e2fsck failed with {status}"),
    }

    let resize2fs = find_tool("resize2fs")?;
    run_status(
        Command::new(&resize2fs).arg(path),
        &resize2fs.to_string_lossy(),
    )?;
    punch_holes(path, ZERO_SCAN_BLOCK)?;
    Ok(())
}

/// Validate an externally built image against the managed-rootfs contract
/// (64 GiB sparse, 16 KiB-block ext4).
pub fn validate_managed_rootfs_at(path: &Path) -> Result<()> {
    validate_managed_rootfs(path, DEFAULT_ROOTFS_SIZE)
}

fn validate_managed_rootfs(path: &Path, min_size: u64) -> Result<()> {
    let size = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    if size < min_size {
        bail!(
            "rootfs {} is {}, expected at least {}",
            path.display(),
            format_size(size),
            format_size(min_size)
        );
    }

    let block_size = ext4_block_size(path)?;
    if block_size != REQUIRED_EXT4_BLOCK_SIZE {
        bail!(
            "rootfs {} has ext4 block size {}, expected {} for the 16K-page DAX kernel",
            path.display(),
            block_size,
            REQUIRED_EXT4_BLOCK_SIZE
        );
    }
    eprintln!("init: rootfs ext4 block size {block_size}");
    Ok(())
}

fn ext4_block_size(path: &Path) -> Result<u64> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut superblock = [0u8; EXT4_SUPERBLOCK_LEN];
    file.seek(SeekFrom::Start(EXT4_SUPERBLOCK_OFFSET))
        .with_context(|| format!("seek ext4 superblock in {}", path.display()))?;
    file.read_exact(&mut superblock)
        .with_context(|| format!("read ext4 superblock from {}", path.display()))?;

    let magic = u16::from_le_bytes([superblock[56], superblock[57]]);
    if magic != 0xEF53 {
        bail!("rootfs {} is not an ext2/3/4 filesystem", path.display());
    }

    let log_block_size = u32::from_le_bytes([
        superblock[24],
        superblock[25],
        superblock[26],
        superblock[27],
    ]);
    1024u64
        .checked_shl(log_block_size)
        .context("invalid ext4 block size")
}

fn find_tool(name: &str) -> Result<PathBuf> {
    if let Some(path) = find_in_path(name) {
        return Ok(path);
    }

    for dir in [
        "/opt/homebrew/opt/e2fsprogs/sbin",
        "/usr/local/opt/e2fsprogs/sbin",
        "/opt/homebrew/sbin",
        "/usr/local/sbin",
    ] {
        let path = Path::new(dir).join(name);
        if path.exists() {
            return Ok(path);
        }
    }

    bail!("{name} not found; install e2fsprogs to resize the rootfs")
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|path| path.exists())
}

fn format_size(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    if bytes % GIB == 0 {
        format!("{}GiB", bytes / GIB)
    } else {
        format!("{bytes} bytes")
    }
}

fn punch_holes(path: &Path, block_size: usize) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let size = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    let mut buf = vec![0u8; block_size];

    if scan_data_extents_for_holes(&mut file, size, &mut buf)? {
        return Ok(());
    }

    let mut offset = 0u64;
    while offset < size {
        file.seek(SeekFrom::Start(offset))?;
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if buf[..n].iter().all(|byte| *byte == 0) && !punch_hole(&file, offset, n as u64) {
            eprintln!("init: sparse hole punching unsupported here, continuing");
            return Ok(());
        }
        offset += n as u64;
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn scan_data_extents_for_holes(file: &mut fs::File, size: u64, buf: &mut [u8]) -> Result<bool> {
    let mut offset = 0u64;
    while offset < size {
        let data_offset = match seek_extent(file, offset, libc::SEEK_DATA)? {
            ExtentSeek::Found(offset) => offset,
            ExtentSeek::NotFound => return Ok(true),
            ExtentSeek::Unsupported => return Ok(false),
        };
        let hole_offset = match seek_extent(file, data_offset, libc::SEEK_HOLE)? {
            ExtentSeek::Found(offset) => offset,
            ExtentSeek::NotFound | ExtentSeek::Unsupported => return Ok(false),
        };
        scan_range_for_holes(file, data_offset, hole_offset.min(size), buf)?;
        offset = hole_offset.max(data_offset + 1);
    }
    Ok(true)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn scan_data_extents_for_holes(_file: &mut fs::File, _size: u64, _buf: &mut [u8]) -> Result<bool> {
    Ok(false)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
enum ExtentSeek {
    Found(u64),
    NotFound,
    Unsupported,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn seek_extent(file: &fs::File, offset: u64, whence: i32) -> Result<ExtentSeek> {
    use std::os::fd::AsRawFd;

    let result = unsafe { libc::lseek(file.as_raw_fd(), offset as libc::off_t, whence) };
    if result >= 0 {
        return Ok(ExtentSeek::Found(result as u64));
    }

    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENXIO) => Ok(ExtentSeek::NotFound),
        Some(libc::EINVAL) => Ok(ExtentSeek::Unsupported),
        _ => Err(error).context("seek sparse extent"),
    }
}

fn scan_range_for_holes(file: &mut fs::File, start: u64, end: u64, buf: &mut [u8]) -> Result<()> {
    let mut offset = start;
    while offset < end {
        file.seek(SeekFrom::Start(offset))?;
        let max_len = (end - offset).min(buf.len() as u64) as usize;
        let n = file.read(&mut buf[..max_len])?;
        if n == 0 {
            break;
        }
        if buf[..n].iter().all(|byte| *byte == 0) && !punch_hole(file, offset, n as u64) {
            eprintln!("init: sparse hole punching unsupported here, continuing");
            break;
        }
        offset += n as u64;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn punch_hole(file: &fs::File, offset: u64, len: u64) -> bool {
    use std::os::fd::AsRawFd;

    #[repr(C)]
    struct Fpunchhole {
        flags: u32,
        reserved: u32,
        offset: i64,
        length: i64,
    }

    const F_PUNCHHOLE: i32 = 99;
    let mut punch = Fpunchhole {
        flags: 0,
        reserved: 0,
        offset: offset as i64,
        length: len as i64,
    };
    unsafe { libc::fcntl(file.as_raw_fd(), F_PUNCHHOLE, &mut punch as *mut Fpunchhole) == 0 }
}

#[cfg(target_os = "linux")]
fn punch_hole(file: &fs::File, offset: u64, len: u64) -> bool {
    use std::os::fd::AsRawFd;

    unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            offset as libc::off_t,
            len as libc::off_t,
        ) == 0
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn punch_hole(_file: &fs::File, _offset: u64, _len: u64) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Seek, Write},
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                env::temp_dir().join(format!("lnx-{name}-{}-{unique}.ext4", std::process::id()));
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn write_fake_ext4(path: &Path, log_block_size: u32, len: u64) {
        let mut file = fs::File::create(path).expect("create fake ext4");
        file.set_len(len).expect("size fake ext4");
        let mut superblock = [0u8; EXT4_SUPERBLOCK_LEN];
        superblock[24..28].copy_from_slice(&log_block_size.to_le_bytes());
        superblock[56..58].copy_from_slice(&0xEF53u16.to_le_bytes());
        file.seek(SeekFrom::Start(EXT4_SUPERBLOCK_OFFSET))
            .expect("seek superblock");
        file.write_all(&superblock).expect("write superblock");
    }

    #[test]
    fn ext4_block_size_reads_superblock() {
        let image = TempFile::new("block-size");
        write_fake_ext4(image.path(), 4, 4096);

        assert_eq!(
            ext4_block_size(image.path()).expect("block size"),
            16 * 1024
        );
    }

    #[test]
    fn validate_managed_rootfs_rejects_4k_ext4() {
        let image = TempFile::new("bad-block-size");
        write_fake_ext4(image.path(), 2, 4096);

        let error = validate_managed_rootfs(image.path(), 4096).expect_err("4K ext4 should fail");
        assert!(
            error.to_string().contains("expected 16384"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn validate_managed_rootfs_accepts_64g_16k_ext4() {
        let image = TempFile::new("good-rootfs");
        write_fake_ext4(image.path(), 4, DEFAULT_ROOTFS_SIZE);

        validate_managed_rootfs(image.path(), DEFAULT_ROOTFS_SIZE).expect("valid rootfs");
    }
}
