use super::*;
use std::ffi::CString;
use std::os::unix::{ffi::OsStrExt, process::CommandExt};
use std::{
    io::{Seek, SeekFrom, Write},
    time::{SystemTime, UNIX_EPOCH},
};

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("lnx-{name}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn temp_layout(temp: &TempDir, instance: &str) -> Layout {
    let instance_dir = temp.path().join("instances").join(instance);
    Layout {
        base: temp.path().to_path_buf(),
        instance: instance.to_string(),
        kernel: temp.path().join("vmlinuz"),
        rootfs: instance_dir.join("rootfs.ext4"),
        instance_dir: instance_dir.clone(),
        snapshot_dir: instance_dir.join("memory-snapshots"),
        checkpoint_dir: instance_dir.join("checkpoints"),
        vm_initialized: instance_dir.join("vm-initialized"),
        run_dir: instance_dir.clone(),
        console_log: instance_dir.join("console.log"),
    }
}

fn write_fake_ext4(path: &Path, state: u16, marker: &[u8]) {
    let mut file = fs::File::create(path).expect("create fake ext4");
    file.set_len(4096).expect("size fake ext4");
    let mut superblock = [0u8; 1024];
    superblock[24..28].copy_from_slice(&4u32.to_le_bytes());
    superblock[56..58].copy_from_slice(&0xEF53u16.to_le_bytes());
    superblock[58..60].copy_from_slice(&state.to_le_bytes());
    file.seek(SeekFrom::Start(1024)).expect("seek superblock");
    file.write_all(&superblock).expect("write superblock");
    file.seek(SeekFrom::Start(3072)).expect("seek marker");
    file.write_all(marker).expect("write marker");
}

fn write_vmstate_header_with_version(
    snapshot: &Path,
    version: u32,
    memory_bytes: u64,
    vcpu_count: u32,
) {
    fs::create_dir_all(snapshot).expect("create snapshot dir");
    let mut header = [0u8; 40];
    header[0..8].copy_from_slice(b"LKRNSS01");
    header[8..12].copy_from_slice(&version.to_le_bytes());
    header[16..24].copy_from_slice(&memory_bytes.to_le_bytes());
    header[32..36].copy_from_slice(&vcpu_count.to_le_bytes());
    fs::write(snapshot.join("vmstate.bin"), header).expect("write vmstate");
}

fn write_vmstate_header(snapshot: &Path, memory_bytes: u64, vcpu_count: u32) {
    write_vmstate_header_with_version(snapshot, SNAPSHOT_VMSTATE_VERSION, memory_bytes, vcpu_count);
}

fn write_snapshot_state_files(snapshot: &Path) {
    fs::create_dir_all(snapshot).expect("create snapshot");
    fs::write(snapshot.join("pages.img"), b"pages").expect("write pages");
    write_vmstate_header(snapshot, 4 * 1024 * 1024 * 1024, 2);
}

fn set_mtime(path: &Path, unix_secs: i64) {
    let c_path = CString::new(path.as_os_str().as_bytes()).expect("cstring path");
    let times = [
        libc::timespec {
            tv_sec: unix_secs,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: unix_secs,
            tv_nsec: 0,
        },
    ];
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
    assert_eq!(rc, 0, "utimensat {}", path.display());
}

#[test]
fn prepare_restore_clones_snapshot_into_bounded_work_dir() {
    let temp = TempDir::new("restore-snapshot-clone");
    let layout = temp_layout(&temp, "default");
    let snapshot = layout.snapshot_dir.join("latest");
    write_snapshot_state_files(&snapshot);
    write_fake_ext4(&snapshot.join("rootfs.ext4"), 4, b"snapshot-rootfs");
    set_mtime(&snapshot.join("rootfs.ext4"), 100);
    set_mtime(&snapshot.join("pages.img"), 101);
    set_mtime(&snapshot.join("vmstate.bin"), 101);
    let run_log = RunLog::open(&layout).expect("run log");

    let restore =
        prepare_restore_for_start(&layout, Some(&snapshot), Some("snapshot-test"), &run_log)
            .unwrap()
            .expect("restore work");

    assert_eq!(
        restore.snapshot,
        layout.snapshot_dir.join(RESTORE_WORK_SNAPSHOT)
    );
    assert_eq!(restore.rootfs, restore.snapshot.join("rootfs.ext4"));
    assert_eq!(restore.generation_id, "snapshot-test");
    assert!(restore.snapshot.join("vmstate.bin").exists());
    assert!(restore.snapshot.join("pages.img").exists());
    assert!(restore.rootfs.exists());
    assert_ne!(restore.rootfs, snapshot.join("rootfs.ext4"));
}

#[test]
fn snapshot_lifecycle_manifest_records_generation_and_file_state() {
    let temp = TempDir::new("snapshot-lifecycle-meta");
    let layout = temp_layout(&temp, "default");
    let snapshot = layout.snapshot_dir.join("latest");
    write_snapshot_state_files(&snapshot);
    write_fake_ext4(&snapshot.join("rootfs.ext4"), 4, b"snapshot-rootfs");

    write_snapshot_lifecycle_manifest(&snapshot, "snapshot-test", "run-test", &layout.rootfs)
        .expect("write snapshot manifest");
    let manifest =
        fs::read_to_string(snapshot.join(SNAPSHOT_LIFECYCLE_META)).expect("read manifest");

    assert!(manifest.contains("version=1\n"));
    assert!(manifest.contains("generation_id=snapshot-test\n"));
    assert!(manifest.contains("source_run_id=run-test\n"));
    assert!(manifest.contains("vmstate.bin.size="));
    assert!(manifest.contains("pages.img.size="));
    assert!(manifest.contains("rootfs.ext4.size="));
    assert_eq!(
        read_snapshot_generation_id(&snapshot).as_deref(),
        Some("snapshot-test")
    );
}

#[test]
fn restore_rootfs_clone_log_includes_snapshot_generation() {
    let temp = TempDir::new("restore-rootfs-log");
    let layout = temp_layout(&temp, "default");
    let snapshot = layout.snapshot_dir.join("latest");
    write_snapshot_state_files(&snapshot);
    write_fake_ext4(&snapshot.join("rootfs.ext4"), 4, b"snapshot-rootfs");
    set_mtime(&snapshot.join("rootfs.ext4"), 100);
    set_mtime(&snapshot.join("pages.img"), 101);
    set_mtime(&snapshot.join("vmstate.bin"), 101);
    write_snapshot_lifecycle_manifest(&snapshot, "snapshot-test", "run-test", &layout.rootfs)
        .expect("write snapshot manifest");
    let run_log = RunLog::open(&layout).expect("run log");

    prepare_restore_for_start(&layout, Some(&snapshot), None, &run_log)
        .unwrap()
        .expect("restore work");

    let log = fs::read_to_string(layout.run_dir.join("lnx.log")).expect("read run log");
    assert!(log.contains("snapshot.restore.clone generation_id=snapshot-test"));
    assert!(log.contains(&format!("source={}", snapshot.display())));
    assert!(log.contains(&format!(
        "work={}",
        layout.snapshot_dir.join(RESTORE_WORK_SNAPSHOT).display()
    )));
}

