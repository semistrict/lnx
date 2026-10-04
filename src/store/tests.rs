use super::*;
use crate::paths::Layout;
use crate::runner::test_support::hold_as_owner;

struct Fixture {
    _temp: tempfile::TempDir,
    layout: Layout,
    store: Store,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let layout = Layout::resolve_in_base("vm", temp.path().to_path_buf(), None, None);
        fs::create_dir_all(&layout.instance_dir).expect("create instance dir");
        let store = Store::new(&layout.instance_dir);
        Self {
            _temp: temp,
            layout,
            store,
        }
    }

    fn lock(&self) -> InstanceLock {
        hold_as_owner(&self.layout)
    }

    /// An instance created from an image whose disk holds `disk`.
    fn initialized(disk: &[u8]) -> (Self, GenerationId) {
        let fixture = Self::new();
        let lock = fixture.lock();
        let staging = fixture.store.stage(&lock).expect("stage image");
        fs::write(staging.dir().join(ROOTFS), disk).expect("write image rootfs");
        let id = fixture
            .store
            .initialize(&lock, staging)
            .expect("initialize store");
        (fixture, id)
    }

    /// Publishes the final snapshot of `run` with the run's disk and a memory
    /// image holding `memory`, without committing it.
    fn publish_snapshot(&self, lock: &InstanceLock, run: &Run, memory: &[u8]) -> GenerationId {
        let staging = self.store.stage(lock).expect("stage snapshot");
        clone_or_copy_file(&run.rootfs(), &staging.dir().join(ROOTFS)).expect("clone disk");
        fs::write(staging.dir().join(VMSTATE), b"vmstate").expect("write vmstate");
        fs::write(staging.dir().join(PAGES), memory).expect("write pages");
        self.store
            .publish(
                lock,
                staging,
                run.base.as_ref().map(|base| base.id().clone()),
                Origin::Snapshot {
                    run: run.id.clone(),
                },
            )
            .expect("publish snapshot")
    }

    fn latest_disk(&self) -> Vec<u8> {
        let latest = self.store.latest().expect("read latest").expect("latest");
        fs::read(latest.rootfs()).expect("read latest rootfs")
    }

    fn phase(&self) -> Phase {
        self.store
            .record()
            .expect("read record")
            .expect("record")
            .phase
    }

    fn entries(&self, dir: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.layout.instance_dir.join(dir))
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

#[test]
fn a_run_works_on_private_clones_of_latest() {
    let (fixture, image) = Fixture::initialized(b"image disk");
    let lock = fixture.lock();

    let run = fixture.store.begin_run(&lock, None).expect("begin run");
    fs::write(run.rootfs(), b"changed by the guest").expect("guest writes");

    assert_eq!(run.base.as_ref().map(Generation::id), Some(&image));
    assert!(!run.restores_memory());
    assert_eq!(fixture.latest_disk(), b"image disk");
    assert!(matches!(fixture.phase(), Phase::Running { run: id, .. } if id == run.id));
}

#[test]
fn committing_a_snapshot_makes_it_latest_and_collects_the_rest() {
    let (fixture, image) = Fixture::initialized(b"image disk");
    let lock = fixture.lock();
    let run = fixture.store.begin_run(&lock, None).expect("begin run");
    fixture
        .store
        .mark_dirty(&lock, &run.id)
        .expect("mark dirty");
    fs::write(run.rootfs(), b"after commands").expect("guest writes");

    let snapshot = fixture.publish_snapshot(&lock, &run, b"memory");
    fixture
        .store
        .commit_latest(&lock, &snapshot)
        .expect("commit snapshot");

    assert_eq!(fixture.phase(), Phase::Stopped);
    assert_eq!(fixture.latest_disk(), b"after commands");
    let latest = fixture.store.latest().unwrap().unwrap();
    assert!(latest.manifest.has_memory());
    assert_eq!(latest.manifest.parent, Some(image));
    assert_eq!(fixture.entries(GENERATIONS_DIR), [snapshot.to_string()]);
    assert!(fixture.entries(RUNS_DIR).is_empty());
}

