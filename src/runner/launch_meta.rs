use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{LAUNCH_METADATA, RunConfig, host_home_for_cwd, owner_restart_args};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ShareLayout {
    pub(crate) host_home: PathBuf,
    pub(crate) outside_home_cwd: Option<PathBuf>,
    pub(crate) no_host_shares: bool,
}

pub(crate) struct SnapshotShareLayout {
    pub(crate) metadata: LaunchMetadata,
    pub(crate) layout: ShareLayout,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct LaunchMetadata {
    pub(crate) version: u32,
    pub(crate) owner_args: Vec<String>,
    pub(crate) compatibility: LaunchCompatibility,
    pub(crate) shares: LaunchShares,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) vhost_user_fs: Vec<LaunchVhostUserFsMount>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct LaunchCompatibility {
    pub(crate) host_share_cache: LaunchHostShareCache,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct LaunchHostShareCache {
    pub(crate) dax: bool,
}

// A restored snapshot keeps its snapshot-time virtiofs devices and guest
// mounts, so the device topology is part of snapshot compatibility. Version 2
// dropped the nix package-store mount; version-1 snapshots carry its virtiofs
// device and must cold-boot.
pub(crate) const LAUNCH_METADATA_VERSION: u32 = 2;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct LaunchShares {
    pub(crate) no_host_shares: bool,
    pub(crate) host_home: Option<PathBuf>,
    pub(crate) outside_home_cwd: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct LaunchVhostUserFsMount {
    pub(crate) tag: String,
    pub(crate) mount: String,
    pub(crate) socket: PathBuf,
    pub(crate) read_only: bool,
}

// A restored guest keeps its snapshot-time share mounts and kernel-side
// virtiofs caches. A snapshot is only valid for the same host share roots and
// host-share cache policy: a drifted root would silently back the old guest
// mount points with a different host directory, and an old cache policy can
// preserve stale host-file contents or size after the host changed while the VM
// was stopped.
pub(crate) fn launch_metadata_for_config(config: &RunConfig) -> Result<LaunchMetadata> {
    let host_home = host_home_for_cwd(&config.cwd)?;
    let outside_home_cwd = (!config.cwd.starts_with(&host_home)).then(|| config.cwd.clone());
    Ok(launch_metadata_for_parts(
        config,
        host_home,
        outside_home_cwd,
        config.no_host_shares,
        host_share_cache_metadata(),
    ))
}

fn launch_metadata_for_parts(
    config: &RunConfig,
    host_home: PathBuf,
    outside_home_cwd: Option<PathBuf>,
    no_host_shares: bool,
    host_share_cache: LaunchHostShareCache,
) -> LaunchMetadata {
    LaunchMetadata {
        version: LAUNCH_METADATA_VERSION,
        owner_args: owner_restart_args(config),
        compatibility: LaunchCompatibility { host_share_cache },
        shares: LaunchShares {
            no_host_shares,
            host_home: (!no_host_shares).then_some(host_home),
            outside_home_cwd: if no_host_shares {
                None
            } else {
                outside_home_cwd
            },
        },
        vhost_user_fs: config
            .vhost_user_fs
            .iter()
            .map(|mount| LaunchVhostUserFsMount {
                tag: mount.tag.clone(),
                mount: mount.mountpoint.clone(),
                socket: mount.socket.clone(),
                read_only: mount.read_only,
            })
            .collect(),
    }
}

fn host_share_cache_metadata() -> LaunchHostShareCache {
    LaunchHostShareCache { dax: true }
}

/// Whether the default restore snapshot was written by the current launch
/// metadata version. Missing launch metadata is treated as matching; deciding
/// on it is the general snapshot compatibility check's job.
pub fn default_restore_version_matches(snapshot: &Path) -> Result<bool> {
    match read_launch_metadata(snapshot) {
        Ok(metadata) => Ok(metadata.version == LAUNCH_METADATA_VERSION),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(_) => Ok(false),
    }
}

pub fn snapshot_shares_incompatibility_for_import(
    snapshot_path: &Path,
    cwd: &Path,
    no_host_shares: bool,
) -> Result<Option<String>> {
    let host_home = host_home_for_cwd(cwd)?;
    let outside_home_cwd = (!cwd.starts_with(&host_home)).then(|| cwd.to_path_buf());
    let current = LaunchMetadata {
        version: LAUNCH_METADATA_VERSION,
        owner_args: Vec::new(),
        compatibility: LaunchCompatibility {
            host_share_cache: host_share_cache_metadata(),
        },
        shares: LaunchShares {
            no_host_shares,
            host_home: (!no_host_shares).then_some(host_home),
            outside_home_cwd: if no_host_shares {
                None
            } else {
                outside_home_cwd
            },
        },
        vhost_user_fs: Vec::new(),
    };
    Ok(snapshot_launch_incompatibility(snapshot_path, &current))
}

pub(crate) fn snapshot_launch_incompatibility(
    snapshot_path: &Path,
    current: &LaunchMetadata,
) -> Option<String> {
    match read_launch_metadata(snapshot_path) {
        Ok(snapshot) if launch_metadata_matches_ignoring_cwd(&snapshot, current) => None,
        Ok(snapshot) => Some(describe_launch_mismatch(&snapshot, current)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Some("launch_metadata: snapshot has no launch.json".to_string())
        }
        Err(e) => Some(format!("launch_metadata_unreadable: {e}")),
    }
}

pub(crate) fn describe_launch_mismatch(
    snapshot: &LaunchMetadata,
    current: &LaunchMetadata,
) -> String {
    let mut mismatches = Vec::new();
    if snapshot.shares.no_host_shares != current.shares.no_host_shares {
        mismatches.push(format!(
            "host-shares: snapshot={} current={}",
            if snapshot.shares.no_host_shares {
                "disabled"
            } else {
                "enabled"
            },
            if current.shares.no_host_shares {
                "disabled"
            } else {
                "enabled"
            }
        ));
    }
    if snapshot.compatibility.host_share_cache != current.compatibility.host_share_cache {
        mismatches.push(format!(
            "host-share-cache: snapshot={} current={}",
            describe_host_share_cache(&snapshot.compatibility.host_share_cache),
            describe_host_share_cache(&current.compatibility.host_share_cache)
        ));
    }
    if snapshot.shares.host_home != current.shares.host_home {
        mismatches.push(format!(
            "home: snapshot={} current={}",
            optional_path_display(snapshot.shares.host_home.as_deref()),
            optional_path_display(current.shares.host_home.as_deref())
        ));
    }
    if normalized_vhost_user_fs(&snapshot.vhost_user_fs)
        != normalized_vhost_user_fs(&current.vhost_user_fs)
    {
        mismatches.push(format!(
            "vhost-user-fs: snapshot={} current={}",
            vhost_user_fs_launch_value(&snapshot.vhost_user_fs),
            vhost_user_fs_launch_value(&current.vhost_user_fs)
        ));
    }
    if mismatches.is_empty() {
        "share_mismatch: launch metadata differs only in ignored fields".to_string()
    } else {
        format!("share_mismatch: {}", mismatches.join("; "))
    }
}

fn optional_path_display(path: Option<&Path>) -> String {
    path.map(|path| path.display().to_string())
        .unwrap_or_else(|| "<absent>".to_string())
}

fn describe_host_share_cache(cache: &LaunchHostShareCache) -> String {
    if cache.dax {
        "dax".to_string()
    } else {
        "nodax".to_string()
    }
}

pub(crate) fn launch_metadata_matches_ignoring_cwd(
    snapshot: &LaunchMetadata,
    current: &LaunchMetadata,
) -> bool {
    let mut snapshot = snapshot.clone();
    let mut current = current.clone();
    snapshot.owner_args.clear();
    current.owner_args.clear();
    snapshot.shares.outside_home_cwd = None;
    current.shares.outside_home_cwd = None;
    snapshot == current
}

fn normalized_vhost_user_fs(mounts: &[LaunchVhostUserFsMount]) -> Vec<LaunchVhostUserFsMount> {
    let mut mounts = mounts.to_vec();
    mounts.sort_by(|a, b| {
        (&a.tag, &a.mount, &a.socket, a.read_only).cmp(&(&b.tag, &b.mount, &b.socket, b.read_only))
    });
    mounts
}

fn vhost_user_fs_launch_value(mounts: &[LaunchVhostUserFsMount]) -> String {
    normalized_vhost_user_fs(mounts)
        .iter()
        .map(|mount| {
            format!(
                "{}:{}:{}:{}",
                mount.tag,
                mount.mount,
                mount.socket.display(),
                if mount.read_only { "ro" } else { "rw" }
            )
        })
        .collect::<Vec<_>>()
        .join(";")
}

pub(crate) fn write_launch_metadata(path: &Path, metadata: &LaunchMetadata) -> Result<()> {
    let data = serde_json::to_vec_pretty(metadata).context("encode launch metadata")?;
    fs::write(path, data).with_context(|| format!("write {}", path.display()))
}

pub(crate) fn read_launch_metadata(snapshot_path: &Path) -> std::io::Result<LaunchMetadata> {
    let path = snapshot_path.join(LAUNCH_METADATA);
    let data = fs::read(&path)?;
    serde_json::from_slice(&data)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}

pub(crate) fn parse_shares_stamp(stamp: &str) -> BTreeMap<String, String> {
    stamp
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

pub(crate) fn snapshot_share_layout(snapshot_path: &Path) -> Result<Option<SnapshotShareLayout>> {
    let metadata = match read_launch_metadata(snapshot_path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| {
                format!("read {}", snapshot_path.join(LAUNCH_METADATA).display())
            });
        }
    };
    Ok(Some(SnapshotShareLayout {
        layout: ShareLayout {
            host_home: metadata.shares.host_home.clone().unwrap_or_default(),
            outside_home_cwd: metadata.shares.outside_home_cwd.clone(),
            no_host_shares: metadata.shares.no_host_shares,
        },
        metadata,
    }))
}
