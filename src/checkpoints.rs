use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::paths::Layout;

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

pub fn fork(_source: &Layout, checkpoint: &Checkpoint, dest: &Layout) -> Result<()> {
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
    fs::create_dir_all(
        dest.rootfs
            .parent()
            .context("destination rootfs has no parent")?,
    )
    .with_context(|| format!("create {}", dest.rootfs.parent().unwrap().display()))?;
    clone_or_copy(&checkpoint.path.join("rootfs.ext4"), &dest.rootfs)?;
    clone_snapshot_dir(&checkpoint.path, &dest.snapshot_dir.join("latest"))?;
    clone_host_share_state(&checkpoint.path.join("host-share-state"), dest)?;
    mark_vm_initialized(dest)?;
    Ok(())
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
    for name in [
        "vmstate.bin",
        "pages.img",
        "rootfs.ext4",
        "snapshot.meta",
        "checkpoint.meta",
        "initramfs.stamp",
        "launch.json",
    ] {
        let src_file = src.join(name);
        if src_file.exists() {
            clone_or_copy(&src_file, &dest.join(name))?;
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
