use super::*;
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

    promote_snapshot_rootfs(&snapshot, &layout.rootfs, &timings, &run_log)
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

    let error = promote_snapshot_rootfs(&snapshot, &layout.rootfs, &timings, &run_log)
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

#[test]
fn shares_stamp_content_lists_home_cwd_and_network() {
    assert_eq!(
        shares_stamp_content_with_cache_stamp(
            Path::new("/Users/ramon"),
            None,
            "net=gvproxy",
            false,
            HOST_SHARE_CACHE_NODAX_STAMP,
        ),
        "host-share-cache=nodax+keep-cache+writeback+restore-sync-v1\nhome=/Users/ramon\nnet=gvproxy\n"
    );
    assert_eq!(
        shares_stamp_content_with_cache_stamp(
            Path::new("/Users/ramon"),
            Some(Path::new("/tmp/build")),
            "net=vmnet:prefix=24:gateway=192.168.106.0",
            false,
            HOST_SHARE_CACHE_NODAX_STAMP,
        ),
        "host-share-cache=nodax+keep-cache+writeback+restore-sync-v1\nhome=/Users/ramon\ncwd=/tmp/build\nnet=vmnet:prefix=24:gateway=192.168.106.0\n"
    );
    assert_eq!(
        shares_stamp_content_with_cache_stamp(
            Path::new("/Users/ramon"),
            None,
            "net=gvproxy",
            true,
            HOST_SHARE_CACHE_NODAX_STAMP,
        ),
        "host-shares=disabled-v1\nnet=gvproxy\n"
    );
    assert_eq!(
        shares_stamp_content_with_cache_stamp(
            Path::new("/Users/ramon"),
            None,
            "net=gvproxy",
            false,
            HOST_SHARE_CACHE_DAX_STAMP,
        ),
        "host-share-cache=dax+keep-cache+writeback+restore-sync-v1\nhome=/Users/ramon\nnet=gvproxy\n"
    );
}

#[test]
fn shares_stamp_content_records_package_store() {
    assert_eq!(
        shares_stamp_content_with_cache_and_package_store(
            Path::new("/Users/ramon"),
            None,
            "net=gvproxy",
            false,
            HOST_SHARE_CACHE_NODAX_STAMP,
            "packages=readonly-v1 root=/Users/ramon/.lnx/stores/nix-linux-aarch64",
        ),
        "host-share-cache=nodax+keep-cache+writeback+restore-sync-v1\nhome=/Users/ramon\npackages=readonly-v1 root=/Users/ramon/.lnx/stores/nix-linux-aarch64\nnet=gvproxy\n"
    );
}

#[test]
fn snapshot_shares_compatibility_requires_identical_stamp() {
    let temp = TempDir::new("snapshot-shares");
    fs::create_dir_all(temp.path()).expect("create snapshot dir");
    let current = shares_stamp_content(Path::new("/Users/ramon"), None, "net=gvproxy", false);

    // A snapshot from before share/cache-policy stamping may preserve
    // unsafe host-file cache state, so it must not memory-restore.
    assert_eq!(
        snapshot_shares_incompatibility(temp.path(), &current),
        Some(
            "host_share_cache_policy: snapshot has no host-share/network compatibility stamp"
                .to_string()
        )
    );

    fs::write(temp.path().join("shares.stamp"), &current).expect("write stamp");
    assert_eq!(snapshot_shares_incompatibility(temp.path(), &current), None);

    let legacy_cache_policy = "home=/Users/ramon\nnet=gvproxy\n";
    fs::write(temp.path().join("shares.stamp"), legacy_cache_policy).expect("write stamp");
    assert_eq!(
            snapshot_shares_incompatibility(temp.path(), &current),
            Some(
                "host_share_cache_policy: snapshot was created before host-share cache policy was recorded"
                    .to_string()
            )
        );

    let drifted = shares_stamp_content(Path::new("/home/ramon"), None, "net=gvproxy", false);
    fs::write(temp.path().join("shares.stamp"), &current).expect("write stamp");
    assert_eq!(
        snapshot_shares_incompatibility(temp.path(), &drifted),
        Some("share_mismatch: home: snapshot=/Users/ramon current=/home/ramon".to_string())
    );

    // The same shares on a different network backing must not restore.
    let renetworked = shares_stamp_content(
        Path::new("/Users/ramon"),
        None,
        "net=vmnet:prefix=24:gateway=192.168.106.0",
        false,
    );
    assert_eq!(
        snapshot_shares_incompatibility(temp.path(), &renetworked),
        Some(
            "share_mismatch: net: snapshot=gvproxy current=vmnet:prefix=24:gateway=192.168.106.0"
                .to_string()
        )
    );

    let disabled = shares_stamp_content(Path::new("/Users/ramon"), None, "net=gvproxy", true);
    fs::write(temp.path().join("shares.stamp"), &disabled).expect("write stamp");
    assert_eq!(
        snapshot_shares_incompatibility(temp.path(), &disabled),
        None
    );

    assert_eq!(
            snapshot_shares_incompatibility(temp.path(), &current),
            Some(
                "share_mismatch: host-shares: snapshot=disabled current=enabled; host-share-cache: snapshot=<absent> current=nodax+keep-cache+writeback+restore-sync-v1; home: snapshot=<absent> current=/Users/ramon"
                    .to_string()
            )
        );

    fs::write(temp.path().join("shares.stamp"), &current).expect("write stamp");
    assert_eq!(
            snapshot_shares_incompatibility(temp.path(), &disabled),
            Some(
                "share_mismatch: host-shares: snapshot=enabled current=disabled; host-share-cache: snapshot=nodax+keep-cache+writeback+restore-sync-v1 current=<absent>; home: snapshot=/Users/ramon current=<absent>"
                    .to_string()
            )
        );

    let dax_current = shares_stamp_content_with_cache_stamp(
        Path::new("/Users/ramon"),
        None,
        "net=gvproxy",
        false,
        HOST_SHARE_CACHE_DAX_STAMP,
    );
    assert_eq!(
            snapshot_shares_incompatibility(temp.path(), &dax_current),
            Some(
                "share_mismatch: host-share-cache: snapshot=nodax+keep-cache+writeback+restore-sync-v1 current=dax+keep-cache+writeback+restore-sync-v1"
                    .to_string()
            )
        );
}

