//! Moves an instance from the layout lnx used before the store into the
//! store.
//!
//! The old layout kept a canonical `rootfs.ext4`, a live `host-share-state/`,
//! `memory-snapshots/latest/`, crash markers (`.restore-work.active`,
//! `final-snapshot.outcome`) and checkpoint directories. Migration clones
//! (APFS clones are instant) instead of renaming, commits `state.json` as its
//! single commit point, and only then deletes the old files, so an interrupted
//! migration either never happened or only left files to delete.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{
    CheckpointRef, GenerationId, HOST_SHARE_STATE, Origin, PAGES, Phase, RECORD_VERSION, ROOTFS,
    Record, RunId, Store, VMSTATE, commit_json, sync_dir, sync_tree,
};
use crate::fsutil::remove_path_if_exists;
use crate::paths::Layout;
use crate::runner::{InstanceLock, ProcessIdentity};
use crate::sparse_copy::{clone_or_copy_file, clone_or_copy_tree};

const SNAPSHOTS_DIR: &str = "memory-snapshots";
const LATEST: &str = "latest";
const LATEST_PREVIOUS: &str = ".latest.previous";
const RESTORE_WORK: &str = ".restore-work";
const RESTORE_WORK_ACTIVE: &str = ".restore-work.active";
const CHECKPOINT_META: &str = "checkpoint.meta";
const OLD_LOCK: &str = "bootstrap.lock.d";
const OLD_START_LOCK: &str = "owner-start.lock.d";

/// What migration found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Migration {
    /// The instance already uses the store, or has no state at all.
    NotNeeded,
    /// The old state is now in the store.
    Migrated {
        latest: GenerationId,
        crashed_run: Option<RunId>,
        checkpoints: usize,
    },
}

/// Migrates `layout`'s instance into the store if it still uses the old
/// layout. Requires the instance lock.
pub(crate) fn migrate_legacy_layout(layout: &Layout, lock: &InstanceLock) -> Result<Migration> {
    let store = Store::new(&layout.instance_dir);
    if store.exists() {
        remove_legacy_files(layout)?;
        return Ok(Migration::NotNeeded);
    }
    let snapshots = layout.instance_dir.join(SNAPSHOTS_DIR);
    let canonical = layout.instance_dir.join(ROOTFS);
    let latest = legacy_latest(&snapshots);
    if latest.is_none() && !canonical.exists() {
        return Ok(Migration::NotNeeded);
    }
    refuse_live_legacy_owner(layout)?;
    // A previous attempt may have published generations before dying; with no
    // record they are unreferenced, so start over.
    remove_path_if_exists(&store.generations_dir())?;
    remove_path_if_exists(&store.runs_dir())?;

    let latest_id = match &latest {
        Some(dir) => store.import_dir(lock, dir, None, Origin::Migrated)?,
        None => {
            let staging = store.stage(lock)?;
            clone_or_copy_file(&canonical, &staging.dir.join(ROOTFS))?;
            let shares = layout.instance_dir.join(HOST_SHARE_STATE);
            if shares.exists() {
                clone_or_copy_tree(&shares, &staging.dir.join(HOST_SHARE_STATE))?;
            }
            store.publish(lock, staging, None, Origin::Migrated)?
        }
    };

    let crashed_run = migrate_crashed_run(layout, &store, &snapshots)?;

    let mut checkpoints = 0;
    for (id, dir) in legacy_checkpoints(layout)? {
        let generation = store.import_dir(lock, &dir, None, Origin::Migrated)?;
        let (name, created_unix) = read_checkpoint_meta(&dir);
        store.add_checkpoint(
            lock,
            &CheckpointRef {
                id,
                name,
                generation,
                created_unix,
            },
        )?;
        checkpoints += 1;
    }

    let phase = match &crashed_run {
        Some(run) => Phase::Dirty {
            run: run.clone(),
            owner: ProcessIdentity { pid: 0, started: 0 },
        },
        None => Phase::Stopped,
    };
    commit_json(
        &store.state_path(),
        &Record {
            version: RECORD_VERSION,
            latest: Some(latest_id.clone()),
            phase,
        },
    )?;
    remove_legacy_files(layout)?;
    Ok(Migration::Migrated {
        latest: latest_id,
        crashed_run,
        checkpoints,
    })
}

