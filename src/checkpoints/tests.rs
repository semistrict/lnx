use super::*;
use crate::runner::test_support::hold_as_owner;
use crate::store::{self, test_support::initialized};

struct Fixture {
    _temp: tempfile::TempDir,
    source: Layout,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = Layout::resolve_in_base("source", temp.path().to_path_buf(), None, None);
        Self {
            _temp: temp,
            source,
        }
    }

    fn dest(&self, name: &str) -> Layout {
        Layout::resolve_in_base(name, self.source.base.clone(), None, None)
    }
}

fn latest_disk(layout: &Layout) -> Vec<u8> {
    store::test_support::latest_disk(layout)
}

/// Gives the source a new latest generation whose disk holds `disk`.
fn advance_source(layout: &Layout, disk: &[u8]) {
    let lock = hold_as_owner(layout);
    let store = Store::new(&layout.instance_dir);
    let run = store.begin_run(&lock, None).expect("begin run");
    let staging = store.stage(&lock).expect("stage");
    fs::write(staging.dir().join(store::ROOTFS), disk).expect("write disk");
    let id = store
        .publish(
            &lock,
            staging,
            None,
            store::Origin::Snapshot { run: run.id },
        )
        .expect("publish");
    store.commit_latest(&lock, &id).expect("commit");
}

#[test]
fn a_stopped_instance_is_checkpointed_by_reference() {
    let fixture = Fixture::new();
    let image = initialized(&fixture.source, b"disk", true);

    let checkpoint = create(&fixture.source, Some("  before upgrade\n")).expect("checkpoint");

    assert_eq!(checkpoint.generation, image);
    assert_eq!(checkpoint.name.as_deref(), Some("before upgrade"));
    assert_eq!(
        resolve(&fixture.source, "before upgrade").expect("by name"),
        checkpoint
    );
    assert_eq!(
        resolve(&fixture.source, &checkpoint.id).expect("by id"),
        checkpoint
    );
}

#[test]
fn checkpoints_taken_back_to_back_keep_distinct_ids() {
    let fixture = Fixture::new();
    initialized(&fixture.source, b"disk", true);

    let first = create(&fixture.source, Some("one")).expect("first");
    let second = create(&fixture.source, Some("two")).expect("second");

    assert_ne!(first.id, second.id);
    assert_eq!(list(&fixture.source).expect("list").len(), 2);
}

#[test]
fn restoring_a_checkpoint_rolls_back_and_keeps_the_replaced_state() {
    let fixture = Fixture::new();
    initialized(&fixture.source, b"v1", true);
    let v1 = create(&fixture.source, Some("v1")).expect("checkpoint v1");
    advance_source(&fixture.source, b"v2");

    let before = restore(&fixture.source, &v1)
        .expect("restore")
        .expect("state changed");

    assert_eq!(latest_disk(&fixture.source), b"v1");
    assert_eq!(before.name.as_deref(), Some(BEFORE_RESTORE));
    assert_eq!(
        resolve(&fixture.source, BEFORE_RESTORE).expect("kept").generation,
        before.generation
    );

    // Undo the restore; the newer replaced state takes the name over.
    let undo = resolve(&fixture.source, BEFORE_RESTORE).expect("before-restore");
    restore(&fixture.source, &undo).expect("undo");
    assert_eq!(latest_disk(&fixture.source), b"v2");
    let names: Vec<_> = list(&fixture.source)
        .expect("list")
        .into_iter()
        .filter_map(|checkpoint| checkpoint.name)
        .collect();
    assert_eq!(names.iter().filter(|name| *name == BEFORE_RESTORE).count(), 1);
    assert!(names.contains(&"v1".to_string()));
}

#[test]
fn restoring_the_current_state_changes_nothing() {
    let fixture = Fixture::new();
    initialized(&fixture.source, b"v1", true);
    let v1 = create(&fixture.source, Some("v1")).expect("checkpoint v1");

    assert_eq!(restore(&fixture.source, &v1).expect("restore"), None);
    assert_eq!(list(&fixture.source).expect("list").len(), 1);
}

#[test]
fn resolve_rejects_ambiguous_checkpoint_names() {
    let fixture = Fixture::new();
    initialized(&fixture.source, b"disk", true);
    create(&fixture.source, Some("same")).expect("first");
    advance_source(&fixture.source, b"later disk");
    let lock = hold_as_owner(&fixture.source);
    let store = Store::new(&fixture.source.instance_dir);
    store
        .add_checkpoint(
            &lock,
            &CheckpointRef {
                id: "second".to_string(),
                name: Some("same".to_string()),
                generation: store.record().unwrap().unwrap().latest.unwrap(),
                created_unix: 2,
            },
        )
        .expect("second checkpoint");
    drop(lock);

    let error = resolve(&fixture.source, "same").expect_err("ambiguous");

    assert_eq!(error.to_string(), "checkpoint name is ambiguous: same");
}

