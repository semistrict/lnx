use super::*;
use crate::{
    descriptor::{self, InstanceDescriptor},
    paths::Layout,
};

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

fn write_vmstate(path: &Path, cpus: u32, memory_mib: u64) {
    let mut header = [0u8; 40];
    header[0..8].copy_from_slice(b"LKRNSS01");
    header[8..12].copy_from_slice(&runner::SNAPSHOT_VMSTATE_VERSION.to_le_bytes());
    header[16..24].copy_from_slice(&(memory_mib * 1024 * 1024).to_le_bytes());
    header[32..36].copy_from_slice(&cpus.to_le_bytes());
    fs::write(path, header).expect("write vmstate");
}

fn write_restore_compatibility_metadata(path: &Path) {
    fs::write(path.join("initramfs.stamp"), b"source=test-agent\n").expect("write initramfs stamp");
    fs::write(
        path.join("launch.json"),
        br#"{
  "version": 2,
  "owner_args": [],
  "compatibility": { "host_share_cache": { "dax": true } },
  "shares": {
    "no_host_shares": true,
    "host_home": null,
    "outside_home_cwd": null
  }
}"#,
    )
    .expect("write launch metadata");
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
fn delete_removes_checkpoint_directory() {
    let temp = TempDir::new("checkpoint-delete");
    let layout = layout(&temp.path);
    let (checkpoint, _) = new_checkpoint_path(&layout, None).expect("new");
    write_metadata(&layout, &checkpoint).expect("write metadata");
    assert_eq!(list(&layout).expect("list checkpoints").len(), 1);

    delete(&layout, &checkpoint).expect("delete checkpoint");

    assert!(!checkpoint.path.exists());
    assert_eq!(list(&layout).expect("list checkpoints"), Vec::new());
}

#[test]
fn delete_refuses_path_outside_checkpoint_dir() {
    let temp = TempDir::new("checkpoint-delete-outside");
    let layout = layout(&temp.path);
    let outside = temp.path.join("outside-checkpoint");
    fs::create_dir_all(&outside).expect("create outside dir");
    let checkpoint = Checkpoint {
        id: "outside".to_string(),
        name: None,
        created_unix: now_unix(),
        path: outside.clone(),
    };

    let err = delete(&layout, &checkpoint).expect_err("delete outside checkpoint_dir should fail");

    assert!(err.to_string().contains("refusing to delete"));
    assert!(outside.exists());
}

#[test]
fn resolve_then_delete_by_name() {
    let temp = TempDir::new("checkpoint-delete-by-name");
    let layout = layout(&temp.path);
    let (checkpoint, _) = new_checkpoint_path(&layout, Some("named")).expect("new");
    write_metadata(&layout, &checkpoint).expect("write metadata");

    let resolved = resolve(&layout, "named").expect("resolve by name");
    delete(&layout, &resolved).expect("delete resolved checkpoint");

    assert_eq!(list(&layout).expect("list checkpoints"), Vec::new());
}

