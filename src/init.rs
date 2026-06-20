use std::{
    env, fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};

use crate::paths::Layout;

const DEFAULT_IMAGE_VERSION: &str = "images-v0.4.0";
const NESTED_HELPER_IMAGE_VERSION: &str = "images-v0.5.0";
const RELEASE_BASE: &str = "https://github.com/semistrict/lnx/releases/download";
const DEFAULT_ROOTFS_SIZE: u64 = 64 * 1024 * 1024 * 1024;
const REQUIRED_EXT4_BLOCK_SIZE: u64 = 16 * 1024;
const EXT4_SUPERBLOCK_OFFSET: u64 = 1024;
const EXT4_SUPERBLOCK_LEN: usize = 1024;
const EXT4_MAGIC: u16 = 0xEF53;
#[cfg(test)]
const EXT4_VALID_FS: u16 = 0x0001;
const EXT4_ERROR_FS: u16 = 0x0002;
const ZERO_SCAN_BLOCK: usize = 16 * 1024 * 1024;

pub fn run(layout: &Layout, kernel: Option<&Path>, rootfs: Option<&Path>) -> Result<()> {
    eprintln!("init: base {}", layout.base.display());
    create_dir(&layout.base)?;
    ensure_base_ignored(&layout.base)?;
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
    ensure_base_ignored(&layout.base)?;
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

pub fn ensure_base_ignored(base: &Path) -> Result<()> {
    fs::create_dir_all(base).with_context(|| format!("create {}", base.display()))?;
    let ignore = base.join(".gitignore");
    if ignore.exists() {
        return Ok(());
    }
    fs::write(&ignore, "*\n").with_context(|| format!("write {}", ignore.display()))
}

pub fn ensure_kernel(layout: &Layout) -> Result<()> {
    download_kernel(&layout.kernel)
}

pub fn ensure_nested_linux_lnx(dest: &Path) -> Result<()> {
    download_executable_release(dest, "lnx-linux-aarch64", NESTED_HELPER_IMAGE_VERSION)
}

pub fn ensure_nested_linux_gvproxy(dest: &Path) -> Result<()> {
    download_executable_release(dest, "gvproxy-linux-arm64", NESTED_HELPER_IMAGE_VERSION)
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
    crate::sparse_copy::clone_or_copy_file(src, dest)
        .with_context(|| format!("copy {} to {}", src.display(), dest.display()))?;
    Ok(())
}

fn default_rootfs(layout: &Layout) -> PathBuf {
    layout.base.join("cache").join("rootfs.ext4")
}

fn clone_or_copy(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
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
    download_release_version(dest, asset, DEFAULT_IMAGE_VERSION)
}

fn download_release_version(dest: &Path, asset: &str, version: &str) -> Result<()> {
    if dest.exists() {
        eprintln!("init: {asset} exists, skipping {}", dest.display());
        return Ok(());
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let url = format!("{RELEASE_BASE}/{version}/{asset}");
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

fn download_executable_release(dest: &Path, asset: &str, version: &str) -> Result<()> {
    download_release_version(dest, asset, version)?;
    make_executable(dest)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("chmod executable {}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
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

struct Ext4Superblock {
    log_block_size: u32,
    state: u16,
    errors: u16,
}

impl Ext4Superblock {
    fn block_size(&self) -> Result<u64> {
        1024u64
            .checked_shl(self.log_block_size)
            .context("invalid ext4 block size")
    }

    fn has_errors(&self) -> bool {
        self.state & EXT4_ERROR_FS != 0
    }
}

pub(crate) fn ensure_ext4_has_no_errors(path: &Path, label: &str) -> Result<()> {
    let superblock = read_ext4_superblock(path)?;
    if superblock.has_errors() {
        bail!(
            "{label} {} is marked with ext4 errors (state=0x{:04x}, errors=0x{:04x}); run e2fsck before restoring or snapshotting it",
            path.display(),
            superblock.state,
            superblock.errors
        );
    }
    Ok(())
}

fn read_ext4_superblock(path: &Path) -> Result<Ext4Superblock> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut superblock = [0u8; EXT4_SUPERBLOCK_LEN];
    file.seek(SeekFrom::Start(EXT4_SUPERBLOCK_OFFSET))
        .with_context(|| format!("seek ext4 superblock in {}", path.display()))?;
    file.read_exact(&mut superblock)
        .with_context(|| format!("read ext4 superblock from {}", path.display()))?;

    let magic = u16::from_le_bytes([superblock[56], superblock[57]]);
    if magic != EXT4_MAGIC {
        bail!("rootfs {} is not an ext2/3/4 filesystem", path.display());
    }

    let log_block_size = u32::from_le_bytes([
        superblock[24],
        superblock[25],
        superblock[26],
        superblock[27],
    ]);
    let state = u16::from_le_bytes([superblock[58], superblock[59]]);
    let errors = u16::from_le_bytes([superblock[60], superblock[61]]);
    Ok(Ext4Superblock {
        log_block_size,
        state,
        errors,
    })
}

fn ext4_block_size(path: &Path) -> Result<u64> {
    read_ext4_superblock(path)?.block_size()
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
mod tests;
