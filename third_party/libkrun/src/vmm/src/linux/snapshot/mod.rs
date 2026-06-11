// Snapshot/restore support for the Linux KVM backend.
//
// The Linux implementation uses the same on-disk shape as the macOS backend:
//   <path>/vmstate.bin   -- tagged bincode/raw sections
//   <path>/pages.img     -- guest RAM contents

pub mod container;
pub mod orchestrator;
pub mod ram;

pub use container::{SectionId, SnapshotReader};
pub use orchestrator::{CaptureInputs, capture, capture_with_paused_hook, restore};

use std::fmt;
use std::io;
use std::path::PathBuf;

pub const PAGES_IMG: &str = "pages.img";
pub const VMSTATE_BIN: &str = "vmstate.bin";

#[derive(Debug)]
pub enum SnapshotError {
    Io(io::Error),
    BadMagic,
    BadVersion(u32),
    BadHash { id: u32, index: u32 },
    Truncated,
    SectionMissing { id: u32, index: u32 },
    Bincode(bincode::Error),
    ConfigMismatch(String),
    DeviceRefused(String),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use SnapshotError::*;
        match self {
            Io(e) => write!(f, "snapshot io error: {e}"),
            BadMagic => write!(f, "snapshot bad magic"),
            BadVersion(v) => write!(f, "snapshot unsupported version {v}"),
            BadHash { id, index } => write!(f, "snapshot section {id}/{index} hash mismatch"),
            Truncated => write!(f, "snapshot truncated"),
            SectionMissing { id, index } => write!(f, "snapshot missing section {id}/{index}"),
            Bincode(e) => write!(f, "snapshot bincode error: {e}"),
            ConfigMismatch(s) => write!(f, "snapshot config mismatch: {s}"),
            DeviceRefused(s) => write!(f, "snapshot device refused: {s}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<io::Error> for SnapshotError {
    fn from(e: io::Error) -> Self {
        SnapshotError::Io(e)
    }
}

impl From<bincode::Error> for SnapshotError {
    fn from(e: bincode::Error) -> Self {
        SnapshotError::Bincode(e)
    }
}

pub type Result<T> = std::result::Result<T, SnapshotError>;

pub fn vmstate_path(dir: &std::path::Path) -> PathBuf {
    dir.join(VMSTATE_BIN)
}

pub fn pages_img_path(dir: &std::path::Path) -> PathBuf {
    dir.join(PAGES_IMG)
}

pub(super) fn snapshot_sync_enabled() -> bool {
    std::env::var("KRUN_SNAPSHOT_SYNC")
        .map(|value| {
            let value = value.trim();
            !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}
