use super::*;
use std::path::Path;

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
fn settings_round_trip_and_merge_with_identity() {
    let base = std::env::temp_dir().join(format!("lnx-descriptor-{}", std::process::id()));
    let layout = layout(&base);

    assert!(load(&layout).expect("load missing").cpus.is_none());

    let descriptor = InstanceDescriptor {
        cpus: Some(3),
        memory_mib: Some(2048),
        ..Default::default()
    };
    save(&layout, &descriptor).expect("save settings");
    ensure_identity(&layout, "release:test-v1").expect("ensure identity");

    let loaded = load(&layout).expect("load");
    assert_eq!(loaded.cpus, Some(3));
    assert_eq!(loaded.memory_mib, Some(2048));
    assert_eq!(loaded.name.as_deref(), Some("test"));
    assert_eq!(loaded.image.as_deref(), Some("release:test-v1"));
    assert!(loaded.created.is_some());

    let _ = fs::remove_dir_all(&base);
}