#[test]
fn a_run_that_crashed_before_serving_commands_is_discarded_silently() {
    let (fixture, image) = Fixture::initialized(b"image disk");
    let run = {
        let lock = fixture.lock();
        fixture.store.begin_run(&lock, None).expect("begin run")
        // The owner dies here: the lock is released, the record says running.
    };

    let lock = fixture.lock();
    let recovery = fixture.store.recover(&lock).expect("recover");

    assert_eq!(recovery, Recovery::DiscardedIdleRun(run.id));
    assert_eq!(fixture.phase(), Phase::Stopped);
    assert_eq!(
        fixture
            .store
            .latest()
            .unwrap()
            .map(|latest| latest.id().clone()),
        Some(image)
    );
    assert!(fixture.entries(RUNS_DIR).is_empty());
}

#[test]
fn a_run_that_crashed_after_serving_commands_needs_an_explicit_choice() {
    let (fixture, _image) = Fixture::initialized(b"image disk");
    let run = {
        let lock = fixture.lock();
        let run = fixture.store.begin_run(&lock, None).expect("begin run");
        fixture
            .store
            .mark_dirty(&lock, &run.id)
            .expect("mark dirty");
        fs::write(run.rootfs(), b"acknowledged write").expect("guest writes");
        run
    };

    let lock = fixture.lock();
    let error = fixture
        .store
        .recover(&lock)
        .expect_err("dirty crash is not recovered silently");
    let crashed = error
        .downcast_ref::<CrashedWithUnsavedState>()
        .expect("typed crash error");

    assert_eq!(crashed.run, run.id);
    assert_eq!(fixture.entries(RUNS_DIR), [run.id.to_string()]);
    assert_eq!(fixture.store.crashed_run().unwrap(), Some(run.id));
}

#[test]
fn salvaging_a_crashed_run_keeps_its_acknowledged_writes() {
    let (fixture, image) = Fixture::initialized(b"image disk");
    {
        let lock = fixture.lock();
        let run = fixture.store.begin_run(&lock, None).expect("begin run");
        fixture
            .store
            .mark_dirty(&lock, &run.id)
            .expect("mark dirty");
        fs::write(run.rootfs(), b"acknowledged write").expect("guest writes");
    }

    let lock = fixture.lock();
    let salvaged = fixture.store.salvage(&lock).expect("salvage");

    assert_eq!(fixture.latest_disk(), b"acknowledged write");
    let latest = fixture.store.latest().unwrap().unwrap();
    assert_eq!(latest.id(), &salvaged);
    assert!(!latest.manifest.has_memory());
    assert_eq!(latest.manifest.parent, Some(image));
    assert_eq!(fixture.store.recover(&lock).unwrap(), Recovery::Clean);
}

#[test]
fn discarding_a_crashed_run_returns_to_latest() {
    let (fixture, image) = Fixture::initialized(b"image disk");
    {
        let lock = fixture.lock();
        let run = fixture.store.begin_run(&lock, None).expect("begin run");
        fixture
            .store
            .mark_dirty(&lock, &run.id)
            .expect("mark dirty");
    }

    let lock = fixture.lock();
    fixture.store.discard_run(&lock).expect("discard");

    assert_eq!(fixture.phase(), Phase::Stopped);
    assert_eq!(fixture.latest_disk(), b"image disk");
    assert_eq!(
        fixture
            .store
            .latest()
            .unwrap()
            .map(|latest| latest.id().clone()),
        Some(image)
    );
    assert!(fixture.entries(RUNS_DIR).is_empty());
}

#[test]
fn a_snapshot_published_before_a_crash_is_rolled_forward() {
    let (fixture, _image) = Fixture::initialized(b"image disk");
    let snapshot = {
        let lock = fixture.lock();
        let run = fixture.store.begin_run(&lock, None).expect("begin run");
        fixture
            .store
            .mark_dirty(&lock, &run.id)
            .expect("mark dirty");
        fs::write(run.rootfs(), b"final disk").expect("guest writes");
        fixture.publish_snapshot(&lock, &run, b"memory")
        // The owner dies between publishing and committing.
    };

    let lock = fixture.lock();
    let recovery = fixture.store.recover(&lock).expect("recover");

    assert_eq!(recovery, Recovery::RolledForward(snapshot));
    assert_eq!(fixture.phase(), Phase::Stopped);
    assert_eq!(fixture.latest_disk(), b"final disk");
}

/// Publishes a snapshot the guest asked for mid-run and makes it latest.
fn snapshot_exit(fixture: &Fixture, lock: &InstanceLock, run: &Run) -> GenerationId {
    let staging = fixture.store.stage(lock).expect("stage snapshot-exit");
    clone_or_copy_file(&run.rootfs(), &staging.dir().join(ROOTFS)).expect("clone disk");
    let id = fixture
        .store
        .publish(
            lock,
            staging,
            None,
            Origin::SnapshotExit {
                run: run.id.clone(),
            },
        )
        .expect("publish snapshot-exit");
    fixture
        .store
        .advance_latest(lock, &run.id, &id)
        .expect("advance");
    id
}

