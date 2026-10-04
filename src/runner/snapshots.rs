//! What a memory snapshot records about the VM it came from, and the checks
//! that decide whether a VM configuration can resume it.

use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{
    LaunchMetadata, RunConfig, RunLog, snapshot_deterministic_incompatibility,
    snapshot_launch_incompatibility,
};

// Accepted vmstate.bin container version. Source backend lives in the META
// section, not in the header version.
pub(crate) const SNAPSHOT_VMSTATE_VERSION: u32 = 4;

pub(crate) struct SnapshotVmConfig {
    #[cfg_attr(
        any(not(all(target_os = "linux", target_arch = "aarch64")), not(test)),
        allow(dead_code)
    )]
    pub(crate) version: u32,
    pub(crate) memory_bytes: u64,
    pub(crate) vcpu_count: u32,
}

impl SnapshotVmConfig {
    pub(crate) fn memory_mib(&self) -> u64 {
        self.memory_bytes / 1024 / 1024
    }

    pub(crate) fn matches(&self, cpus: u8, memory_mib: u32) -> bool {
        self.vcpu_count == cpus as u32 && self.memory_mib() == memory_mib as u64
    }
}

pub(crate) fn snapshot_vm_config(snapshot: &Path) -> Result<Option<SnapshotVmConfig>> {
    let path = snapshot.join("vmstate.bin");
    if !path.exists() {
        return Ok(None);
    }
    let mut file = fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
    let mut header = [0u8; 40];
    file.read_exact(&mut header)
        .with_context(|| format!("read {}", path.display()))?;
    if &header[0..8] != b"LKRNSS01" {
        bail!("bad snapshot magic in {}", path.display());
    }
    let version = u32::from_le_bytes(header[8..12].try_into().unwrap());
    if version != SNAPSHOT_VMSTATE_VERSION {
        bail!(
            "unsupported snapshot version {version} in {}",
            path.display()
        );
    }
    Ok(Some(SnapshotVmConfig {
        version,
        memory_bytes: u64::from_le_bytes(header[16..24].try_into().unwrap()),
        vcpu_count: u32::from_le_bytes(header[32..36].try_into().unwrap()),
    }))
}

/// How to get past a memory snapshot that cannot be resumed. Dropping the
/// memory keeps the disk, so this is never destructive.
pub(crate) fn drop_memory_guidance(instance: &str) -> String {
    format!(
        "recovery: `lnx --instance {instance} snapshots clear` drops the saved memory; the next run boots from the saved disk"
    )
}

pub(crate) fn snapshot_initramfs_is_compatible(snapshot_path: &Path, current_stamp: &Path) -> bool {
    let Some(snapshot_key) = initramfs_stamp_key(&snapshot_path.join("initramfs.stamp")) else {
        return false;
    };
    let Some(current_key) = initramfs_stamp_key(current_stamp) else {
        return false;
    };
    snapshot_key == current_key
}

pub(crate) fn initramfs_stamp_key(path: &Path) -> Option<String> {
    let stamp = fs::read_to_string(path).ok()?;
    for line in stamp.lines() {
        if let Some(value) = line.strip_prefix("source=") {
            return Some(format!("source={value}"));
        }
    }
    for line in stamp.lines() {
        if let Some(value) = line.strip_prefix("sha256=") {
            return Some(format!("sha256={value}"));
        }
    }
    None
}

/// Refuses to resume a memory snapshot that this VM configuration cannot
/// run: a different guest agent, share layout, deterministic mode or VM
/// shape. The run never falls back to booting on its own (AGENTS.md); the
/// user chooses that with `snapshots clear`.
pub(crate) fn validate_restore_compatibility(
    snapshot: &Path,
    initramfs_stamp: &Path,
    launch_metadata: &LaunchMetadata,
    deterministic_stamp: &str,
    config: &RunConfig,
    run_log: &RunLog,
) -> Result<()> {
    let reason = if !snapshot_initramfs_is_compatible(snapshot, initramfs_stamp) {
        Some("it was taken by a different version of the lnx guest agent".to_string())
    } else if let Some(reason) = snapshot_launch_incompatibility(snapshot, launch_metadata) {
        Some(format!("its host shares differ ({reason})"))
    } else if let Some(reason) =
        snapshot_deterministic_incompatibility(snapshot, deterministic_stamp)
    {
        Some(format!("its deterministic mode differs ({reason})"))
    } else {
        match snapshot_vm_config(snapshot).with_context(|| {
            format!(
                "read snapshot header from {}",
                snapshot.join("vmstate.bin").display()
            )
        })? {
            Some(snapshot_config) if !snapshot_config.matches(config.cpus, config.memory_mib) => {
                Some(format!(
                    "it has {} CPUs and {} MiB of memory, not {} and {}",
                    snapshot_config.vcpu_count,
                    snapshot_config.memory_mib(),
                    config.cpus,
                    config.memory_mib
                ))
            }
            _ => None,
        }
    };
    let Some(reason) = reason else {
        return Ok(());
    };
    run_log.line(format!("snapshot.restore.incompatible reason={reason}"));
    bail!(
        "the saved memory snapshot cannot be resumed: {reason}\n{}",
        drop_memory_guidance(&config.layout.instance)
    )
}

pub(crate) fn validate_snapshot_rootfs(snapshot_path: &Path) -> Result<()> {
    crate::init::ensure_ext4_has_no_errors(&snapshot_path.join("rootfs.ext4"), "snapshot rootfs")
}