#[test]
fn fork_clones_checkpoint_files_and_deterministic_state_to_destination_layout() {
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
    write_vmstate(&checkpoint.path.join("vmstate.bin"), 2, 4096);
    fs::write(checkpoint.path.join("pages.img"), b"pages").expect("pages");
    write_restore_compatibility_metadata(&checkpoint.path);
    fs::write(
        checkpoint.path.join("deterministic.stamp"),
        b"deterministic=true\nseed=fork-seed\n",
    )
    .expect("deterministic stamp");
    fs::write(
        checkpoint.path.join("deterministic-clock.state"),
        b"clock_state=deterministic-clock-state-v1\nevent_sequence=42\n",
    )
    .expect("deterministic clock state");
    fs::write(
        checkpoint.path.join("snapshot.meta"),
        b"generation_id=snapshot-1\n",
    )
    .expect("snapshot meta");
    fs::create_dir_all(checkpoint.path.join("host-share-state/home/upper/src/app"))
        .expect("create host-share state");
    fs::write(
        checkpoint
            .path
            .join("host-share-state/home/upper/src/app/.wrangler-state"),
        b"worker state",
    )
    .expect("write host-share state");
    write_metadata(&source, &checkpoint).expect("metadata");

    fork(&source, &checkpoint, &dest).expect("fork");

    assert!(!dest.kernel.exists());
    assert_eq!(fs::read(&dest.rootfs).expect("read rootfs"), b"rootfs");
    assert!(dest.snapshot_dir.join("latest/vmstate.bin").exists());
    assert_eq!(
        fs::read(dest.snapshot_dir.join("latest/pages.img")).expect("read pages"),
        b"pages"
    );
    assert_eq!(
        fs::read(dest.snapshot_dir.join("latest/initramfs.stamp")).expect("read stamp"),
        b"source=test-agent\n"
    );
    assert_eq!(
        fs::read(dest.snapshot_dir.join("latest/deterministic.stamp"))
            .expect("read deterministic stamp"),
        b"deterministic=true\nseed=fork-seed\n"
    );
    assert_eq!(
        fs::read(dest.snapshot_dir.join("latest/deterministic-clock.state"))
            .expect("read deterministic clock state"),
        b"clock_state=deterministic-clock-state-v1\nevent_sequence=42\n"
    );
    assert_eq!(
        fs::read(dest.snapshot_dir.join("latest/snapshot.meta")).expect("read snapshot meta"),
        b"generation_id=snapshot-1\n"
    );
    assert!(dest.snapshot_dir.join("latest/checkpoint.meta").exists());
    assert_eq!(
        fs::read(&dest.vm_initialized).expect("read initialized marker"),
        b"1\n"
    );
    assert_eq!(
        fs::read(
            dest.instance_dir
                .join("host-share-state/home/upper/src/app/.wrangler-state")
        )
        .expect("read host-share state"),
        b"worker state"
    );
}

#[test]
fn fork_supports_a_split_runtime_directory() {
    let temp = TempDir::new("checkpoint-fork-split-runtime");
    let source = layout(&temp.path.join("source"));
    let persistent_base = temp.path.join("destination");
    let mut dest = layout(&persistent_base);
    dest.instance = "forked".to_string();
    dest.instance_dir = persistent_base.join("instances/forked");
    dest.rootfs = dest.instance_dir.join("rootfs.ext4");
    dest.snapshot_dir = dest.instance_dir.join("memory-snapshots");
    dest.checkpoint_dir = dest.instance_dir.join("checkpoints");
    dest.vm_initialized = dest.instance_dir.join("vm-initialized");
    dest.run_dir = temp.path.join("runtime/instances/forked");
    dest.console_log = dest.run_dir.join("console.log");
    let checkpoint = Checkpoint {
        id: "checkpoint".to_string(),
        name: None,
        created_unix: now_unix(),
        path: source.checkpoint_dir.join("checkpoint"),
    };
    fs::create_dir_all(&checkpoint.path).expect("create checkpoint");
    fs::write(checkpoint.path.join("rootfs.ext4"), b"rootfs").expect("rootfs");
    write_vmstate(&checkpoint.path.join("vmstate.bin"), 2, 4096);
    fs::write(checkpoint.path.join("pages.img"), b"pages").expect("pages");
    write_restore_compatibility_metadata(&checkpoint.path);

    fork(&source, &checkpoint, &dest).expect("fork with split runtime");

    assert_eq!(fs::read(&dest.rootfs).expect("read rootfs"), b"rootfs");
    assert!(dest.snapshot_dir.join("latest/vmstate.bin").exists());
    assert!(
        !dest.run_dir.exists(),
        "fork does not publish runtime state"
    );
    let transaction_root =
        crate::paths::existing_instance_transaction_root(&persistent_base.join("instances"))
            .expect("read transaction root")
            .expect("transaction root");
    let staging_remains = fs::read_dir(transaction_root.join("fork"))
        .expect("read fork staging")
        .filter_map(Result::ok)
        .any(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()));
    assert!(!staging_remains, "staging directory must be cleaned up");
}