#[test]
fn writes_after_a_mid_run_snapshot_are_not_dropped_by_recovery() {
    let (fixture, _image) = Fixture::initialized(b"image disk");
    let (run, exit) = {
        let lock = fixture.lock();
        let run = fixture.store.begin_run(&lock, None).expect("begin run");
        fixture.store.mark_dirty(&lock, &run.id).expect("mark dirty");
        fs::write(run.rootfs(), b"before snapshot-exit").expect("guest writes");
        let exit = snapshot_exit(&fixture, &lock, &run);
        fs::write(run.rootfs(), b"acknowledged after").expect("guest writes on");
        (run, exit)
        // The owner dies.
    };

    let lock = fixture.lock();
    let error = fixture
        .store
        .recover(&lock)
        .expect_err("not rolled forward to the mid-run snapshot");
    assert_eq!(
        error.downcast_ref::<CrashedWithUnsavedState>().map(|crash| &crash.run),
        Some(&run.id)
    );
    fixture.store.salvage(&lock).expect("keep the disk");
    assert_eq!(fixture.latest_disk(), b"acknowledged after");
    assert_ne!(fixture.store.latest().unwrap().unwrap().id(), &exit);
}

#[test]
fn an_idle_run_that_crashes_after_a_mid_run_snapshot_keeps_it() {
    let (fixture, _image) = Fixture::initialized(b"image disk");
    let exit = {
        let lock = fixture.lock();
        let run = fixture.store.begin_run(&lock, None).expect("begin run");
        fs::write(run.rootfs(), b"snapshot-exit disk").expect("guest writes");
        snapshot_exit(&fixture, &lock, &run)
    };

    let lock = fixture.lock();
    let recovery = fixture.store.recover(&lock).expect("recover");

    assert!(matches!(recovery, Recovery::DiscardedIdleRun(_)));
    assert_eq!(fixture.store.latest().unwrap().unwrap().id(), &exit);
    assert_eq!(fixture.latest_disk(), b"snapshot-exit disk");
}

#[test]
fn a_half_written_generation_is_never_latest_and_is_collected() {
    let (fixture, image) = Fixture::initialized(b"image disk");
    let run = {
        let lock = fixture.lock();
        let run = fixture.store.begin_run(&lock, None).expect("begin run");
        let staging = fixture.store.stage(&lock).expect("stage");
        fs::write(staging.dir().join(ROOTFS), b"partial").expect("partial write");
        run
        // The owner dies while staging.
    };

    let lock = fixture.lock();
    let recovery = fixture.store.recover(&lock).expect("recover");

    assert_eq!(recovery, Recovery::DiscardedIdleRun(run.id));
    assert_eq!(fixture.entries(GENERATIONS_DIR), [image.to_string()]);
    assert_eq!(fixture.latest_disk(), b"image disk");
}

#[test]
fn checkpointed_generations_survive_collection_until_unreferenced() {
    let (fixture, image) = Fixture::initialized(b"image disk");
    let lock = fixture.lock();
    fixture
        .store
        .add_checkpoint(
            &lock,
            &CheckpointRef {
                id: "base".to_string(),
                name: Some("before experiments".to_string()),
                generation: image.clone(),
                created_unix: 1,
            },
        )
        .expect("checkpoint image");
    let run = fixture.store.begin_run(&lock, None).expect("begin run");
    let snapshot = fixture.publish_snapshot(&lock, &run, b"memory");
    fixture
        .store
        .commit_latest(&lock, &snapshot)
        .expect("commit");

    assert_eq!(
        fixture.entries(GENERATIONS_DIR),
        [image.to_string(), snapshot.to_string()]
    );
    assert_eq!(
        fixture
            .store
            .checkpoints()
            .unwrap()
            .into_iter()
            .map(|checkpoint| checkpoint.generation)
            .collect::<Vec<_>>(),
        std::slice::from_ref(&image)
    );

    fixture
        .store
        .remove_checkpoint(&lock, "base")
        .expect("remove checkpoint");

    assert_eq!(fixture.entries(GENERATIONS_DIR), [snapshot.to_string()]);
    assert!(fixture.store.checkpoints().unwrap().is_empty());
}

