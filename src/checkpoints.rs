use std::{
    fs::{self, OpenOptions},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    descriptor,
    paths::{Layout, ensure_instance_transaction_root, instance_transaction_roots},
    runner::{self, SNAPSHOT_RESTORE_FILES},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub id: String,
    pub name: Option<String>,
    pub created_unix: u64,
    pub path: PathBuf,
}

pub fn new_checkpoint_path(layout: &Layout, name: Option<&str>) -> Result<(Checkpoint, PathBuf)> {
    let created_unix = now_unix();
    let now = OffsetDateTime::from_unix_timestamp(created_unix as i64)
        .context("format checkpoint timestamp")?;
    let timestamp = now
        .format(&time::macros::format_description!(
            "[year][month][day]T[hour][minute][second]Z"
        ))
        .context("format checkpoint id timestamp")?;
    let id = format!("{timestamp}-{}", std::process::id());
    let path = layout.checkpoint_dir.join(&id);
    let checkpoint = Checkpoint {
        id,
        name: name.map(ToOwned::to_owned),
        created_unix,
        path: path.clone(),
    };
    Ok((checkpoint, path))
}

pub fn write_metadata(layout: &Layout, checkpoint: &Checkpoint) -> Result<()> {
    fs::create_dir_all(&checkpoint.path)
        .with_context(|| format!("create {}", checkpoint.path.display()))?;
    let mut metadata = String::new();
    metadata.push_str("version=1\n");
    metadata.push_str(&format!("id={}\n", checkpoint.id));
    metadata.push_str(&format!("source_instance={}\n", layout.instance));
    metadata.push_str(&format!("created_unix={}\n", checkpoint.created_unix));
    if let Some(name) = &checkpoint.name {
        metadata.push_str(&format!("name={}\n", sanitize_name(name)));
    }
    fs::write(checkpoint.path.join("checkpoint.meta"), metadata).with_context(|| {
        format!(
            "write {}",
            checkpoint.path.join("checkpoint.meta").display()
        )
    })
}

pub fn list(layout: &Layout) -> Result<Vec<Checkpoint>> {
    let mut checkpoints = Vec::new();
    let entries = match fs::read_dir(&layout.checkpoint_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(checkpoints),
        Err(e) => {
            return Err(e).with_context(|| format!("read {}", layout.checkpoint_dir.display()));
        }
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        checkpoints.push(read_metadata(path, id)?);
    }
    checkpoints.sort_by_key(|checkpoint| checkpoint.created_unix);
    Ok(checkpoints)
}

pub fn resolve(layout: &Layout, identifier: &str) -> Result<Checkpoint> {
    let checkpoints = list(layout)?;
    let matches = checkpoints
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

pub fn delete(layout: &Layout, checkpoint: &Checkpoint) -> Result<()> {
    if !checkpoint.path.starts_with(&layout.checkpoint_dir) {
        bail!(
            "refusing to delete checkpoint outside {}: {}",
            layout.checkpoint_dir.display(),
            checkpoint.path.display()
        );
    }
    fs::remove_dir_all(&checkpoint.path)
        .with_context(|| format!("remove {}", checkpoint.path.display()))
}

pub fn fork(source: &Layout, checkpoint: &Checkpoint, dest: &Layout) -> Result<()> {
    if dest.rootfs.exists() {
        bail!(
            "destination rootfs already exists: {}",
            dest.rootfs.display()
        );
    }
    if dest.snapshot_dir.exists() {
        bail!(
            "destination snapshots already exist: {}",
            dest.snapshot_dir.display()
        );
    }
    validate_memory_checkpoint(&checkpoint.path)?;
    let dest_descriptor = destination_descriptor(source, checkpoint, dest)?;
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
    cleanup_stale_fork_transactions(parent)?;
    let transaction_root = ensure_instance_transaction_root(parent)?;
    let fork_staging_root = transaction_root.join("fork");
    fs::create_dir_all(&fork_staging_root)
        .with_context(|| format!("create {}", fork_staging_root.display()))?;
    let staging_creation_guard = lock_fork_transaction_root(&fork_staging_root)?;
    let staging = tempfile::Builder::new()
        .prefix("fork-")
        .tempdir_in(&fork_staging_root)
        .with_context(|| {
            format!(
                "create fork staging directory in {}",
                fork_staging_root.display()
            )
        })?;
    let staging_lease_path = staging.path().join(".lnx-fork-lease");
    let staging_lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&staging_lease_path)
        .with_context(|| format!("create {}", staging_lease_path.display()))?;
    if unsafe { libc::flock(staging_lease.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("lock {}", staging_lease_path.display()));
    }
    drop(staging_creation_guard);
    let staging_layout = staging_layout(dest, staging.path())?;
    clone_or_copy(&checkpoint.path.join("rootfs.ext4"), &staging_layout.rootfs)?;
    clone_snapshot_dir(
        &checkpoint.path,
        &staging_layout.snapshot_dir.join("latest"),
    )?;
    clone_host_share_state(&checkpoint.path.join("host-share-state"), &staging_layout)?;
    descriptor::save_in_instance_dir(&staging_layout.instance_dir, &dest_descriptor)?;
    mark_vm_initialized(&staging_layout)?;
    if dest.instance_dir.exists() {
        bail!(
            "destination instance appeared while forking: {}",
            dest.instance_dir.display()
        );
    }
    fs::rename(staging.path(), &dest.instance_dir).with_context(|| {
        format!(
            "publish staged fork {} to {}",
            staging.path().display(),
            dest.instance_dir.display()
        )
    })?;
    if let Err(error) = fs::remove_file(dest.instance_dir.join(".lnx-fork-lease")) {
        eprintln!(
            "warning: fork was published but its internal lease file could not be removed from {}: {error}",
            dest.instance_dir.display()
        );
    }
    Ok(())
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
        let entries = match fs::read_dir(&fork_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", fork_root.display()));
            }
        };
        drop(entries);
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
                Ok(lease) => {
                    let locked = unsafe {
                        libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0
                    };
                    if !locked {
                        let error = std::io::Error::last_os_error();
                        let would_block = matches!(
                            error.raw_os_error(),
                            Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK
                        );
                        if !would_block {
                            return Err(error)
                                .with_context(|| format!("lock {}", lease_path.display()));
                        }
                    }
                    locked
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age >= Duration::from_secs(10)),
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

