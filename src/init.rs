use std::{
    ffi::CString,
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};

use crate::paths::Layout;

const DEFAULT_IMAGE_VERSION: &str = "images-v0.1.0";
const RELEASE_BASE: &str = "https://github.com/semistrict/lnx/releases/download";
const ZERO_SCAN_BLOCK: usize = 64 * 1024;

pub fn run(layout: &Layout, kernel: Option<&Path>, rootfs: Option<&Path>) -> Result<()> {
    eprintln!("init: base {}", layout.base.display());
    create_dir(&layout.base)?;
    create_dir(&layout.instance_dir)?;
    create_dir(
        layout
            .rootfs
            .parent()
            .context("rootfs path has no parent directory")?,
    )?;

    if let Some(kernel) = kernel {
        copy_if_needed(kernel, &layout.kernel, "kernel")?;
    } else {
        download_kernel(&layout.kernel)?;
    }

    if let Some(rootfs) = rootfs {
        copy_if_needed(rootfs, &layout.rootfs, "rootfs")?;
    } else if clone_default_rootfs(layout)? {
    } else {
        download_release(&layout.rootfs, "rootfs.ext4.zst")?;
    }

    eprintln!("init: kernel {}", layout.kernel.display());
    eprintln!("init: rootfs {}", layout.rootfs.display());
    eprintln!("init: complete");
    Ok(())
}

fn create_dir(path: &Path) -> Result<()> {
    if path.exists() {
        eprintln!("init: directory exists {}", path.display());
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

fn clone_default_rootfs(layout: &Layout) -> Result<bool> {
    if layout.rootfs.exists() || layout.instance == "default" {
        return Ok(false);
    }
    let default_rootfs = layout
        .base
        .join("images")
        .join("default")
        .join("rootfs.ext4");
    if !default_rootfs.exists() {
        return Ok(false);
    }
    eprintln!(
        "init: clone rootfs {} -> {}",
        default_rootfs.display(),
        layout.rootfs.display()
    );
    clone_or_copy(&default_rootfs, &layout.rootfs)?;
    Ok(true)
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
    fs::copy(src, dest).with_context(|| format!("copy {} to {}", src.display(), dest.display()))?;
    Ok(())
}

fn download_kernel(dest: &Path) -> Result<()> {
    if dest.exists() {
        eprintln!("init: kernel exists, skipping {}", dest.display());
        return Ok(());
    }

    let mut errors = Vec::new();
    for asset in ["kernel.Image", "vmlinuz.gz"] {
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
                    .arg("--stdout")
                    .arg(&download_tmp)
                    .stdout(Stdio::from(
                        fs::File::create(&output_tmp)
                            .with_context(|| format!("create {}", output_tmp.display()))?,
                    )),
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