#[test]
fn snapshot_publish_replaces_latest_without_accumulating_temp_dirs() {
    let temp = TempDir::new("snapshot-publish");
    let layout = temp_layout(&temp, "default");
    let latest = layout.snapshot_dir.join("latest");
    let next = snapshot_publish_temp(&latest).expect("next path");
    fs::create_dir_all(&latest).expect("create latest");
    fs::write(latest.join("rootfs.ext4"), b"old").expect("old rootfs");
    fs::create_dir_all(&next).expect("create next");
    fs::write(next.join("rootfs.ext4"), b"new").expect("new rootfs");
    let run_log = RunLog::open(&layout).expect("run log");

    publish_snapshot_dir(&latest, &next, &run_log, "run-test", "snapshot-test")
        .expect("publish snapshot");

    assert_eq!(
        fs::read(latest.join("rootfs.ext4")).expect("read latest"),
        b"new"
    );
    assert!(!next.exists());
    assert!(
        !snapshot_publish_previous(&latest)
            .expect("previous path")
            .exists()
    );
}

#[test]
fn snapshot_runtime_cleanup_removes_only_fixed_work_and_publish_dirs() {
    let temp = TempDir::new("snapshot-cleanup");
    let layout = temp_layout(&temp, "default");
    let latest = layout.snapshot_dir.join("latest");
    let work = layout.snapshot_dir.join(RESTORE_WORK_SNAPSHOT);
    let next = snapshot_publish_temp(&latest).expect("next path");
    let previous = snapshot_publish_previous(&latest).expect("previous path");
    for path in [&latest, &work, &next, &previous] {
        fs::create_dir_all(path).expect("create snapshot dir");
        fs::write(path.join("marker"), path.display().to_string()).expect("write marker");
    }
    let run_log = RunLog::open(&layout).expect("run log");

    cleanup_snapshot_runtime_state(&layout, &run_log).expect("cleanup");

    assert!(latest.exists());
    assert!(!work.exists());
    assert!(!next.exists());
    assert!(!previous.exists());
}

#[test]
fn restore_snapshot_rootfs_newer_than_memory_is_rejected() {
    let temp = TempDir::new("restore-rootfs-stale");
    let layout = temp_layout(&temp, "default");
    let snapshot = layout.snapshot_dir.join("latest");
    write_snapshot_state_files(&snapshot);
    write_fake_ext4(&snapshot.join("rootfs.ext4"), 4, b"snapshot-rootfs");
    set_mtime(&snapshot.join("pages.img"), 100);
    set_mtime(&snapshot.join("vmstate.bin"), 100);
    set_mtime(&snapshot.join("rootfs.ext4"), 103);
    let run_log = RunLog::open(&layout).expect("run log");

    let err = prepare_restore_for_start(&layout, Some(&snapshot), Some("snapshot-test"), &run_log)
        .unwrap_err();
    let message = format!("{err:#}");

    assert!(err.downcast_ref::<RestoreRefused>().is_some());
    assert!(message.contains("snapshot rootfs was modified after memory state was captured"));
    assert!(!layout.snapshot_dir.join(RESTORE_WORK_SNAPSHOT).exists());
}

#[test]
fn preflight_host_share_cwd_reports_hidden_working_directory() {
    let temp = TempDir::new("hidden-cwd");
    let layout = temp_layout(&temp, "default");
    let home = temp.path().join("home");
    let cwd = home.join("src/project");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(
        layout
            .instance_dir
            .join("host-share-state/home/whiteouts/src"),
    )
    .expect("create whiteout dir");
    fs::write(
        layout
            .instance_dir
            .join("host-share-state/home/whiteouts/src/.lnx-whiteout"),
        b"whiteout\n",
    )
    .expect("write whiteout marker");

    let err = preflight_host_share_cwd_with_home(&layout, &cwd, false, &home).unwrap_err();
    let message = err.to_string();

    assert!(message.contains("working directory is hidden"));
    assert!(message.contains("lnx fs unshare --remove"));
    assert!(message.contains(&home.join("src").display().to_string()));
}

#[test]
fn preflight_host_share_cwd_allows_descendant_whiteout_namespace() {
    let temp = TempDir::new("cwd-namespace");
    let layout = temp_layout(&temp, "default");
    let home = temp.path().join("home");
    let cwd = home.join("src/project");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(
        layout
            .instance_dir
            .join("host-share-state/home/whiteouts/src/project"),
    )
    .expect("create namespace dir");

    preflight_host_share_cwd_with_home(&layout, &cwd, false, &home).unwrap();
}

#[test]
fn snapshot_vm_config_returns_none_when_vmstate_is_absent() {
    let temp = TempDir::new("snapshot-missing");

    assert!(
        snapshot_vm_config(temp.path())
            .expect("read config")
            .is_none()
    );
}

#[test]
fn snapshot_vm_config_parses_header_and_matches_config() {
    let temp = TempDir::new("snapshot-header");
    write_vmstate_header(temp.path(), 4 * 1024 * 1024 * 1024, 2);

    let config = snapshot_vm_config(temp.path())
        .expect("read config")
        .expect("config present");

    assert_eq!(config.version, SNAPSHOT_VMSTATE_VERSION);
    assert_eq!(config.vcpu_count, 2);
    assert_eq!(config.memory_mib(), 4096);
    assert!(config.matches(2, 4096));
    assert!(!config.matches(1, 4096));
    assert!(!config.matches(2, 8192));
}

#[test]
fn agent_reader_failure_notifies_waiting_clients() {
    let clients = Mutex::new(HashMap::new());
    let active = AtomicUsize::new(1);
    let (tx, rx) = mpsc::channel();
    let channel_id = 0xabcddcba_u64;
    clients.lock().unwrap().insert(
        channel_id,
        BrokerChannel {
            tx,
            active_owned_by_reader: true,
        },
    );

    let dropped = drain_broker_channels(
        &clients,
        &active,
        Some("guest agent disconnected before command completed".to_string()),
    );

    assert_eq!(dropped, 1);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert!(clients.lock().unwrap().is_empty());
    match rx.recv().expect("client error") {
        Message::Error {
            channel_id: id,
            message,
        } => {
            assert_eq!(id, channel_id);
            assert!(message.contains("guest agent disconnected"));
        }
        other => panic!("expected client error, got {other:?}"),
    }
}