/// The old `latest` snapshot, if complete. A crash between the two renames
/// of the old publish protocol left only `.latest.previous`.
fn legacy_latest(snapshots: &Path) -> Option<PathBuf> {
    [LATEST, LATEST_PREVIOUS]
        .into_iter()
        .map(|name| snapshots.join(name))
        .find(|dir| {
            [VMSTATE, PAGES, ROOTFS]
                .iter()
                .all(|name| dir.join(name).is_file())
        })
}

/// An old run that died after starting from a snapshot left its live disk in
/// `.restore-work`; it becomes a crashed run the user can salvage.
fn migrate_crashed_run(layout: &Layout, store: &Store, snapshots: &Path) -> Result<Option<RunId>> {
    let work = snapshots.join(RESTORE_WORK);
    if !snapshots.join(RESTORE_WORK_ACTIVE).exists() || !work.join(ROOTFS).is_file() {
        return Ok(None);
    }
    let run = RunId::new();
    let dir = store.run_dir(&run);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    for entry in fs::read_dir(&work).with_context(|| format!("read {}", work.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            clone_or_copy_file(&entry.path(), &dir.join(entry.file_name()))?;
        }
    }
    // The crashed VM wrote its share state in place, not into the work dir.
    let shares = layout.instance_dir.join(HOST_SHARE_STATE);
    if shares.exists() {
        clone_or_copy_tree(&shares, &dir.join(HOST_SHARE_STATE))?;
    }
    sync_tree(&dir)?;
    sync_dir(&store.runs_dir())?;
    Ok(Some(run))
}

fn legacy_checkpoints(layout: &Layout) -> Result<Vec<(String, PathBuf)>> {
    let dir = layout.instance_dir.join(super::CHECKPOINTS_DIR);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
    };
    let mut checkpoints = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let id = entry.file_name().to_string_lossy().into_owned();
        // Only complete checkpoints: the old code wrote checkpoint.meta last.
        if entry.file_type()?.is_dir()
            && !id.starts_with('.')
            && path.join(ROOTFS).is_file()
            && path.join(CHECKPOINT_META).is_file()
        {
            checkpoints.push((id, path));
        }
    }
    checkpoints.sort();
    Ok(checkpoints)
}

fn read_checkpoint_meta(dir: &Path) -> (Option<String>, u64) {
    let meta = fs::read_to_string(dir.join(CHECKPOINT_META)).unwrap_or_default();
    let mut name = None;
    let mut created_unix = 0;
    for line in meta.lines() {
        match line.split_once('=') {
            Some(("name", value)) if !value.is_empty() => name = Some(value.to_string()),
            Some(("created_unix", value)) => created_unix = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    (name, created_unix)
}

/// Older lnx binaries locked instances with pid-file directories that the
/// new flock does not exclude, so migration must not run under one.
fn refuse_live_legacy_owner(layout: &Layout) -> Result<()> {
    for lock in [OLD_LOCK, OLD_START_LOCK] {
        let dir = layout.run_dir.join(lock);
        for pid_file in ["owner.pid", "maintenance.pid", "starter.pid"] {
            let Ok(pid) = fs::read_to_string(dir.join(pid_file)) else {
                continue;
            };
            let Ok(pid) = pid.trim().parse::<libc::pid_t>() else {
                continue;
            };
            if ProcessIdentity::of(pid).is_some() && pid != std::process::id() as libc::pid_t {
                bail!(
                    "an older lnx process (pid {pid}) is still using instance {}; let it finish, then retry",
                    layout.instance
                );
            }
        }
    }
    Ok(())
}

/// Deletes the old layout's files once the store's record exists.
fn remove_legacy_files(layout: &Layout) -> Result<()> {
    let instance = &layout.instance_dir;
    remove_path_if_exists(&instance.join(SNAPSHOTS_DIR))?;
    remove_path_if_exists(&instance.join(ROOTFS))?;
    remove_path_if_exists(&instance.join(HOST_SHARE_STATE))?;
    remove_path_if_exists(&instance.join("vm-initialized"))?;
    for dir in [&layout.run_dir, instance] {
        for name in [OLD_LOCK, OLD_START_LOCK] {
            remove_path_if_exists(&dir.join(name))?;
            let mut guard = name.to_string();
            guard.push_str(".guard");
            remove_path_if_exists(&dir.join(guard))?;
        }
    }
    let checkpoints = instance.join(super::CHECKPOINTS_DIR);
    if let Ok(entries) = fs::read_dir(&checkpoints) {
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                remove_path_if_exists(&entry.path())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