#[test]
fn fork_preserves_source_vm_settings_with_destination_identity() {
    let temp = TempDir::new("checkpoint-fork-settings");
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
    descriptor::save(
        &source,
        &InstanceDescriptor {
            name: Some(source.instance.clone()),
            created: Some("2026-01-02T03:04:05Z".to_string()),
            image: Some("release:test-image".to_string()),
            cpus: Some(3),
            memory_mib: Some(3072),
        },
    )
    .expect("save source descriptor");
    let checkpoint = Checkpoint {
        id: "checkpoint".to_string(),
        name: None,
        created_unix: now_unix(),
        path: source.checkpoint_dir.join("checkpoint"),
    };
    fs::create_dir_all(&checkpoint.path).expect("create checkpoint");
    fs::write(checkpoint.path.join("rootfs.ext4"), b"rootfs").expect("rootfs");
    write_vmstate(&checkpoint.path.join("vmstate.bin"), 5, 6144);
    fs::write(checkpoint.path.join("pages.img"), b"pages").expect("pages");
    write_restore_compatibility_metadata(&checkpoint.path);

    fork(&source, &checkpoint, &dest).expect("fork");

    let forked = descriptor::load(&dest).expect("load destination descriptor");
    assert_eq!(forked.name.as_deref(), Some("forked"));
    assert_ne!(forked.created.as_deref(), Some("2026-01-02T03:04:05Z"));
    assert!(forked.created.is_some());
    assert_eq!(forked.image.as_deref(), Some("release:test-image"));
    assert_eq!(forked.cpus, Some(5));
    assert_eq!(forked.memory_mib, Some(6144));
}

#[test]
fn fork_validates_snapshot_before_materializing_destination() {
    let temp = TempDir::new("checkpoint-fork-prevalidation");
    let source = layout(&temp.path.join("source"));
    let mut dest = layout(&temp.path.join("dest"));
    dest.instance = "forked".to_string();
    dest.rootfs = dest.instance_dir.join("rootfs.ext4");
    let checkpoint = Checkpoint {
        id: "checkpoint".to_string(),
        name: None,
        created_unix: now_unix(),
        path: source.checkpoint_dir.join("checkpoint"),
    };
    fs::create_dir_all(&checkpoint.path).expect("create checkpoint");
    fs::write(checkpoint.path.join("rootfs.ext4"), b"rootfs").expect("rootfs");
    fs::write(checkpoint.path.join("vmstate.bin"), b"invalid").expect("invalid vmstate");

    let error = fork(&source, &checkpoint, &dest).expect_err("reject malformed vmstate");

    assert!(format!("{error:#}").contains("failed to fill whole buffer"));
    assert!(!dest.rootfs.exists());
    assert!(!dest.snapshot_dir.exists());
    assert!(!descriptor::path(&dest).exists());
}

#[test]
fn fork_rejects_an_incomplete_memory_checkpoint() {
    let temp = TempDir::new("checkpoint-fork-incomplete-memory");
    let source = layout(&temp.path.join("source"));
    let dest = layout(&temp.path.join("dest"));
    let checkpoint = Checkpoint {
        id: "checkpoint".to_string(),
        name: None,
        created_unix: now_unix(),
        path: source.checkpoint_dir.join("checkpoint"),
    };
    fs::create_dir_all(&checkpoint.path).expect("create checkpoint");
    fs::write(checkpoint.path.join("rootfs.ext4"), b"rootfs").expect("rootfs");
    write_vmstate(&checkpoint.path.join("vmstate.bin"), 2, 4096);

    let error = fork(&source, &checkpoint, &dest).expect_err("reject missing pages");

    assert!(error.to_string().contains("pages.img"));
    assert!(!dest.instance_dir.exists());
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

#[test]
fn fork_scavenges_staging_owned_by_a_dead_process() {
    let temp = TempDir::new("checkpoint-fork-stale-staging");
    let instances = temp.path.join("instances");
    let transaction_root = crate::paths::ensure_instance_transaction_root(&instances)
        .expect("create transaction root");
    let stale = transaction_root.join("fork/stale");
    fs::create_dir_all(&stale).expect("create stale staging");
    fs::write(stale.join(".lnx-fork-lease"), b"").expect("write unlocked stale lease");
    fs::write(stale.join("large-partial-state"), b"partial").expect("write partial state");

    cleanup_stale_fork_transactions(&instances).expect("scavenge stale fork");

    assert!(!stale.exists());
}

#[test]
fn fork_scavenger_preserves_a_locked_live_staging_directory() {
    let temp = TempDir::new("checkpoint-fork-live-staging");
    let instances = temp.path.join("instances");
    let transaction_root = crate::paths::ensure_instance_transaction_root(&instances)
        .expect("create transaction root");
    let live = transaction_root.join("fork/live");
    fs::create_dir_all(&live).expect("create live staging");
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(live.join(".lnx-fork-lease"))
        .expect("create live lease");
    assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX) }, 0);

    cleanup_stale_fork_transactions(&instances).expect("scan live fork");

    assert!(live.exists());
}