fn validate_memory_checkpoint(path: &Path) -> Result<()> {
    for name in ["rootfs.ext4", "vmstate.bin"] {
        let file = path.join(name);
        let metadata = fs::symlink_metadata(&file).with_context(|| {
            format!(
                "checkpoint is missing required memory state: {}",
                file.display()
            )
        })?;
        if !metadata.is_file() {
            bail!(
                "checkpoint memory state is not a regular file: {}",
                file.display()
            );
        }
    }
    runner::snapshot_vm_config(path)?
        .with_context(|| format!("checkpoint has no VM state: {}", path.display()))?;
    let pages = path.join("pages.img");
    let metadata = fs::symlink_metadata(&pages).with_context(|| {
        format!(
            "checkpoint is missing required memory state: {}",
            pages.display()
        )
    })?;
    if !metadata.is_file() {
        bail!(
            "checkpoint memory state is not a regular file: {}",
            pages.display()
        );
    }
    let initramfs_stamp = path.join("initramfs.stamp");
    let initramfs_metadata = fs::symlink_metadata(&initramfs_stamp).with_context(|| {
        format!(
            "checkpoint is missing initramfs compatibility stamp: {}",
            initramfs_stamp.display()
        )
    })?;
    if !initramfs_metadata.is_file() || runner::initramfs_stamp_key(&initramfs_stamp).is_none() {
        bail!(
            "checkpoint has no valid initramfs compatibility stamp: {}",
            initramfs_stamp.display()
        );
    }
    let launch = path.join("launch.json");
    let launch_metadata = fs::symlink_metadata(&launch).with_context(|| {
        format!(
            "checkpoint is missing launch metadata: {}",
            launch.display()
        )
    })?;
    if !launch_metadata.is_file() {
        bail!(
            "checkpoint launch metadata is not a regular file: {}",
            launch.display()
        );
    }
    runner::read_launch_metadata(path)
        .with_context(|| format!("read checkpoint launch metadata from {}", launch.display()))?;
    if !runner::default_restore_version_matches(path)? {
        bail!(
            "checkpoint launch metadata version is not restorable: {}",
            launch.display()
        );
    }
    let deterministic_stamp = path.join("deterministic.stamp");
    let deterministic_stamp = match fs::read_to_string(&deterministic_stamp) {
        Ok(stamp) => Some(stamp),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| {
                format!("read checkpoint stamp at {}", deterministic_stamp.display())
            });
        }
    };
    if deterministic_stamp
        .as_deref()
        .is_some_and(|stamp| stamp.lines().any(|line| line == "deterministic=true"))
    {
        let clock = path.join("deterministic-clock.state");
        if !fs::symlink_metadata(&clock).is_ok_and(|metadata| metadata.is_file()) {
            bail!(
                "deterministic checkpoint is missing clock state: {}",
                clock.display()
            );
        }
    }
    Ok(())
}

