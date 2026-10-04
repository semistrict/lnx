//! Checkpoints and forks, on top of an instance's store.
//!
//! A checkpoint is a named reference to a generation (rule C1 in
//! `specs/tla/README.md`). Checkpointing a stopped instance references its
//! latest generation, which is instant; checkpointing a running one asks the
//! VM owner for a live capture. Forking copies a pinned generation into a new
//! instance (C4).

use std::{
    fs::{self, OpenOptions},
    os::fd::AsRawFd,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    descriptor,
    paths::{Layout, ensure_instance_transaction_root, instance_transaction_roots},
    runner::{self, CheckpointSpec},
    store::{CheckpointRef, GenerationId, Store},
};

const LIVE_CAPTURE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub id: String,
    pub name: Option<String>,
    pub created_unix: u64,
    pub generation: GenerationId,
}

impl From<CheckpointRef> for Checkpoint {
    fn from(checkpoint: CheckpointRef) -> Self {
        Self {
            id: checkpoint.id,
            name: checkpoint.name,
            created_unix: checkpoint.created_unix,
            generation: checkpoint.generation,
        }
    }
}

/// A new checkpoint id: when, which process, and how many this process has
/// made before, so ids never repeat.
fn new_id() -> Result<String> {
    static MADE: AtomicU64 = AtomicU64::new(0);
    let timestamp = OffsetDateTime::now_utc()
        .format(&time::macros::format_description!(
            "[year][month][day]T[hour][minute][second]Z"
        ))
        .context("format checkpoint id timestamp")?;
    let sequence = MADE.fetch_add(1, Ordering::Relaxed);
    Ok(format!("{timestamp}-{}-{sequence}", std::process::id()))
}

fn sanitize_name(name: &str) -> String {
    name.replace(['\r', '\n'], " ").trim().to_string()
}

/// Checkpoints the instance as it is now. A running VM is captured live; a
/// stopped instance's latest generation is referenced as is.
pub fn create(layout: &Layout, name: Option<&str>) -> Result<Checkpoint> {
    runner::ensure_store(layout)?;
    let id = new_id()?;
    let name = name.map(sanitize_name);
    if runner::live_owner(layout).is_some() {
        runner::request_live_checkpoint(
            layout,
            &CheckpointSpec {
                id: Some(id.clone()),
                name: name.clone(),
                export_to: None,
            },
            None,
            LIVE_CAPTURE_TIMEOUT,
        )
        .context("checkpoint running VM")?;
        return resolve(layout, &id);
    }
    let created = runner::with_exclusive_instance_state(layout, |lock, _| {
        let store = Store::new(&layout.instance_dir);
        let latest = store
            .record()?
            .and_then(|record| record.latest)
            .with_context(|| format!("instance {} has no saved state", layout.instance))?;
        let checkpoint = CheckpointRef {
            id: id.clone(),
            name: name.clone(),
            generation: latest,
            created_unix: now_unix(),
        };
        store.add_checkpoint(lock, &checkpoint)?;
        Ok(Checkpoint::from(checkpoint))
    })?;
    created.with_context(|| {
        format!(
            "instance {} started while the checkpoint was being taken; retry",
            layout.instance
        )
    })
}

pub fn list(layout: &Layout) -> Result<Vec<Checkpoint>> {
    runner::ensure_store(layout)?;
    Ok(Store::new(&layout.instance_dir)
        .checkpoints()?
        .into_iter()
        .map(Checkpoint::from)
        .collect())
}