#[test]
fn a_pinned_generation_is_not_collected() {
    let (fixture, image) = Fixture::initialized(b"image disk");
    let lock = fixture.lock();
    let pin = fixture.store.pin(&image).expect("pin image");
    let run = fixture.store.begin_run(&lock, None).expect("begin run");
    let snapshot = fixture.publish_snapshot(&lock, &run, b"memory");

    fixture
        .store
        .commit_latest(&lock, &snapshot)
        .expect("commit");
    assert!(fixture.store.generation_dir(&image).exists());
    assert_eq!(fs::read(pin.generation.rootfs()).unwrap(), b"image disk");

    drop(pin);
    fixture.store.collect_garbage(&lock).expect("collect");
    assert!(!fixture.store.generation_dir(&image).exists());
}

#[test]
fn pinning_a_collected_generation_fails() {
    let (fixture, image) = Fixture::initialized(b"image disk");
    let lock = fixture.lock();
    let run = fixture.store.begin_run(&lock, None).expect("begin run");
    let snapshot = fixture.publish_snapshot(&lock, &run, b"memory");
    fixture
        .store
        .commit_latest(&lock, &snapshot)
        .expect("commit");

    let error = fixture.store.pin(&image).expect_err("collected");

    assert!(format!("{error:#}").contains("no longer exists"));
}

#[test]
fn a_generation_with_a_truncated_file_is_rejected() {
    let (fixture, image) = Fixture::initialized(b"image disk");
    fs::write(fixture.store.generation_dir(&image).join(ROOTFS), b"torn").expect("truncate");

    let error = fixture.store.generation(&image).expect_err("size mismatch");

    assert!(format!("{error:#}").contains("has size 4 instead of 10"));
}

#[test]
fn dropping_memory_keeps_the_disk_and_boots_next_time() {
    let (fixture, _image) = Fixture::initialized(b"image disk");
    let lock = fixture.lock();
    let run = fixture.store.begin_run(&lock, None).expect("begin run");
    fs::write(run.rootfs(), b"final disk").expect("guest writes");
    let snapshot = fixture.publish_snapshot(&lock, &run, b"memory");
    fixture
        .store
        .commit_latest(&lock, &snapshot)
        .expect("commit");

    let dropped = fixture
        .store
        .drop_memory(&lock)
        .expect("drop memory")
        .expect("there was memory to drop");

    let latest = fixture.store.latest().unwrap().unwrap();
    assert_eq!(latest.id(), &dropped);
    assert!(!latest.manifest.has_memory());
    assert_eq!(fixture.latest_disk(), b"final disk");
    assert_eq!(fixture.store.drop_memory(&lock).unwrap(), None);
    let next = fixture
        .store
        .begin_run(&lock, None)
        .expect("begin next run");
    assert!(!next.restores_memory());
}

#[test]
fn a_run_restoring_memory_gets_private_memory_files() {
    let (fixture, _image) = Fixture::initialized(b"image disk");
    let lock = fixture.lock();
    let run = fixture.store.begin_run(&lock, None).expect("begin run");
    let snapshot = fixture.publish_snapshot(&lock, &run, b"memory");
    fixture
        .store
        .commit_latest(&lock, &snapshot)
        .expect("commit");

    let next = fixture
        .store
        .begin_run(&lock, None)
        .expect("begin next run");

    assert!(next.restores_memory());
    assert_eq!(fs::read(next.dir.join(PAGES)).unwrap(), b"memory");
    fs::write(next.dir.join(PAGES), b"scribbled").expect("vhost-user writes RAM");
    assert_eq!(
        fs::read(fixture.store.generation_dir(&snapshot).join(PAGES)).unwrap(),
        b"memory"
    );
}

#[test]
fn committed_files_replace_atomically() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join(STATE_FILE);
    commit_file(&path, b"first").expect("commit first");
    commit_file(&path, b"second").expect("commit second");

    assert_eq!(fs::read_to_string(&path).unwrap(), "second");
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn ids_are_single_path_components() {
    assert!(GenerationId::try_from("../escape".to_string()).is_err());
    assert!(GenerationId::try_from(".staging-x".to_string()).is_err());
    assert!(RunId::try_from(String::new()).is_err());
    let id = GenerationId::new();
    assert_eq!(GenerationId::try_from(id.to_string()).unwrap(), id);
}
