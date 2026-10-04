use super::*;
use crate::runner::test_support::hold_as_owner;
use crate::store::{Phase, Recovery};

struct Fixture {
    _temp: tempfile::TempDir,
    layout: Layout,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let layout = Layout::resolve_in_base("vm", temp.path().to_path_buf(), None, None);
        fs::create_dir_all(&layout.instance_dir).expect("create instance dir");
        Self {
            _temp: temp,
            layout,
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.layout.instance_dir.join(relative)
    }

    fn write(&self, relative: &str, contents: &[u8]) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).expect("create parent");
        fs::write(path, contents).expect("write legacy file");
    }

    fn snapshot(&self, dir: &str, disk: &[u8]) {
        self.write(&format!("{dir}/{ROOTFS}"), disk);
        self.write(&format!("{dir}/{VMSTATE}"), b"vmstate");
        self.write(&format!("{dir}/{PAGES}"), b"pages");
        self.write(&format!("{dir}/initramfs.stamp"), b"source=agent\n");
    }

    fn migrate(&self) -> Migration {
        let lock = hold_as_owner(&self.layout);
        migrate_legacy_layout(&self.layout, &lock).expect("migrate")
    }

    fn store(&self) -> Store {
        Store::new(&self.layout.instance_dir)
    }

    fn latest_disk(&self) -> Vec<u8> {
        let latest = self.store().latest().unwrap().expect("latest");
        fs::read(latest.rootfs()).unwrap()
    }
}

#[test]
fn a_snapshotted_instance_keeps_its_memory_snapshot() {
    let fixture = Fixture::new();
    fixture.write(ROOTFS, b"promoted disk");
    fixture.snapshot("memory-snapshots/latest", b"snapshot disk");
    fixture.write(
        "memory-snapshots/final-snapshot.outcome",
        b"version=1\npid=1\nstatus=success\n",
    );
    fixture.write(
        "memory-snapshots/latest/host-share-state/home/upper/file",
        b"share",
    );

    let Migration::Migrated {
        crashed_run,
        checkpoints,
        ..
    } = fixture.migrate()
    else {
        panic!("expected a migration");
    };

    assert_eq!(crashed_run, None);
    assert_eq!(checkpoints, 0);
    assert_eq!(fixture.latest_disk(), b"snapshot disk");
    let latest = fixture.store().latest().unwrap().unwrap();
    assert!(latest.manifest.has_memory());
    assert_eq!(latest.manifest.origin, Origin::Migrated);
    assert_eq!(
        fs::read(latest.dir.join("host-share-state/home/upper/file")).unwrap(),
        b"share"
    );
    for legacy in ["memory-snapshots", ROOTFS, HOST_SHARE_STATE] {
        assert!(!fixture.path(legacy).exists(), "{legacy} was not removed");
    }
}

#[test]
fn a_cold_instance_becomes_a_disk_only_generation() {
    let fixture = Fixture::new();
    fixture.write(ROOTFS, b"cold disk");
    fixture.write("host-share-state/home/upper/file", b"live share");

    fixture.migrate();

    assert_eq!(fixture.latest_disk(), b"cold disk");
    let latest = fixture.store().latest().unwrap().unwrap();
    assert!(!latest.manifest.has_memory());
    assert_eq!(
        fs::read(latest.dir.join("host-share-state/home/upper/file")).unwrap(),
        b"live share"
    );
}