#[test]
fn fresh_owner_slot_removes_stale_bootstrap_lock() {
    let temp = TempDir::new("fresh-owner-stale-lock");
    let layout = temp_layout(&temp, "vm");
    fs::create_dir_all(&layout.run_dir).expect("create run dir");
    let lock = layout.run_dir.join("bootstrap.lock.d");
    fs::create_dir(&lock).expect("create lock");
    fs::write(lock.join("owner.pid"), "999999").expect("write stale pid");
    let run_log = RunLog::open(&layout).expect("open run log");

    wait_for_fresh_owner_slot(&layout, &run_log).expect("stale lock should be removed");

    assert!(!lock.exists());
}

#[test]
fn fresh_owner_slot_replace_stops_recorded_owner() {
    let temp = TempDir::new("fresh-owner-replace");
    let layout = temp_layout(&temp, "vm");
    fs::create_dir_all(&layout.run_dir).expect("create run dir");
    let lock = layout.run_dir.join("bootstrap.lock.d");
    fs::create_dir(&lock).expect("create lock");
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg("trap 'exit 0' TERM; while :; do sleep 1; done")
        .process_group(0)
        .spawn()
        .expect("spawn child owner");
    fs::write(lock.join("owner.pid"), child.id().to_string()).expect("write owner pid");
    fs::write(layout.run_dir.join("broker.sock"), "").expect("write broker socket placeholder");
    let run_log = RunLog::open(&layout).expect("open run log");

    prepare_fresh_owner_slot(&layout, true, &run_log).expect("replace owner");

    let _ = child.wait();
    assert!(!process_alive(child.id() as libc::pid_t));
    assert!(!lock.exists());
    assert!(!layout.run_dir.join("broker.sock").exists());
}

#[test]
fn bootstrap_lock_stale_reclaim_takes_ownership() {
    let temp = TempDir::new("bootstrap-lock-stale-reclaim");
    let lock_path = temp.path().join("bootstrap.lock.d");
    fs::create_dir(&lock_path).expect("create lock dir");
    // pid 0 is never alive per process_alive, so the lock is stale.
    fs::write(lock_path.join("owner.pid"), "0").expect("write stale owner pid");

    let lock = BootstrapLock::try_acquire(&lock_path).expect("try_acquire should not error");

    assert!(lock.is_some());
    let owner_pid = fs::read_to_string(lock_path.join("owner.pid")).expect("read owner pid");
    assert_eq!(owner_pid, std::process::id().to_string());
}

#[test]
fn bootstrap_lock_live_lock_is_not_reclaimed() {
    let temp = TempDir::new("bootstrap-lock-live");
    let lock_path = temp.path().join("bootstrap.lock.d");
    fs::create_dir(&lock_path).expect("create lock dir");
    let live_pid = std::process::id().to_string();
    fs::write(lock_path.join("owner.pid"), &live_pid).expect("write live owner pid");

    let lock = BootstrapLock::try_acquire(&lock_path).expect("try_acquire should not error");

    assert!(lock.is_none());
    let owner_pid = fs::read_to_string(lock_path.join("owner.pid")).expect("read owner pid");
    assert_eq!(owner_pid, live_pid);
}