pub fn resolve(layout: &Layout, identifier: &str) -> Result<Checkpoint> {
    let matches = list(layout)?
        .into_iter()
        .filter(|checkpoint| {
            checkpoint.id == identifier || checkpoint.name.as_deref() == Some(identifier)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [checkpoint] => Ok(checkpoint.clone()),
        [] => bail!("checkpoint not found: {identifier}"),
        _ => bail!("checkpoint name is ambiguous: {identifier}"),
    }
}

/// Removes a checkpoint's reference; its generation is reclaimed once nothing
/// else names it. Needs the instance lock, so not while the VM runs.
pub fn delete(layout: &Layout, checkpoint: &Checkpoint) -> Result<()> {
    let deleted = runner::with_exclusive_instance_state(layout, |lock, _| {
        Store::new(&layout.instance_dir).remove_checkpoint(lock, &checkpoint.id)
    })?;
    if deleted.is_none() {
        bail!(
            "cannot delete a checkpoint while instance {} is running or another state operation is in progress",
            layout.instance
        );
    }
    Ok(())
}

/// The name of the checkpoint `restore` keeps the replaced state under.
pub const BEFORE_RESTORE: &str = "before-restore";

/// Rolls the instance back to `checkpoint`. The state it replaces is kept as
/// the checkpoint named [`BEFORE_RESTORE`] (replacing an older one), so a
/// restore can itself be undone. The instance must be stopped.
pub fn restore(layout: &Layout, checkpoint: &Checkpoint) -> Result<Option<Checkpoint>> {
    let restored = runner::with_exclusive_instance_state(layout, |lock, _| {
        let store = Store::new(&layout.instance_dir);
        store.recover(lock)?;
        let previous = store
            .record()?
            .and_then(|record| record.latest)
            .with_context(|| format!("instance {} has no saved state", layout.instance))?;
        if previous == checkpoint.generation {
            return Ok(None);
        }
        let before = CheckpointRef {
            id: new_id()?,
            name: Some(BEFORE_RESTORE.to_string()),
            generation: previous,
            created_unix: now_unix(),
        };
        store.add_checkpoint(lock, &before)?;
        store.commit_latest(lock, &checkpoint.generation)?;
        for older in store.checkpoints()? {
            if older.name.as_deref() == Some(BEFORE_RESTORE) && older.id != before.id {
                store.remove_checkpoint(lock, &older.id)?;
            }
        }
        Ok(Some(Checkpoint::from(before)))
    })?;
    restored.with_context(|| {
        format!(
            "instance {} started while it was being restored; retry",
            layout.instance
        )
    })
}

/// What a fork starts from.
pub enum ForkSource<'a> {
    Checkpoint(&'a Checkpoint),
    /// The source instance as it is now, captured live if it is running.
    Current,
}

/// Creates instance `dest` from `source`.
pub fn fork(source: &Layout, from: ForkSource<'_>, dest: &Layout) -> Result<()> {
    runner::ensure_store(source)?;
    if dest.instance_dir.exists() {
        bail!(
            "destination instance already exists: {}",
            dest.instance_dir.display()
        );
    }
    let parent = dest
        .instance_dir
        .parent()
        .context("destination instance has no parent")?;
    if parent != dest.base.join("instances") {
        bail!(
            "destination instance is outside its instance store: {}",
            dest.instance_dir.display()
        );
    }
    let staging = StagedInstance::create(parent)?;
    let store = Store::new(&source.instance_dir);
    match from {
        ForkSource::Checkpoint(checkpoint) => {
            let pin = store
                .pin(&checkpoint.generation)
                .with_context(|| format!("checkpoint {} cannot be forked", checkpoint.id))?;
            Store::export(&pin, staging.dir())?;
        }
        ForkSource::Current if runner::live_owner(source).is_some() => {
            runner::request_live_checkpoint(
                source,
                &CheckpointSpec {
                    id: None,
                    name: None,
                    export_to: Some(staging.dir().to_path_buf()),
                },
                None,
                LIVE_CAPTURE_TIMEOUT,
            )
            .context("capture running source instance")?;
        }
        ForkSource::Current => {
            runner::refuse_crashed_run(source)?;
            let latest = store
                .record()?
                .and_then(|record| record.latest)
                .with_context(|| format!("instance {} has no saved state", source.instance))?;
            let pin = store.pin(&latest).with_context(|| {
                format!(
                    "instance {} changed during the fork; retry",
                    source.instance
                )
            })?;
            Store::export(&pin, staging.dir())?;
        }
    }
    let exported = Store::new(staging.dir())
        .latest()?
        .context("forked instance has no state")?;
    descriptor::save_in_instance_dir(
        staging.dir(),
        &destination_descriptor(source, dest, &exported.dir)?,
    )?;
    staging.publish(&dest.instance_dir)
}

fn destination_descriptor(
    source: &Layout,
    dest: &Layout,
    generation: &Path,
) -> Result<descriptor::InstanceDescriptor> {
    let mut config = descriptor::load(source)?;
    config.name = Some(dest.instance.clone());
    config.created = OffsetDateTime::now_utc().format(&Rfc3339).ok();
    if let Some(snapshot_config) = runner::snapshot_vm_config(generation)? {
        config.cpus = Some(
            snapshot_config
                .vcpu_count
                .try_into()
                .context("checkpoint vCPU count does not fit instance descriptor")?,
        );
        config.memory_mib = Some(
            snapshot_config
                .memory_mib()
                .try_into()
                .context("checkpoint memory does not fit instance descriptor")?,
        );
    }
    Ok(config)
}

/// A new instance directory assembled in the instances root's transaction
/// area and published by one rename. A crashed fork leaves only a staging
/// directory, which a later fork removes once its lease is free.
struct StagedInstance {
    dir: tempfile::TempDir,
    _lease: fs::File,
}

impl StagedInstance {
    fn create(instances_root: &Path) -> Result<Self> {
        cleanup_stale_fork_transactions(instances_root)?;
        let transaction_root = ensure_instance_transaction_root(instances_root)?;
        let fork_root = transaction_root.join("fork");
        fs::create_dir_all(&fork_root)
            .with_context(|| format!("create {}", fork_root.display()))?;
        let creation_guard = lock_fork_transaction_root(&fork_root)?;
        let dir = tempfile::Builder::new()
            .prefix("fork-")
            .tempdir_in(&fork_root)
            .with_context(|| format!("create fork staging directory in {}", fork_root.display()))?;
        let lease_path = dir.path().join(".lnx-fork-lease");
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lease_path)
            .with_context(|| format!("create {}", lease_path.display()))?;
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("lock {}", lease_path.display()));
        }
        drop(creation_guard);
        Ok(Self { dir, _lease: lease })
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }

    fn publish(self, dest: &Path) -> Result<()> {
        if dest.exists() {
            bail!(
                "destination instance appeared while forking: {}",
                dest.display()
            );
        }
        let staged = self.dir.keep();
        fs::rename(&staged, dest).with_context(|| {
            format!(
                "publish staged fork {} to {}",
                staged.display(),
                dest.display()
            )
        })?;
        if let Err(error) = fs::remove_file(dest.join(".lnx-fork-lease")) {
            eprintln!(
                "warning: fork was published but its internal lease file could not be removed from {}: {error}",
                dest.display()
            );
        }
        Ok(())
    }
}