#[test]
fn an_interrupted_restored_run_becomes_a_crashed_run_to_salvage() {
    let fixture = Fixture::new();
    fixture.write(ROOTFS, b"promoted disk");
    fixture.snapshot("memory-snapshots/latest", b"snapshot disk");
    fixture.snapshot("memory-snapshots/.restore-work", b"acknowledged writes");
    fixture.write(
        "memory-snapshots/.restore-work.active",
        b"generation_id=x\n",
    );
    fixture.write(
        "memory-snapshots/final-snapshot.outcome",
        b"version=1\npid=1\nstatus=pending\n",
    );

    let Migration::Migrated { crashed_run, .. } = fixture.migrate() else {
        panic!("expected a migration");
    };
    let run = crashed_run.expect("crashed run");

    let lock = hold_as_owner(&fixture.layout);
    let store = fixture.store();
    let error = store
        .recover(&lock)
        .expect_err("crashed run needs a choice");
    assert!(
        error
            .downcast_ref::<super::super::CrashedWithUnsavedState>()
            .is_some()
    );
    assert!(matches!(
        store.record().unwrap().unwrap().phase,
        Phase::Dirty { run: dirty, .. } if dirty == run
    ));
    store.salvage(&lock).expect("salvage");
    assert_eq!(fixture.latest_disk(), b"acknowledged writes");
    assert_eq!(store.recover(&lock).unwrap(), Recovery::Clean);
}

#[test]
fn a_publish_interrupted_between_renames_keeps_the_previous_snapshot() {
    let fixture = Fixture::new();
    fixture.snapshot(
        "memory-snapshots/.latest.previous",
        b"previous snapshot disk",
    );

    fixture.migrate();

    assert_eq!(fixture.latest_disk(), b"previous snapshot disk");
}

#[test]
fn complete_checkpoints_become_references() {
    let fixture = Fixture::new();
    fixture.write(ROOTFS, b"disk");
    fixture.snapshot("checkpoints/20260101T000000Z-1", b"checkpoint disk");
    fixture.write(
        "checkpoints/20260101T000000Z-1/checkpoint.meta",
        b"version=1\nid=20260101T000000Z-1\nname=before upgrade\ncreated_unix=42\n",
    );
    fixture.snapshot("checkpoints/20260101T000000Z-2", b"half written");

    let Migration::Migrated { checkpoints, .. } = fixture.migrate() else {
        panic!("expected a migration");
    };

    assert_eq!(checkpoints, 1);
    let refs = fixture.store().checkpoints().unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].id, "20260101T000000Z-1");
    assert_eq!(refs[0].name.as_deref(), Some("before upgrade"));
    assert_eq!(refs[0].created_unix, 42);
    let generation = fixture.store().generation(&refs[0].generation).unwrap();
    assert_eq!(fs::read(generation.rootfs()).unwrap(), b"checkpoint disk");
    assert!(!fixture.path("checkpoints/20260101T000000Z-1").exists());
    assert!(!fixture.path("checkpoints/20260101T000000Z-2").exists());
}

#[test]
fn migration_is_idempotent_and_finishes_interrupted_cleanup() {
    let fixture = Fixture::new();
    fixture.write(ROOTFS, b"disk");
    fixture.migrate();
    // As if the previous migration died after committing but before cleanup.
    fixture.write(ROOTFS, b"stale leftover");

    assert_eq!(fixture.migrate(), Migration::NotNeeded);
    assert!(!fixture.path(ROOTFS).exists());
    assert_eq!(fixture.latest_disk(), b"disk");
}

#[test]
fn an_empty_instance_needs_no_migration() {
    let fixture = Fixture::new();

    assert_eq!(fixture.migrate(), Migration::NotNeeded);
    assert!(!fixture.store().exists());
}

#[test]
fn a_live_older_lnx_blocks_migration() {
    let fixture = Fixture::new();
    fixture.write(ROOTFS, b"disk");
    let lock_dir = fixture.layout.run_dir.join(OLD_LOCK);
    fs::create_dir_all(&lock_dir).unwrap();
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn stand-in for an old owner");
    fs::write(lock_dir.join("owner.pid"), child.id().to_string()).unwrap();

    let lock = hold_as_owner(&fixture.layout);
    let error = migrate_legacy_layout(&fixture.layout, &lock).expect_err("old owner is live");

    assert!(error.to_string().contains("older lnx process"));
    assert!(fixture.path(ROOTFS).exists());
    child.kill().unwrap();
    child.wait().unwrap();
}