#[test]
fn bootstrap_lock_concurrent_stale_reclaim_has_single_winner() {
    let temp = TempDir::new("bootstrap-lock-concurrent");
    let lock_path = temp.path().join("bootstrap.lock.d");
    fs::create_dir(&lock_path).expect("create lock dir");
    fs::write(lock_path.join("owner.pid"), "0").expect("write stale owner pid");

    const THREADS: usize = 8;
    let barrier = Arc::new(std::sync::Barrier::new(THREADS));
    // Pre-fix, this test failed only probabilistically (the race window is
    // narrow); it exists as the regression guard for the invariant that
    // exactly one thread may reclaim a stale lock.
    let winners: Arc<Mutex<Vec<BootstrapLock>>> = Arc::new(Mutex::new(Vec::new()));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let lock_path = lock_path.clone();
            let barrier = Arc::clone(&barrier);
            let winners = Arc::clone(&winners);
            thread::spawn(move || {
                barrier.wait();
                if let Ok(Some(lock)) = BootstrapLock::try_acquire(&lock_path) {
                    // Keep the winning lock alive until every thread has
                    // finished: dropping it early would remove the lock dir
                    // and let a later thread win too.
                    winners.lock().unwrap().push(lock);
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("thread should not panic");
    }

    let winners = Arc::try_unwrap(winners)
        .unwrap_or_else(|_| panic!("winners still shared"))
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(winners.len(), 1);
}

#[test]
fn owner_start_lock_concurrent_stale_reclaim_has_single_winner() {
    let temp = TempDir::new("owner-start-lock-concurrent");
    let lock_path = temp.path().join("owner-start.lock.d");
    fs::create_dir(&lock_path).expect("create lock dir");
    fs::write(lock_path.join("starter.pid"), "0").expect("write stale starter pid");

    const THREADS: usize = 8;
    let barrier = Arc::new(std::sync::Barrier::new(THREADS));
    // Pre-fix, this test failed only probabilistically (the race window is
    // narrow); it exists as the regression guard for the invariant that
    // exactly one thread may reclaim a stale lock.
    let winners: Arc<Mutex<Vec<OwnerStartLock>>> = Arc::new(Mutex::new(Vec::new()));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let lock_path = lock_path.clone();
            let barrier = Arc::clone(&barrier);
            let winners = Arc::clone(&winners);
            thread::spawn(move || {
                barrier.wait();
                if let Ok(Some(lock)) = OwnerStartLock::try_acquire(&lock_path) {
                    // Keep the winning lock alive until every thread has
                    // finished: dropping it early would remove the lock dir
                    // and let a later thread win too.
                    winners.lock().unwrap().push(lock);
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("thread should not panic");
    }

    let winners = Arc::try_unwrap(winners)
        .unwrap_or_else(|_| panic!("winners still shared"))
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(winners.len(), 1);
}

#[test]
fn owner_attempt_log_reset_truncates_stale_diagnostics() {
    let temp = TempDir::new("owner-log-reset");
    let layout = temp_layout(&temp, "vm");
    fs::create_dir_all(&layout.run_dir).expect("create run dir");
    let owner_log = layout.run_dir.join("owner.log");
    fs::write(&owner_log, b"old owner failure").expect("write owner log");
    fs::write(&layout.console_log, b"old console failure").expect("write console log");
    let run_log = RunLog::open(&layout).expect("open run log");

    reset_owner_attempt_logs(&layout, &run_log);

    assert_eq!(fs::read(&owner_log).expect("read owner log"), b"");
    assert_eq!(
        fs::read(&layout.console_log).expect("read console log"),
        b""
    );
}

#[test]
fn snapshot_rootfs_promotion_replaces_cold_boot_rootfs() {
    let temp = TempDir::new("promote-rootfs");
    let layout = temp_layout(&temp, "vm");
    fs::create_dir_all(&layout.run_dir).expect("create run dir");
    let snapshot = layout.snapshot_dir.join("latest");
    fs::create_dir_all(&snapshot).expect("create snapshot");
    fs::write(&layout.rootfs, b"old cold rootfs").expect("write old rootfs");
    write_fake_ext4(
        snapshot.join("rootfs.ext4").as_path(),
        0x0001,
        b"new snapshot rootfs",
    );
    let timings = TimingLog::open(&layout, &["true".to_string()], None).expect("open timings");
    let run_log = RunLog::open(&layout).expect("open run log");

    promote_snapshot_rootfs(
        &snapshot,
        &layout.rootfs,
        &timings,
        &run_log,
        Some("snapshot-test"),
        Some("run-test"),
    )
    .expect("promote snapshot rootfs");

    let promoted = fs::read(&layout.rootfs).expect("read promoted rootfs");
    assert!(
        promoted
            .windows(b"new snapshot rootfs".len())
            .any(|window| window == b"new snapshot rootfs")
    );
    assert!(
        !layout
            .rootfs
            .parent()
            .unwrap()
            .join(".rootfs.ext4.promote")
            .exists()
    );
}

#[test]
fn snapshot_rootfs_promotion_rejects_ext4_errors() {
    let temp = TempDir::new("promote-rootfs-errors");
    let layout = temp_layout(&temp, "vm");
    fs::create_dir_all(&layout.run_dir).expect("create run dir");
    let snapshot = layout.snapshot_dir.join("latest");
    fs::create_dir_all(&snapshot).expect("create snapshot");
    fs::write(&layout.rootfs, b"old cold rootfs").expect("write old rootfs");
    write_fake_ext4(
        snapshot.join("rootfs.ext4").as_path(),
        0x0001 | 0x0002,
        b"bad snapshot rootfs",
    );
    let timings = TimingLog::open(&layout, &["true".to_string()], None).expect("open timings");
    let run_log = RunLog::open(&layout).expect("open run log");

    let error = promote_snapshot_rootfs(
        &snapshot,
        &layout.rootfs,
        &timings,
        &run_log,
        Some("snapshot-test"),
        Some("run-test"),
    )
    .expect_err("bad snapshot rootfs should not be promoted");

    assert!(
        error.to_string().contains("marked with ext4 errors"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(&layout.rootfs).expect("read canonical rootfs"),
        b"old cold rootfs"
    );
}

fn test_launch_metadata(
    host_home: &str,
    outside_home_cwd: Option<&str>,
    no_host_shares: bool,
    host_share_cache: LaunchHostShareCache,
    vhost_user_fs: Vec<LaunchVhostUserFsMount>,
) -> LaunchMetadata {
    LaunchMetadata {
        version: LAUNCH_METADATA_VERSION,
        owner_args: vec!["lnx".to_string(), "_vm-owner".to_string()],
        compatibility: LaunchCompatibility { host_share_cache },
        shares: LaunchShares {
            no_host_shares,
            host_home: (!no_host_shares).then(|| PathBuf::from(host_home)),
            outside_home_cwd: if no_host_shares {
                None
            } else {
                outside_home_cwd.map(PathBuf::from)
            },
        },
        vhost_user_fs,
    }
}

fn test_host_share_cache(dax: bool) -> LaunchHostShareCache {
    LaunchHostShareCache { dax }
}

#[test]
fn launch_metadata_records_vhost_user_fs_and_restart_args() {
    let temp = TempDir::new("snapshot-vhost-user-fs-json");
    fs::create_dir_all(temp.path()).expect("create snapshot dir");
    let current = test_launch_metadata(
        "/Users/ramon",
        None,
        false,
        test_host_share_cache(false),
        vec![LaunchVhostUserFsMount {
            tag: "testfs".to_string(),
            mount: "/mnt/testfs".to_string(),
            socket: PathBuf::from("/tmp/testfs.sock"),
            read_only: true,
        }],
    );
    write_launch_metadata(&temp.path().join(LAUNCH_METADATA), &current)
        .expect("write launch metadata");
    let raw = fs::read_to_string(temp.path().join(LAUNCH_METADATA)).expect("read launch metadata");
    assert!(raw.contains("owner_args"));
    assert!(raw.contains("vhost_user_fs"));
    assert_eq!(snapshot_launch_incompatibility(temp.path(), &current), None);

    let mut changed_socket = current.clone();
    changed_socket.vhost_user_fs[0].socket = PathBuf::from("/tmp/other.sock");
    assert_eq!(
        snapshot_launch_incompatibility(temp.path(), &changed_socket),
        Some(
            "share_mismatch: vhost-user-fs: snapshot=testfs:/mnt/testfs:/tmp/testfs.sock:ro current=testfs:/mnt/testfs:/tmp/other.sock:ro"
                .to_string()
        )
    );
}

#[test]
fn snapshot_launch_compatibility_requires_matching_json() {
    let temp = TempDir::new("snapshot-launch-json");
    fs::create_dir_all(temp.path()).expect("create snapshot dir");
    let current = test_launch_metadata(
        "/Users/ramon",
        None,
        false,
        test_host_share_cache(false),
        Vec::new(),
    );

    assert_eq!(
        snapshot_launch_incompatibility(temp.path(), &current),
        Some("launch_metadata: snapshot has no launch.json".to_string())
    );

    write_launch_metadata(&temp.path().join(LAUNCH_METADATA), &current)
        .expect("write launch metadata");
    assert_eq!(snapshot_launch_incompatibility(temp.path(), &current), None);

    let mut drifted_home = current.clone();
    drifted_home.shares.host_home = Some(PathBuf::from("/home/ramon"));
    assert_eq!(
        snapshot_launch_incompatibility(temp.path(), &drifted_home),
        Some("share_mismatch: home: snapshot=/Users/ramon current=/home/ramon".to_string())
    );

    let disabled = test_launch_metadata(
        "/Users/ramon",
        None,
        true,
        test_host_share_cache(false),
        Vec::new(),
    );
    write_launch_metadata(&temp.path().join(LAUNCH_METADATA), &disabled)
        .expect("write disabled launch metadata");
    assert_eq!(
        snapshot_launch_incompatibility(temp.path(), &disabled),
        None
    );
    assert_eq!(
        snapshot_launch_incompatibility(temp.path(), &current),
        Some(
            "share_mismatch: host-shares: snapshot=disabled current=enabled; home: snapshot=<absent> current=/Users/ramon"
                .to_string()
        )
    );

    write_launch_metadata(&temp.path().join(LAUNCH_METADATA), &current)
        .expect("write launch metadata");
    let mut dax_current = current.clone();
    dax_current.compatibility.host_share_cache = test_host_share_cache(true);
    assert_eq!(
        snapshot_launch_incompatibility(temp.path(), &dax_current),
        Some("share_mismatch: host-share-cache: snapshot=nodax current=dax".to_string())
    );
}

#[test]
fn default_restore_version_matching_reads_launch_metadata() {
    let temp = TempDir::new("default-restore-version");
    fs::create_dir_all(temp.path()).expect("create snapshot dir");

    // No launch metadata: leave the decision to the general snapshot check.
    assert!(default_restore_version_matches(temp.path()).expect("match without metadata"));

    let current = test_launch_metadata(
        "/Users/ramon",
        None,
        true,
        test_host_share_cache(false),
        Vec::new(),
    );
    write_launch_metadata(&temp.path().join(LAUNCH_METADATA), &current)
        .expect("write launch metadata");
    assert!(default_restore_version_matches(temp.path()).expect("current version matches"));

    // A version-1 snapshot may carry the nix package-store virtiofs device
    // and must cold-boot instead of restoring.
    let legacy = serde_json::to_string(&current)
        .expect("encode launch metadata")
        .replace("\"version\":2", "\"version\":1");
    assert!(legacy.contains("\"version\":1"));
    fs::write(temp.path().join(LAUNCH_METADATA), legacy).expect("write legacy launch metadata");
    assert!(!default_restore_version_matches(temp.path()).expect("legacy version mismatches"));
}

#[test]
fn snapshot_launch_compatibility_tolerates_cwd_share_changes() {
    let temp = TempDir::new("snapshot-launch-cwd");
    fs::create_dir_all(temp.path()).expect("create snapshot dir");
    let snapshot = test_launch_metadata(
        "/Users/ramon",
        None,
        false,
        test_host_share_cache(false),
        Vec::new(),
    );
    let current = test_launch_metadata(
        "/Users/ramon",
        Some("/private/tmp"),
        false,
        test_host_share_cache(false),
        Vec::new(),
    );

    write_launch_metadata(&temp.path().join(LAUNCH_METADATA), &snapshot)
        .expect("write launch metadata");
    assert_eq!(snapshot_launch_incompatibility(temp.path(), &current), None);
}

#[test]
fn snapshot_share_layout_reads_recorded_launch_metadata() {
    let temp = TempDir::new("snapshot-share-layout-json");
    fs::create_dir_all(temp.path()).expect("create snapshot dir");
    let metadata = test_launch_metadata(
        "/Users/ramon",
        Some("/tmp/build"),
        false,
        test_host_share_cache(false),
        Vec::new(),
    );
    write_launch_metadata(&temp.path().join(LAUNCH_METADATA), &metadata)
        .expect("write launch metadata");

    let layout = snapshot_share_layout(temp.path())
        .expect("read layout")
        .expect("layout");

    assert_eq!(layout.metadata, metadata);
    assert_eq!(
        layout.layout,
        ShareLayout {
            host_home: PathBuf::from("/Users/ramon"),
            outside_home_cwd: Some(PathBuf::from("/tmp/build")),
            no_host_shares: false,
        }
    );
}

#[test]
fn snapshot_deterministic_compatibility_requires_matching_mode_and_seed() {
    let temp = TempDir::new("snapshot-deterministic");
    fs::create_dir_all(temp.path()).expect("create snapshot dir");
    let disabled = deterministic_stamp_content(None);
    let seed_a = DeterministicConfig {
        seed: "seed-a".to_string(),
    };
    let seed_b = DeterministicConfig {
        seed: "seed-b".to_string(),
    };
    let enabled_a = deterministic_stamp_content(Some(&seed_a));
    let enabled_b = deterministic_stamp_content(Some(&seed_b));

    assert_eq!(
        snapshot_deterministic_incompatibility(temp.path(), &disabled),
        None,
        "legacy snapshots without deterministic stamp remain nondeterministic-compatible"
    );
    assert_eq!(
        snapshot_deterministic_incompatibility(temp.path(), &enabled_a),
        Some("snapshot has no deterministic compatibility stamp".to_string())
    );

    fs::write(temp.path().join("deterministic.stamp"), &enabled_a).expect("write stamp");
    assert_eq!(
        snapshot_deterministic_incompatibility(temp.path(), &enabled_a),
        None
    );
    assert_eq!(
        snapshot_deterministic_incompatibility(temp.path(), &enabled_b),
        Some("seed: snapshot=seed-a current=seed-b".to_string())
    );
    assert_eq!(
            snapshot_deterministic_incompatibility(temp.path(), &disabled),
            Some(
                "deterministic: snapshot=enabled-v1 current=disabled-v1; seed: snapshot=seed-a current=<absent>; initial_realtime_unix_secs: snapshot=0 current=<absent>; clock_state: snapshot=deterministic-clock-state-v1 current=<absent>; restore_timer_rebase: snapshot=disabled-v1 current=<absent>; virtual_counter: snapshot=kvm-controlled-counter-v1 current=<absent>; kvm_halt_poll: snapshot=disabled-v1 current=<absent>; kvm_wfi_exit: snapshot=enabled-v1 current=<absent>; host_activity_gate: snapshot=broker-and-device-idle-v1 current=<absent>; rtc: snapshot=deterministic-zero-v1 current=<absent>; trng: snapshot=deterministic-smccc-v1 current=<absent>; virtio_rng: snapshot=deterministic-stateless-v1 current=<absent>; vsock_timesync: snapshot=disabled-v1 current=<absent>; restore_entropy: snapshot=sha256-seed-v1 current=<absent>; exec_user: snapshot=uid1000-gid1000-lnxuser current=<absent>; exec_env: snapshot=c-utf8-utc-v1 current=<absent>; exec_tty: snapshot=none-24x80-xterm-256color-v1 current=<absent>; network: snapshot=gvproxy-fixed-v1 current=<absent>"
                    .to_string()
            )
        );
}

#[test]
fn deterministic_time_configures_libkrun_restore_rebase_policy() {
    unsafe {
        std::env::remove_var("KRUN_DETERMINISTIC_TIME");
    }
    configure_libkrun_deterministic_time(true);
    assert_eq!(std::env::var("KRUN_DETERMINISTIC_TIME").as_deref(), Ok("1"));
    configure_libkrun_deterministic_time(false);
    assert!(std::env::var_os("KRUN_DETERMINISTIC_TIME").is_none());
}

#[test]
fn deterministic_clock_state_round_trips_and_restores_from_snapshot() {
    let temp = TempDir::new("deterministic-clock");
    fs::create_dir_all(temp.path()).expect("create snapshot dir");
    let state = DeterministicClockState {
        realtime_unix_nanos: 12,
        monotonic_nanos: 34,
        counter_frequency_hz: 1_000_000_000,
        event_sequence: 56,
        timer_jump_count: 7,
        last_timer_deadline_ticks: 890,
    };
    write_deterministic_clock_state(&temp.path().join(DETERMINISTIC_CLOCK_STATE), &state)
        .expect("write state");

    assert_eq!(read_deterministic_clock_state(temp.path()).unwrap(), state);
    assert_eq!(
        deterministic_clock_state_for_start(
            Some(&DeterministicConfig {
                seed: "seed42".to_string()
            }),
            Some(temp.path())
        )
        .unwrap(),
        Some(state)
    );
}

#[test]
fn deterministic_clock_event_sequence_tracks_trace_sequence() {
    let temp = TempDir::new("trace-clock-sequence");
    let instance_dir = temp.path().join("instances").join("trace-vm");
    let run_dir = instance_dir.clone();
    let layout = Layout {
        base: temp.path().to_path_buf(),
        instance: "trace-vm".to_string(),
        kernel: temp.path().join("vmlinuz"),
        rootfs: instance_dir.join("rootfs.ext4"),
        instance_dir: instance_dir.clone(),
        snapshot_dir: instance_dir.join("memory-snapshots"),
        checkpoint_dir: instance_dir.join("checkpoints"),
        vm_initialized: instance_dir.join("vm-initialized"),
        run_dir: run_dir.clone(),
        console_log: run_dir.join("console.log"),
    };
    fs::create_dir_all(&layout.run_dir).expect("create run dir");
    let trace = TraceLog::open(&layout).expect("open trace");
    trace.set_next_sequence(7);
    trace.event("restored_event", Vec::new());

    let state_path = layout.run_dir.join(DETERMINISTIC_CLOCK_STATE);
    write_deterministic_clock_state(&state_path, &initial_deterministic_clock_state())
        .expect("write state");
    sync_deterministic_clock_event_sequence(&layout.run_dir.join("initramfs.stamp"), Some(&trace))
        .expect("sync sequence");

    let state =
        parse_deterministic_clock_state(&fs::read_to_string(&state_path).expect("read state"))
            .expect("parse state");
    assert_eq!(state.event_sequence, 8);
    assert_eq!(state.timer_jump_count, 0);
    assert_eq!(state.last_timer_deadline_ticks, 0);
}

#[test]
fn deterministic_timer_jumps_import_into_trace_once() {
    let temp = TempDir::new("trace-timer-jumps");
    let instance_dir = temp.path().join("instances").join("trace-vm");
    let run_dir = instance_dir.clone();
    let layout = Layout {
        base: temp.path().to_path_buf(),
        instance: "trace-vm".to_string(),
        kernel: temp.path().join("vmlinuz"),
        rootfs: instance_dir.join("rootfs.ext4"),
        instance_dir: instance_dir.clone(),
        snapshot_dir: instance_dir.join("memory-snapshots"),
        checkpoint_dir: instance_dir.join("checkpoints"),
        vm_initialized: instance_dir.join("vm-initialized"),
        run_dir: run_dir.clone(),
        console_log: run_dir.join("console.log"),
    };
    fs::create_dir_all(&layout.run_dir).expect("create run dir");
    fs::write(
        layout.run_dir.join(DETERMINISTIC_TIMER_JUMPS),
        "deadline_ticks=10 counter_frequency_hz=1000000000 deadline_nanos=10\n",
    )
    .expect("write jumps");
    let trace = TraceLog::open(&layout).expect("open trace");

    import_deterministic_timer_jumps(&layout.run_dir.join("initramfs.stamp"), Some(&trace))
        .expect("import jumps");
    import_deterministic_timer_jumps(&layout.run_dir.join("initramfs.stamp"), Some(&trace))
        .expect("import jumps again");
    drop(trace);

    let connection =
        Connection::open(layout.run_dir.join("deterministic-trace.sqlite3")).expect("open db");
    let events: i64 = connection
        .query_row(
            "SELECT count(*) FROM events WHERE event = 'timer_jump'",
            [],
            |row| row.get(0),
        )
        .expect("count timer jumps");
    assert_eq!(events, 1);
    let deadline: i64 = connection
        .query_row(
            "SELECT value FROM event_integer_fields WHERE key = 'deadline_nanos'",
            [],
            |row| row.get(0),
        )
        .expect("read deadline");
    assert_eq!(deadline, 10);
}

#[test]
fn deterministic_restore_requires_clock_state() {
    let temp = TempDir::new("deterministic-clock-missing");
    fs::create_dir_all(temp.path()).expect("create snapshot dir");
    let err = deterministic_clock_state_for_start(
        Some(&DeterministicConfig {
            seed: "seed42".to_string(),
        }),
        Some(temp.path()),
    )
    .unwrap_err()
    .to_string();

    assert!(err.contains("deterministic-clock.state"));
}

#[test]
fn deterministic_exec_identity_and_env_are_host_independent() {
    let config = DeterministicConfig {
        seed: "seed42".to_string(),
    };
    assert_eq!(
        exec_identity(false, Some(&config)),
        (
            DETERMINISTIC_EXEC_UID,
            DETERMINISTIC_EXEC_GID,
            DETERMINISTIC_EXEC_GROUP.to_string()
        )
    );
    assert_eq!(exec_identity(true, Some(&config)), (0, 0, String::new()));
    assert_eq!(
        exec_env(Some(&config)),
        vec![
            ("TERM".to_string(), DETERMINISTIC_TERM.to_string()),
            ("LANG".to_string(), "C.UTF-8".to_string()),
            ("LC_ALL".to_string(), "C.UTF-8".to_string()),
            ("TZ".to_string(), "UTC".to_string()),
        ]
    );
}

#[test]
fn zone_from_localtime_target_parses_zoneinfo_paths() {
    assert_eq!(
        zone_from_localtime_target("/var/db/timezone/zoneinfo/America/New_York"),
        Some("America/New_York".to_string())
    );
    assert_eq!(
        zone_from_localtime_target("/usr/share/zoneinfo/Europe/Berlin"),
        Some("Europe/Berlin".to_string())
    );
    assert_eq!(zone_from_localtime_target("/usr/share/zoneinfo/"), None);
    assert_eq!(zone_from_localtime_target("/etc/localtime.copy"), None);
}

#[test]
fn deterministic_stamp_records_network_policy() {
    let config = DeterministicConfig {
        seed: "seed42".to_string(),
    };
    let stamp = deterministic_stamp_content(Some(&config));

    assert!(stamp.contains("network=gvproxy-fixed-v1\n"));
    assert!(stamp.contains("exec_env=c-utf8-utc-v1\n"));
    assert!(stamp.contains("restore_entropy=sha256-seed-v1\n"));
    assert!(stamp.contains("clock_state=deterministic-clock-state-v1\n"));
    assert!(stamp.contains("restore_timer_rebase=disabled-v1\n"));
    assert!(stamp.contains("virtual_counter=kvm-controlled-counter-v1\n"));
    assert!(stamp.contains("kvm_halt_poll=disabled-v1\n"));
    assert!(stamp.contains("kvm_wfi_exit=enabled-v1\n"));
    assert!(stamp.contains("host_activity_gate=broker-and-device-idle-v1\n"));
    assert!(stamp.contains("rtc=deterministic-zero-v1\n"));
    assert!(stamp.contains("trng=deterministic-smccc-v1\n"));
    assert!(stamp.contains("virtio_rng=deterministic-stateless-v1\n"));
    assert!(stamp.contains("vsock_timesync=disabled-v1\n"));
}

#[test]
fn deterministic_restore_entropy_depends_only_on_seed() {
    let seed_a_first = deterministic_restore_entropy("seed-a");
    let seed_a_second = deterministic_restore_entropy("seed-a");
    let seed_b = deterministic_restore_entropy("seed-b");

    assert_eq!(seed_a_first.len(), RESTORE_ENTROPY_BYTES);
    assert_eq!(seed_a_first, seed_a_second);
    assert_ne!(seed_a_first, seed_b);
    assert!(seed_a_first.iter().any(|byte| *byte != 0));
}

#[test]
fn deterministic_request_ids_depend_on_seed_and_exec_context() {
    let command = vec!["pytest".to_string(), "-q".to_string()];
    let first = deterministic_exec_request_id("seed42", &command, "/", false, false, 1, 1);
    let second = deterministic_exec_request_id("seed42", &command, "/", false, false, 1, 1);
    let different_seed =
        deterministic_exec_request_id("other-seed", &command, "/", false, false, 1, 1);
    let different_command = deterministic_exec_request_id(
        "seed42",
        &["pytest".to_string(), "-vv".to_string()],
        "/",
        false,
        false,
        1,
        1,
    );

    assert_ne!(first, 0);
    assert_eq!(first, second);
    assert_ne!(first, different_seed);
    assert_ne!(first, different_command);
    assert_eq!(
        deterministic_restore_sync_request_id("seed42"),
        deterministic_restore_sync_request_id("seed42")
    );
    assert_ne!(
        deterministic_restore_sync_request_id("seed42"),
        deterministic_restore_sync_request_id("other-seed")
    );
}

#[test]
fn trace_log_stores_ordered_events_in_independent_sqlite_db() {
    let temp = TempDir::new("trace-log");
    let instance_dir = temp.path().join("instances").join("trace-vm");
    let run_dir = instance_dir.clone();
    let layout = Layout {
        base: temp.path().to_path_buf(),
        instance: "trace-vm".to_string(),
        kernel: temp.path().join("vmlinuz"),
        rootfs: instance_dir.join("rootfs.ext4"),
        instance_dir: instance_dir.clone(),
        snapshot_dir: instance_dir.join("memory-snapshots"),
        checkpoint_dir: instance_dir.join("checkpoints"),
        vm_initialized: instance_dir.join("vm-initialized"),
        run_dir: run_dir.clone(),
        console_log: run_dir.join("console.log"),
    };
    fs::create_dir_all(&layout.run_dir).expect("create run dir");
    let trace = TraceLog::open(&layout).expect("open trace");

    trace.event("vm_start_config", vec![trace_text("seed", "seed42")]);
    trace.event("guest_exit_status", vec![trace_integer("status", 0)]);
    drop(trace);

    let connection =
        Connection::open(layout.run_dir.join("deterministic-trace.sqlite3")).expect("open db");
    let format: String = connection
        .query_row(
            "SELECT value FROM trace_metadata WHERE key = 'format'",
            [],
            |row| row.get(0),
        )
        .expect("read metadata");
    assert_eq!(format, "lnx-deterministic-trace-v1");

    let events = connection
        .prepare("SELECT sequence, event FROM events ORDER BY sequence")
        .expect("prepare events")
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .expect("query events")
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("collect events");

    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, 0);
    assert_eq!(events[0].1, "vm_start_config");
    assert_eq!(events[1].0, 1);
    assert_eq!(events[1].1, "guest_exit_status");

    let seed: String = connection
        .query_row(
            "SELECT value FROM event_text_fields WHERE sequence = 0 AND key = 'seed'",
            [],
            |row| row.get(0),
        )
        .expect("read seed");
    assert_eq!(seed, "seed42");
    let status: i64 = connection
        .query_row(
            "SELECT value FROM event_integer_fields WHERE sequence = 1 AND key = 'status'",
            [],
            |row| row.get(0),
        )
        .expect("read status");
    assert_eq!(status, 0);
}

#[test]
fn initramfs_stamp_key_prefers_source_but_keeps_sha256_compatibility() {
    let temp = TempDir::new("initramfs-stamp-key");
    let stamp = temp.path().join("initramfs.stamp");

    fs::write(&stamp, "sha256=old-binary-hash\n").expect("write legacy stamp");
    assert_eq!(
        initramfs_stamp_key(&stamp),
        Some("sha256=old-binary-hash".to_string())
    );

    fs::write(
        &stamp,
        "sha256=old-binary-hash\nsource=guest-agent-source-hash\n",
    )
    .expect("write mixed stamp");
    assert_eq!(
        initramfs_stamp_key(&stamp),
        Some("source=guest-agent-source-hash".to_string())
    );

    fs::write(&stamp, "unrelated=true\n").expect("write unrelated stamp");
    assert_eq!(initramfs_stamp_key(&stamp), None);
}

#[test]
fn snapshot_initramfs_compatibility_requires_matching_source_stamp() {
    let temp = TempDir::new("snapshot-initramfs-compatibility");
    let snapshot = temp.path().join("snapshot");
    fs::create_dir(&snapshot).expect("create snapshot");
    let current = temp.path().join("initramfs.stamp");

    fs::write(snapshot.join("initramfs.stamp"), "source=old\n").expect("write snapshot stamp");
    fs::write(&current, "source=new\n").expect("write current stamp");
    assert!(!snapshot_initramfs_is_compatible(&snapshot, &current));

    fs::write(&current, "source=old\n").expect("write matching current stamp");
    assert!(snapshot_initramfs_is_compatible(&snapshot, &current));
}

#[test]
fn snapshot_vm_config_rejects_bad_magic_and_version() {
    let temp = TempDir::new("snapshot-bad");
    fs::create_dir_all(temp.path()).expect("create snapshot dir");
    fs::write(temp.path().join("vmstate.bin"), [0u8; 40]).expect("write bad vmstate");
    assert!(snapshot_vm_config(temp.path()).is_err());

    let mut header = [0u8; 40];
    header[0..8].copy_from_slice(b"LKRNSS01");
    header[8..12].copy_from_slice(&(SNAPSHOT_VMSTATE_VERSION + 97).to_le_bytes());
    fs::write(temp.path().join("vmstate.bin"), header).expect("write bad version");
    assert!(snapshot_vm_config(temp.path()).is_err());
}

#[test]
fn framed_message_round_trips_over_unix_stream() {
    let (mut left, mut right) = UnixStream::pair().expect("unix pair");
    let message = Message::Data {
        channel_id: 7,
        bytes: b"hello".to_vec(),
    };

    write_message(&mut left, &message).expect("write message");
    let decoded = read_message(&mut right).expect("read message");

    assert_eq!(decoded, message);
}

#[test]
fn framed_message_rejects_oversized_writes_and_reads() {
    let (mut left, mut right) = UnixStream::pair().expect("unix pair");
    let too_large = Message::Data {
        channel_id: 1,
        bytes: vec![0; MAX_MESSAGE_SIZE as usize + 1],
    };
    assert!(write_message(&mut left, &too_large).is_err());

    left.write_all(&(MAX_MESSAGE_SIZE + 1).to_be_bytes())
        .expect("write oversized length");
    assert!(read_message(&mut right).is_err());
}

#[test]
fn broker_idle_ttl_defaults_to_immediate_shutdown() {
    assert_eq!(broker_idle_ttl_from_env(None), Duration::ZERO);
    assert_eq!(broker_idle_ttl_from_env(Some("nope")), Duration::ZERO);
}

#[test]
fn broker_idle_ttl_reads_milliseconds_from_env() {
    assert_eq!(
        broker_idle_ttl_from_env(Some("30000")),
        Duration::from_secs(30)
    );
}

#[test]
fn owner_idle_ttl_defaults_to_five_second_grace() {
    assert_eq!(owner_idle_ttl_from_env(None), Duration::from_secs(5));
    assert_eq!(
        owner_idle_ttl_from_env(Some("nope")),
        Duration::from_secs(5)
    );
}

#[test]
fn owner_idle_ttl_reads_env_but_clamps_to_minimum() {
    assert_eq!(
        owner_idle_ttl_from_env(Some("30000")),
        Duration::from_secs(30)
    );
    assert_eq!(
        owner_idle_ttl_from_env(Some("0")),
        Duration::from_millis(250)
    );
}

#[test]
fn debug_flag_parses_nodaemonreuse_token() {
    assert!(debug_flag_enabled_in(
        Some("trace,nodaemonreuse;other"),
        "nodaemonreuse"
    ));
    assert!(debug_flag_enabled_in(
        Some("trace nodaemonreuse"),
        "nodaemonreuse"
    ));
    assert!(!debug_flag_enabled_in(
        Some("trace-nodaemonreuse"),
        "nodaemonreuse"
    ));
    assert!(!debug_flag_enabled_in(None, "nodaemonreuse"));
}

#[test]
fn rootfs_backend_defaults_to_pmem() {
    assert_eq!(RootfsBackend::from_env(None).unwrap(), RootfsBackend::Pmem);
    assert_eq!(
        RootfsBackend::from_env(Some(String::new())).unwrap(),
        RootfsBackend::Pmem
    );
}

#[test]
fn rootfs_backend_rejects_block() {
    assert!(RootfsBackend::from_env(Some("block".to_string())).is_err());
}

#[test]
fn rootfs_backend_rejects_unknown_values() {
    assert!(RootfsBackend::from_env(Some("virtiofs".to_string())).is_err());
}

#[test]
fn forward_spec_round_trips_the_cli_format() {
    let forward = PortForward {
        listen_host: "127.0.0.1".to_string(),
        listen_port: 16081,
        guest_host: "localhost".to_string(),
        guest_port: 6080,
    };
    assert_eq!(forward_spec(&forward), "127.0.0.1:16081:localhost:6080");
}

#[test]
fn localhost_url_forward_keeps_loopback_family() {
    assert_eq!(
        localhost_url_forward("http://localhost:3773/pair"),
        Some(("127.0.0.1", 3773))
    );
    assert_eq!(
        localhost_url_forward("https://127.0.0.1:8443/callback"),
        Some(("127.0.0.1", 8443))
    );
    assert_eq!(
        localhost_url_forward("http://[::1]:5173/"),
        Some(("::1", 5173))
    );
    assert_eq!(localhost_url_forward("https://example.com:443/"), None);
}

#[test]
fn existing_broker_client_propagates_protocol_mismatch() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket = PathBuf::from(format!("/tmp/lnx-bp-{}-{unique}.sock", std::process::id()));
    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).expect("listen broker");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept broker");
        let _ = read_message(&mut stream).expect("read client hello");
        write_message(
            &mut stream,
            &Message::Hello {
                version: PROTOCOL_VERSION - 1,
            },
        )
        .expect("write stale hello");
    });

    let err = run_existing_broker_client(
        &socket,
        &["true".to_string()],
        Path::new("/"),
        true,
        true,
        None,
        "default",
        None,
    )
    .expect_err("protocol mismatch should fail fast");
    server.join().expect("broker thread");
    let _ = fs::remove_file(&socket);

    assert!(err.downcast_ref::<BrokerProtocolMismatch>().is_some());
}