#[test]
fn deleting_a_checkpoint_releases_its_generation() {
    let fixture = Fixture::new();
    let image = initialized(&fixture.source, b"disk", true);
    let checkpoint = create(&fixture.source, Some("old")).expect("checkpoint");
    advance_source(&fixture.source, b"later disk");
    assert!(
        Store::new(&fixture.source.instance_dir)
            .generation_dir(&image)
            .exists()
    );

    delete(&fixture.source, &checkpoint).expect("delete");

    assert!(list(&fixture.source).unwrap().is_empty());
    assert!(
        !Store::new(&fixture.source.instance_dir)
            .generation_dir(&image)
            .exists()
    );
}

#[test]
fn deleting_a_checkpoint_waits_for_a_stopped_instance() {
    let fixture = Fixture::new();
    initialized(&fixture.source, b"disk", true);
    let checkpoint = create(&fixture.source, None).expect("checkpoint");
    let owner = hold_as_owner(&fixture.source);

    let error = delete(&fixture.source, &checkpoint).expect_err("owner holds the instance");

    assert!(error.to_string().contains("is running"));
    drop(owner);
}

#[test]
fn forking_a_checkpoint_copies_its_state_with_the_destination_identity() {
    let fixture = Fixture::new();
    fs::create_dir_all(&fixture.source.instance_dir).unwrap();
    descriptor::save(
        &fixture.source,
        &descriptor::InstanceDescriptor {
            name: Some("source".to_string()),
            image: Some("release:test".to_string()),
            ..Default::default()
        },
    )
    .expect("write source descriptor");
    initialized(&fixture.source, b"checkpointed disk", true);
    let checkpoint = create(&fixture.source, Some("base")).expect("checkpoint");
    advance_source(&fixture.source, b"later disk");
    let dest = fixture.dest("copy");

    fork(&fixture.source, ForkSource::Checkpoint(&checkpoint), &dest).expect("fork");

    assert_eq!(latest_disk(&dest), b"checkpointed disk");
    assert_eq!(latest_disk(&fixture.source), b"later disk");
    let dest_descriptor = descriptor::load(&dest).expect("dest descriptor");
    assert_eq!(dest_descriptor.name.as_deref(), Some("copy"));
    assert_eq!(dest_descriptor.image.as_deref(), Some("release:test"));
    assert!(!dest.instance_dir.join(".lnx-fork-lease").exists());
}

#[test]
fn forking_a_stopped_instance_copies_its_latest_state() {
    let fixture = Fixture::new();
    initialized(&fixture.source, b"current disk", false);
    let dest = fixture.dest("copy");

    fork(&fixture.source, ForkSource::Current, &dest).expect("fork");

    assert_eq!(latest_disk(&dest), b"current disk");
    assert!(
        list(&fixture.source).unwrap().is_empty(),
        "no checkpoint is left behind"
    );
}

#[test]
fn forking_refuses_an_existing_destination() {
    let fixture = Fixture::new();
    initialized(&fixture.source, b"disk", false);
    let dest = fixture.dest("copy");
    initialized(&dest, b"existing", false);

    let error = fork(&fixture.source, ForkSource::Current, &dest).expect_err("exists");

    assert!(
        error
            .to_string()
            .contains("destination instance already exists")
    );
    assert_eq!(latest_disk(&dest), b"existing");
}

#[test]
fn forking_a_corrupt_checkpoint_creates_nothing() {
    let fixture = Fixture::new();
    let image = initialized(&fixture.source, b"disk", true);
    let checkpoint = create(&fixture.source, None).expect("checkpoint");
    fs::write(
        Store::new(&fixture.source.instance_dir)
            .generation_dir(&image)
            .join(store::PAGES),
        b"truncated",
    )
    .expect("corrupt pages");
    let dest = fixture.dest("copy");

    let error = fork(&fixture.source, ForkSource::Checkpoint(&checkpoint), &dest)
        .expect_err("corrupt checkpoint");

    assert!(format!("{error:#}").contains("instead of"), "{error:#}");
    assert!(!dest.instance_dir.exists());
}

#[test]
fn fork_scavenges_staging_owned_by_a_dead_process() {
    let temp = tempfile::tempdir().expect("tempdir");
    let instances = temp.path().join("instances");
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
    let temp = tempfile::tempdir().expect("tempdir");
    let instances = temp.path().join("instances");
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