fn staging_layout(dest: &Layout, staging_dir: &Path) -> Result<Layout> {
    let relocate = |path: &Path| -> Result<PathBuf> {
        let relative = path.strip_prefix(&dest.instance_dir).with_context(|| {
            format!(
                "destination path {} is outside instance {}",
                path.display(),
                dest.instance_dir.display()
            )
        })?;
        Ok(staging_dir.join(relative))
    };
    let staging_instance = staging_dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("fork staging directory name is not UTF-8")?
        .to_string();
    Ok(Layout {
        base: dest.base.clone(),
        instance: staging_instance,
        kernel: dest.kernel.clone(),
        rootfs: relocate(&dest.rootfs)?,
        instance_dir: staging_dir.to_path_buf(),
        snapshot_dir: relocate(&dest.snapshot_dir)?,
        checkpoint_dir: relocate(&dest.checkpoint_dir)?,
        vm_initialized: relocate(&dest.vm_initialized)?,
        // Fork publication only materializes persistent instance state. A
        // split runtime directory intentionally lives outside instance_dir,
        // so use staging-local placeholders for descriptor helpers.
        run_dir: staging_dir.to_path_buf(),
        console_log: staging_dir.join("console.log"),
    })
}

fn destination_descriptor(
    source: &Layout,
    checkpoint: &Checkpoint,
    dest: &Layout,
) -> Result<descriptor::InstanceDescriptor> {
    let mut config = descriptor::load(source)?;
    config.name = Some(dest.instance.clone());
    config.created = OffsetDateTime::now_utc().format(&Rfc3339).ok();
    if let Some(snapshot_config) = runner::snapshot_vm_config(&checkpoint.path)? {
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

fn mark_vm_initialized(layout: &Layout) -> Result<()> {
    if let Some(parent) = layout.vm_initialized.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(&layout.vm_initialized, b"1\n")
        .with_context(|| format!("write {}", layout.vm_initialized.display()))
}

pub fn display_time(created_unix: u64) -> String {
    OffsetDateTime::from_unix_timestamp(created_unix as i64)
        .ok()
        .and_then(|time| time.format(&Rfc3339).ok())
        .unwrap_or_else(|| created_unix.to_string())
}

fn read_metadata(path: PathBuf, fallback_id: String) -> Result<Checkpoint> {
    let metadata = fs::read_to_string(path.join("checkpoint.meta")).unwrap_or_default();
    let mut id = fallback_id;
    let mut name = None;
    let mut created_unix = metadata_created_unix(&path)?;
    for line in metadata.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "id" => id = value.to_string(),
            "name" if !value.is_empty() => name = Some(value.to_string()),
            "created_unix" => {
                if let Ok(value) = value.parse() {
                    created_unix = value;
                }
            }
            _ => {}
        }
    }
    Ok(Checkpoint {
        id,
        name,
        created_unix,
        path,
    })
}

fn metadata_created_unix(path: &Path) -> Result<u64> {
    let modified = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH);
    Ok(modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn sanitize_name(name: &str) -> String {
    name.replace(['\r', '\n'], " ").trim().to_string()
}

fn clone_snapshot_dir(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    for name in SNAPSHOT_RESTORE_FILES
        .iter()
        .copied()
        .chain(["checkpoint.meta"])
    {
        let src_file = src.join(name);
        match fs::symlink_metadata(&src_file) {
            Ok(metadata) if metadata.is_file() => {
                clone_or_copy(&src_file, &dest.join(name))?;
            }
            Ok(_) => bail!(
                "checkpoint restore state is not a regular file: {}",
                src_file.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("stat {}", src_file.display()));
            }
        }
    }
    Ok(())
}

fn clone_host_share_state(src: &Path, dest: &Layout) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    clone_tree(src, &dest.instance_dir.join("host-share-state"))
}

fn clone_tree(src: &Path, dest: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
    if metadata.is_dir() {
        fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
        for entry in fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
            let entry = entry.with_context(|| format!("read {}", src.display()))?;
            clone_tree(&entry.path(), &dest.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if metadata.file_type().is_symlink() {
        let link = fs::read_link(src).with_context(|| format!("readlink {}", src.display()))?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        std::os::unix::fs::symlink(&link, dest)
            .with_context(|| format!("symlink {} to {}", link.display(), dest.display()))?;
        return Ok(());
    }
    clone_or_copy(src, dest)
}

fn clone_or_copy(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    crate::sparse_copy::clone_or_copy_file(src, dest)
}

#[cfg(test)]
mod tests;