#[test]
fn existing_broker_client_treats_missing_hello_as_not_ready() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket = PathBuf::from(format!("/tmp/lnx-bh-{}-{unique}.sock", std::process::id()));
    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).expect("listen broker");
    let server = thread::spawn(move || {
        let (_stream, _) = listener.accept().expect("accept broker");
    });

    let status = run_existing_broker_client(
        &socket,
        &["true".to_string()],
        Path::new("/"),
        true,
        true,
        None,
        "default",
        None,
    )
    .expect("missing hello is transient");
    server.join().expect("broker thread");
    let _ = fs::remove_file(&socket);

    assert_eq!(status, None);
}

#[test]
fn guest_cwd_uses_host_path_under_home() {
    assert_eq!(
        guest_cwd(Path::new("/Users/ramon/src/lnx"), Path::new("/Users/ramon")),
        "/Users/ramon/src/lnx"
    );
}

#[test]
fn guest_cwd_uses_host_path_outside_home() {
    assert_eq!(
        guest_cwd(Path::new("/tmp/build"), Path::new("/Users/ramon")),
        "/tmp/build"
    );
}

#[test]
fn host_home_for_cwd_uses_mounted_macos_home() {
    assert_eq!(
        host_home_for_cwd(Path::new("/Users/ramon/src/lnx")).unwrap(),
        PathBuf::from("/Users/ramon")
    );
}

#[test]
fn home_write_allowlist_is_relative_under_home() {
    assert_eq!(
        home_write_allowlist(Path::new("/Users/ramon/src/lnx"), Path::new("/Users/ramon")),
        vec!["src/lnx".to_string()]
    );
}

#[test]
fn home_write_allowlist_uses_dot_for_home_root() {
    assert_eq!(
        home_write_allowlist(Path::new("/Users/ramon"), Path::new("/Users/ramon")),
        vec![".".to_string()]
    );
}

#[test]
fn home_write_allowlist_is_empty_outside_home() {
    assert!(home_write_allowlist(Path::new("/tmp/build"), Path::new("/Users/ramon")).is_empty());
}

#[test]
fn cwd_write_allowlist_allows_entire_outside_home_cwd_share() {
    assert_eq!(cwd_write_allowlist(), vec![".".to_string()]);
}
