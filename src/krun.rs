use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::Result;
use libkrun::{LogLevel, VirtioFs};

const VIRTIOFS_DAX_WINDOW_BYTES: u64 = 8 << 30;

pub(crate) fn log_level_from_verbosity(level: u32) -> LogLevel {
    match level {
        0 => LogLevel::Off,
        1 => LogLevel::Error,
        2 => LogLevel::Warn,
        3 => LogLevel::Info,
        4 => LogLevel::Debug,
        _ => LogLevel::Trace,
    }
}

pub(crate) fn init_logging_once(level: LogLevel) -> Result<()> {
    static LOG_LEVEL: OnceLock<LogLevel> = OnceLock::new();
    if LOG_LEVEL.set(level).is_err() {
        return Ok(());
    }
    libkrun::init_logging(level)?;
    Ok(())
}

pub(crate) struct DeterministicHostActivity;

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
pub(crate) fn deterministic_host_activity() -> DeterministicHostActivity {
    libkrun::VmBuilder::deterministic_host_activity_begin();
    DeterministicHostActivity
}

#[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
pub(crate) fn deterministic_host_activity() -> DeterministicHostActivity {
    DeterministicHostActivity
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
impl Drop for DeterministicHostActivity {
    fn drop(&mut self) {
        libkrun::VmBuilder::deterministic_host_activity_end();
    }
}

// Mirrors the linux-aarch64 impl above: the guard is droppable on every
// platform, so explicit drops of it are never platform-conditional no-ops.
#[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
impl Drop for DeterministicHostActivity {
    fn drop(&mut self) {}
}

pub(crate) fn host_share_virtiofs(
    tag: &str,
    path: &Path,
    write_allowlist: &[String],
    unshare_dir: &Path,
) -> VirtioFs {
    VirtioFs::shared(tag, path)
        .dax_window_bytes(VIRTIOFS_DAX_WINDOW_BYTES)
        .write_allowlist(write_allowlist.iter().map(PathBuf::from))
        .unshare_dir(unshare_dir)
}
