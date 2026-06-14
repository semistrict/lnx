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
    fs::create_dir_all(
        dest.rootfs
            .parent()
            .context("destination rootfs has no parent")?,
    )
    .with_context(|| format!("create {}", dest.rootfs.parent().unwrap().display()))?;
    clone_or_copy(&checkpoint.path.join("rootfs.ext4"), &dest.rootfs)?;
    clone_snapshot_dir(&checkpoint.path, &dest.snapshot_dir.join("latest"))?;
    mark_vm_initialized(dest)?;
    if source.kernel.exists() && !dest.kernel.exists() {
        clone_or_copy(&source.kernel, &dest.kernel)?;
    }
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
        "checkpoint.meta",
        "initramfs.stamp",
        "shares.stamp",
    ] {
        let src_file = src.join(name);
        if src_file.exists() {
            clone_or_copy(&src_file, &dest.join(name))?;
        }
    }
    Ok(())
}

fn clone_or_copy(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    crate::sparse_copy::clone_or_copy_file(src, dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Layout;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "lnx-{name}-{}-{}",
                std::process::id(),
                now_unix()
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn layout(base: &Path) -> Layout {
        Layout {
            base: base.to_path_buf(),
            instance: "test".to_string(),
            kernel: base.join("vmlinuz"),
            rootfs: base.join("instances/test/rootfs.ext4"),
            instance_dir: base.join("instances/test"),
            snapshot_dir: base.join("instances/test/memory-snapshots"),
            checkpoint_dir: base.join("instances/test/checkpoints"),
            vm_initialized: base.join("instances/test/vm-initialized"),
            run_dir: base.join("instances/test"),
            console_log: base.join("instances/test/console.log"),
        }
    }

    #[test]
    fn metadata_round_trips_named_checkpoint() {
        let temp = TempDir::new("checkpoint-meta");
        let layout = layout(&temp.path);
        let (checkpoint, _) = new_checkpoint_path(&layout, Some("browser ready")).expect("new");
        fs::create_dir_all(&checkpoint.path).expect("create checkpoint");
        fs::write(checkpoint.path.join("vmstate.bin"), b"state").expect("vmstate");

        write_metadata(&layout, &checkpoint).expect("write metadata");
        let listed = list(&layout).expect("list checkpoints");

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name.as_deref(), Some("browser ready"));
        assert_eq!(
            resolve(&layout, "browser ready").expect("resolve").id,
            checkpoint.id
        );
    }

    #[test]
    fn resolve_rejects_ambiguous_checkpoint_names() {
        let temp = TempDir::new("checkpoint-ambiguous");
        let layout = layout(&temp.path);
        for id in ["one", "two"] {
            let checkpoint = Checkpoint {
                id: id.to_string(),
                name: Some("same-name".to_string()),
                created_unix: now_unix(),
                path: layout.checkpoint_dir.join(id),
            };
            write_metadata(&layout, &checkpoint).expect("write metadata");
        }

        let err = resolve(&layout, "same-name").expect_err("ambiguous name should fail");
        assert!(err.to_string().contains("ambiguous"));
    }

    #[test]
    fn fork_clones_checkpoint_files_to_destination_layout() {
        let temp = TempDir::new("checkpoint-fork");
        let source = layout(&temp.path.join("source"));
        let dest = Layout {
            base: temp.path.join("dest"),
            instance: "forked".to_string(),
            kernel: temp.path.join("dest/vmlinuz"),
            rootfs: temp.path.join("dest/instances/forked/rootfs.ext4"),
            instance_dir: temp.path.join("dest/instances/forked"),
            snapshot_dir: temp.path.join("dest/instances/forked/memory-snapshots"),
            checkpoint_dir: temp.path.join("dest/instances/forked/checkpoints"),
            vm_initialized: temp.path.join("dest/instances/forked/vm-initialized"),
            run_dir: temp.path.join("dest/instances/forked"),
            console_log: temp.path.join("dest/instances/forked/console.log"),
        };
        let checkpoint = Checkpoint {
            id: "checkpoint".to_string(),
            name: Some("checkpoint".to_string()),
            created_unix: now_unix(),
            path: source.checkpoint_dir.join("checkpoint"),
        };
        fs::create_dir_all(&checkpoint.path).expect("create checkpoint");
        fs::create_dir_all(source.kernel.parent().unwrap()).expect("create kernel parent");
        fs::write(&source.kernel, b"kernel").expect("kernel");
        fs::write(checkpoint.path.join("rootfs.ext4"), b"rootfs").expect("rootfs");
        fs::write(checkpoint.path.join("vmstate.bin"), b"vmstate").expect("vmstate");
        fs::write(checkpoint.path.join("pages.img"), b"pages").expect("pages");
        fs::write(checkpoint.path.join("initramfs.stamp"), b"stamp").expect("stamp");
        write_metadata(&source, &checkpoint).expect("metadata");

        fork(&source, &checkpoint, &dest).expect("fork");

        assert_eq!(fs::read(&dest.kernel).expect("read kernel"), b"kernel");
        assert_eq!(fs::read(&dest.rootfs).expect("read rootfs"), b"rootfs");
        assert_eq!(
            fs::read(dest.snapshot_dir.join("latest/vmstate.bin")).expect("read vmstate"),
            b"vmstate"
        );
        assert_eq!(
            fs::read(dest.snapshot_dir.join("latest/pages.img")).expect("read pages"),
            b"pages"
        );
        assert_eq!(
            fs::read(dest.snapshot_dir.join("latest/initramfs.stamp")).expect("read stamp"),
            b"stamp"
        );
        assert!(dest.snapshot_dir.join("latest/checkpoint.meta").exists());
        assert_eq!(
            fs::read(&dest.vm_initialized).expect("read initialized marker"),
            b"1\n"
        );
    }

    #[test]
    fn fork_rejects_existing_destination_rootfs() {
        let temp = TempDir::new("checkpoint-fork-existing");
        let source = layout(&temp.path.join("source"));
        let mut dest = layout(&temp.path.join("dest"));
        dest.instance = "dest".to_string();
        let checkpoint = Checkpoint {
            id: "checkpoint".to_string(),
            name: None,
            created_unix: now_unix(),
            path: source.checkpoint_dir.join("checkpoint"),
        };
        fs::create_dir_all(dest.rootfs.parent().unwrap()).expect("create dest parent");
        fs::write(&dest.rootfs, b"existing").expect("existing rootfs");

        let err = fork(&source, &checkpoint, &dest).expect_err("existing rootfs should fail");

        assert!(
            err.to_string()
                .contains("destination rootfs already exists")
        );
    }
}
