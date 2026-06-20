use super::*;
use crate::paths::Layout;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("lnx-{name}-{}-{}", std::process::id(), now_unix()));
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
    fs::write(checkpoint.path.join("rootfs.ext4"), b"rootfs").expect("rootfs");
    fs::write(checkpoint.path.join("vmstate.bin"), b"vmstate").expect("vmstate");
    fs::write(checkpoint.path.join("pages.img"), b"pages").expect("pages");
    fs::write(checkpoint.path.join("initramfs.stamp"), b"stamp").expect("stamp");
    write_metadata(&source, &checkpoint).expect("metadata");

    fork(&source, &checkpoint, &dest).expect("fork");

    assert!(!dest.kernel.exists());
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