fn lock_fork_transaction_root(fork_root: &Path) -> Result<fs::File> {
    let path = fork_root.join(".guard");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("lock {}", path.display()));
    }
    Ok(file)
}

fn cleanup_stale_fork_transactions(instances_root: &Path) -> Result<()> {
    for transaction_root in instance_transaction_roots(instances_root)? {
        let fork_root = transaction_root.join("fork");
        if !fork_root.exists() {
            continue;
        }
        let _guard = lock_fork_transaction_root(&fork_root)?;
        let entries =
            fs::read_dir(&fork_root).with_context(|| format!("read {}", fork_root.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("read {}", fork_root.display()))?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            let lease_path = path.join(".lnx-fork-lease");
            let stale = match OpenOptions::new().read(true).write(true).open(&lease_path) {
                Ok(lease) => unsafe {
                    libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0
                },
                // Staging directories get their lease while the creation guard
                // is held, so one without a lease belongs to a dead creator.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(error) => {
                    return Err(error).with_context(|| format!("open {}", lease_path.display()));
                }
            };
            if stale {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("remove stale fork staging {}", path.display()))?;
            }
        }
    }
    Ok(())
}

pub fn display_time(created_unix: u64) -> String {
    OffsetDateTime::from_unix_timestamp(created_unix as i64)
        .ok()
        .and_then(|time| time.format(&Rfc3339).ok())
        .unwrap_or_else(|| created_unix.to_string())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests;