#[test]
fn snapshot_shares_compatibility_requires_matching_package_store() {
    let temp = TempDir::new("snapshot-package-shares");
    fs::create_dir_all(temp.path()).expect("create snapshot dir");
    let disabled = shares_stamp_content(Path::new("/Users/ramon"), None, "net=gvproxy", false);
    let enabled = shares_stamp_content_with_cache_and_package_store(
        Path::new("/Users/ramon"),
        None,
        "net=gvproxy",
        false,
        HOST_SHARE_CACHE_NODAX_STAMP,
        "packages=readonly-v1 root=/Users/ramon/.lnx/stores/nix-linux-aarch64",
    );

    fs::write(temp.path().join("shares.stamp"), &disabled).expect("write stamp");
    assert_eq!(
        snapshot_shares_incompatibility(temp.path(), &enabled),
        Some(
            "share_mismatch: packages: snapshot=disabled-v1 current=readonly-v1 root=/Users/ramon/.lnx/stores/nix-linux-aarch64"
                .to_string()
        )
    );

    let disabled_with_explicit_package = shares_stamp_content_with_cache_and_package_store(
        Path::new("/Users/ramon"),
        None,
        "net=gvproxy",
        false,
        HOST_SHARE_CACHE_NODAX_STAMP,
        PACKAGE_STORE_DISABLED_STAMP,
    );
    fs::write(
        temp.path().join("shares.stamp"),
        &disabled_with_explicit_package,
    )
    .expect("write stamp");
    assert_eq!(
        snapshot_shares_incompatibility(temp.path(), &disabled),
        None
    );
}

#[test]
fn snapshot_shares_compatibility_tolerates_cwd_share_changes() {
    let temp = TempDir::new("snapshot-shares-cwd");
    fs::create_dir_all(temp.path()).expect("create snapshot dir");
    let snapshot = shares_stamp_content(Path::new("/Users/ramon"), None, "net=gvproxy", false);
    let current = shares_stamp_content(
        Path::new("/Users/ramon"),
        Some(Path::new("/private/tmp")),
        "net=gvproxy",
        false,
    );

    fs::write(temp.path().join("shares.stamp"), snapshot).expect("write stamp");
    assert_eq!(snapshot_shares_incompatibility(temp.path(), &current), None);
}

#[test]
fn snapshot_share_layout_reads_recorded_share_dirs() {
    let temp = TempDir::new("snapshot-share-layout");
    fs::create_dir_all(temp.path()).expect("create snapshot dir");
    let stamp = shares_stamp_content(
        Path::new("/Users/ramon"),
        Some(Path::new("/tmp/build")),
        "net=gvproxy",
        false,
    );
    fs::write(temp.path().join("shares.stamp"), &stamp).expect("write stamp");

    let layout = snapshot_share_layout(temp.path())
        .expect("read layout")
        .expect("layout");

    assert_eq!(layout.stamp, stamp);
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
                "deterministic: snapshot=enabled-v1 current=disabled-v1; seed: snapshot=seed-a current=<absent>; initial_realtime_unix_secs: snapshot=0 current=<absent>; clock_state: snapshot=deterministic-clock-state-v1 current=<absent>; restore_timer_rebase: snapshot=disabled-v1 current=<absent>; virtual_counter: snapshot=kvm-controlled-counter-v1 current=<absent>; kvm_halt_poll: snapshot=disabled-v1 current=<absent>; rtc: snapshot=deterministic-zero-v1 current=<absent>; trng: snapshot=deterministic-smccc-v1 current=<absent>; virtio_rng: snapshot=deterministic-stateless-v1 current=<absent>; vsock_timesync: snapshot=disabled-v1 current=<absent>; restore_entropy: snapshot=sha256-seed-v1 current=<absent>; exec_user: snapshot=uid1000-gid1000-lnxuser current=<absent>; exec_env: snapshot=c-utf8-utc-v1 current=<absent>; exec_tty: snapshot=none-24x80-xterm-256color-v1 current=<absent>; network: snapshot=gvproxy-fixed-v1 current=<absent>"
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

#[cfg(target_os = "macos")]
#[test]
fn instance_macs_are_stable_local_and_unicast() {
    let mac = instance_mac("default");
    assert_eq!(mac, instance_mac("default"));
    assert_ne!(mac, instance_mac("other"));
    // Locally administered, unicast.
    assert_eq!(mac[0] & 0x02, 0x02);
    assert_eq!(mac[0] & 0x01, 0x00);
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
