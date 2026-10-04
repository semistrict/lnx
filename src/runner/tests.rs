use super::*;
use rusqlite::Connection;
use std::{
    io::Write,
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
        rootfs: None,
        instance_dir: instance_dir.clone(),
        run_dir: instance_dir.clone(),
        console_log: instance_dir.join("console.log"),
    }
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

fn test_broker() -> (Arc<BrokerState>, mpsc::Receiver<Message>, TempDir) {
    let temp = TempDir::new("broker-state");
    let layout = temp_layout(&temp, "vm");
    fs::create_dir_all(&layout.run_dir).expect("create run dir");
    let run_log = Arc::new(RunLog::open(&layout).expect("run log"));
    let (agent_tx, agent_rx) = mpsc::channel();
    (
        BrokerState::new(agent_tx, false, || Ok(()), run_log),
        agent_rx,
        temp,
    )
}

fn open_exec(state: &BrokerState, channel_id: u64) -> (ChannelAdmission, mpsc::Receiver<Message>) {
    let (tx, rx) = mpsc::channel();
    let admission = state
        .open_channel(
            channel_id,
            BrokerChannel {
                tx,
                counts_as_active: true,
            },
            Message::Close { channel_id },
            || Ok(()),
        )
        .expect("open channel");
    (admission, rx)
}

#[test]
fn agent_reader_failure_notifies_waiting_clients() {
    let (state, _agent_rx, _temp) = test_broker();
    let channel_id = 0xabcddcba_u64;
    let (_, rx) = open_exec(&state, channel_id);

    let dropped = state.drain(Some("guest agent disconnected before command completed"));

    assert_eq!(dropped, 1);
    assert_eq!(state.active_channels(), 0);
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
fn broker_shutdown_closes_registration_gate_before_draining_clients() {
    let (state, agent_rx, _temp) = test_broker();
    let channel_id = 0x1234_u64;
    let (_, rx) = open_exec(&state, channel_id);
    assert!(matches!(
        agent_rx.recv().expect("open forwarded"),
        Message::Close { .. }
    ));

    let dropped = state.begin_shutdown(OWNER_STOPPING);

    assert!(state.is_stopping());
    assert_eq!(dropped, 1);
    assert_eq!(state.active_channels(), 0);
    assert_eq!(
        agent_rx.try_recv().expect("the guest is told to end the command"),
        Message::Close { channel_id }
    );
    assert!(matches!(
        rx.recv().expect("shutdown error"),
        Message::Error { channel_id: id, .. } if id == channel_id
    ));
    assert!(
        !state
            .send_to_agent(Message::Eof { channel_id })
            .expect("send")
    );
    assert!(matches!(
        open_exec(&state, 0x5678).0,
        ChannelAdmission::Stopping
    ));
    assert!(
        agent_rx.try_recv().is_err(),
        "nothing reaches the agent after the barrier"
    );
}

/// A run of `layout` that has served a command and written `disk`.
fn dirty_session(layout: &Layout, disk: &[u8]) -> RunSession {
    crate::store::test_support::initialized(layout, b"saved disk", true);
    let run_log = Arc::new(RunLog::open(layout).expect("run log"));
    let lock = test_support::hold_as_owner(layout);
    let session = RunSession::begin(layout, lock, None, run_log).expect("begin run");
    session.mark_dirty().expect("mark dirty");
    fs::write(session.run().rootfs(), disk).expect("guest writes");
    session
}

#[test]
fn a_failed_final_capture_after_the_guest_flushed_keeps_the_disk() {
    let temp = TempDir::new("session-keep-disk");
    let layout = temp_layout(&temp, "vm");
    let session = dirty_session(&layout, b"acknowledged writes");

    session.guest_quiesced();
    session
        .abandon(&anyhow!("VM I/O operation failed"))
        .expect("abandon");
    drop(session);

    assert_eq!(
        crate::store::test_support::latest_disk(&layout),
        b"acknowledged writes"
    );
    refuse_crashed_run(&layout).expect("no recovery needed");
    assert_eq!(
        fs::read_to_string(layout.instance_dir.join(LAST_RUN_NOTICE)).expect("notice"),
        "lnx: the memory of vm's last run could not be saved (VM I/O operation failed); its disk was kept, so this run boots from it\n"
    );
}

#[test]
fn a_run_that_ended_before_the_guest_flushed_is_left_for_recover() {
    let temp = TempDir::new("session-left-dirty");
    let layout = temp_layout(&temp, "vm");
    let session = dirty_session(&layout, b"unflushed writes");

    session.abandon(&anyhow!("VM exited")).expect("abandon");
    drop(session);

    let error = refuse_crashed_run(&layout).expect_err("recovery needed");
    assert!(format!("{error:#}").contains("recover --keep"));
    assert!(!layout.instance_dir.join(LAST_RUN_NOTICE).exists());
}

fn relay_after(message: &str) -> anyhow::Error {
    let (mut client, mut broker) = UnixStream::pair().expect("socket pair");
    write_message(
        &mut broker,
        &Message::Error {
            channel_id: 7,
            message: message.to_string(),
        },
    )
    .expect("write error");
    let writer = SharedStream::new(&client).expect("writer");
    relay_channel_output(&mut client, &writer, 7, None).expect_err("relay fails")
}

#[test]
fn a_command_refused_by_a_stopping_owner_can_be_retried() {
    assert!(relay_after(OWNER_STOPPING_NOT_STARTED).is::<CommandNotStarted>());
}

#[test]
fn a_command_stopped_while_running_is_not_retried() {
    let error = relay_after(OWNER_STOPPING);
    assert!(!error.is::<CommandNotStarted>());
    assert_eq!(error.to_string(), OWNER_STOPPING);
}

#[test]
fn an_owner_whose_guest_panicked_at_boot_is_reported_as_such() {
    use std::os::unix::process::ExitStatusExt;
    let temp = TempDir::new("early-exit-summary");
    let console = temp.path().join("console.log");
    let status = std::process::ExitStatus::from_raw(0);

    fs::write(&console, "[    0.11] Kernel panic - not syncing: Attempted to kill init!\n")
        .expect("write console");
    assert_eq!(
        early_exit_summary(&console, status),
        "the guest kernel panicked while booting"
    );

    fs::write(&console, "[    0.11] booting\n").expect("write console");
    assert_eq!(
        early_exit_summary(&console, status),
        "lnx VM owner exited with exit status: 0 before the broker came up"
    );
}

#[test]
fn a_new_owner_waits_for_its_first_client_before_it_may_stop() {
    let (state, _agent_rx, _temp) = test_broker();

    let status = state.idle_status();
    assert!(!status.busy, "waiting does not restart the idle timer");
    assert!(status.pending, "the starting client is still on its way");

    drop(state.pending_connection());
    assert!(
        state.idle_status().pending,
        "a status probe is not the client that started the owner"
    );

    let channel_id = 0x77;
    open_exec(&state, channel_id);
    state.deliver_to_client(channel_id, Message::Close { channel_id });
    assert!(!state.idle_status().pending, "the first client has arrived");
}

#[test]
fn pending_connections_delay_stopping_without_restarting_the_idle_timer() {
    let (state, _agent_rx, _temp) = test_broker();
    // The client that started the owner has come and gone.
    open_exec(&state, 0x1);
    state.deliver_to_client(0x1, Message::Close { channel_id: 0x1 });

    let probe = state.pending_connection();
    let status = state.idle_status();
    assert!(!status.busy, "a status probe must not keep the VM awake");
    assert!(status.pending);

    drop(probe);
    assert_eq!(
        state.idle_status(),
        IdleStatus {
            busy: false,
            pending: false,
            seen_active: true,
        }
    );
}

#[test]
fn open_channels_keep_the_vm_busy_until_the_agent_closes_them() {
    let (state, _agent_rx, _temp) = test_broker();
    let channel_id = 0x42_u64;

    assert!(matches!(
        open_exec(&state, channel_id).0,
        ChannelAdmission::Opened
    ));
    assert!(matches!(
        open_exec(&state, channel_id).0,
        ChannelAdmission::Collision
    ));
    assert!(state.idle_status().busy);
    assert!(state.idle_status().seen_active);

    state.deliver_to_client(channel_id, Message::Close { channel_id });

    assert_eq!(state.active_channels(), 0);
    assert!(!state.idle_status().busy);
}

#[test]
fn lingering_forward_keeps_the_vm_awake() {
    let (state, _agent_rx, _temp) = test_broker();

    state.keep_awake_for(Duration::from_secs(60));

    assert!(state.idle_status().busy);
}

struct RecordingCaptures {
    log: Arc<Mutex<Vec<String>>>,
}

impl Captures for RecordingCaptures {
    fn checkpoint(&self, spec: &CheckpointSpec) -> Result<()> {
        // Slow enough that a worker which did not wait would be caught.
        thread::sleep(Duration::from_millis(20));
        self.log.lock().unwrap().push(format!(
            "checkpoint {}",
            spec.id.as_deref().unwrap_or("none")
        ));
        Ok(())
    }

    fn snapshot_exit(&self, channel_id: u64) {
        self.log
            .lock()
            .unwrap()
            .push(format!("snapshot-exit {channel_id}"));
    }
}

#[test]
fn capture_worker_runs_jobs_in_order_and_finishes_them_before_stopping() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let worker = CaptureWorker::spawn(RecordingCaptures {
        log: Arc::clone(&log),
    });
    let jobs = worker.jobs();
    let (reply_tx, reply_rx) = mpsc::channel();
    jobs.send(CaptureJob::Checkpoint(CheckpointRequest {
        spec: CheckpointSpec {
            id: Some("a".to_string()),
            ..CheckpointSpec::default()
        },
        reply: reply_tx,
    }))
    .expect("queue checkpoint");
    jobs.send(CaptureJob::SnapshotExit { channel_id: 7 })
        .expect("queue snapshot-exit");

    worker.finish().expect("finish worker");

    assert_eq!(*log.lock().unwrap(), ["checkpoint a", "snapshot-exit 7"]);
    assert_eq!(reply_rx.recv().expect("checkpoint reply"), Ok(()));
}

#[test]
fn fresh_owner_slot_accepts_a_dead_owners_acknowledged_lease() {
    let temp = TempDir::new("fresh-owner-stale-lock");
    let layout = temp_layout(&temp, "vm");
    fs::create_dir_all(&layout.run_dir).expect("create run dir");
    let dead_owner = locks::test_support::exited_process();
    locks::test_support::write_instance_lease(
        &layout,
        &locks::test_support::lease_for(LeaseRole::Owner, dead_owner),
    );
    let run_log = RunLog::open(&layout).expect("open run log");

    wait_for_fresh_owner_slot(&layout, &run_log).expect("stale lease should be validated");

    let replacement = InstanceLock::try_acquire(&layout, LeaseRole::Owner, |_| Ok(()))
        .expect("replace stale lease")
        .expect("replacement lock");
    assert_eq!(
        live_owner(&layout).map(|lease| lease.process),
        Some(ProcessIdentity::current())
    );
    drop(replacement);
}

#[test]
fn validated_instance_lock_reclaims_a_dead_maintenance_lease() {
    let temp = TempDir::new("fresh-owner-stale-maintenance");
    let layout = temp_layout(&temp, "vm");
    fs::create_dir_all(&layout.run_dir).expect("create run dir");
    locks::test_support::write_instance_lease(
        &layout,
        &locks::test_support::lease_for(
            LeaseRole::Maintenance,
            locks::test_support::exited_process(),
        ),
    );

    let replacement = InstanceLock::try_acquire(&layout, LeaseRole::Owner, |_| Ok(()))
        .expect("reclaim dead maintenance lease")
        .expect("replacement owner lease");

    let state = instance_lock_state(&layout).expect("inspect lock");
    assert_eq!(
        state.live_owner().map(|lease| lease.process),
        Some(ProcessIdentity::current())
    );
    drop(replacement);
}

#[test]
fn fresh_owner_slot_replace_stops_recorded_owner() {
    let temp = TempDir::new("fresh-owner-replace");
    let layout = temp_layout(&temp, "vm");
    fs::create_dir_all(&layout.run_dir).expect("create run dir");
    let holder = locks::test_support::spawn_foreign_holder(&layout, LeaseRole::Owner, "exit 0");
    let owner = holder.process;
    let run_log = RunLog::open(&layout).expect("open run log");
    let reaper = thread::spawn(move || {
        let mut holder = holder;
        holder.wait()
    });

    prepare_fresh_owner_slot(&layout, true, &run_log).expect("replace owner");

    let _ = reaper.join().expect("join owner reaper");
    assert!(!owner.is_running());
    assert_eq!(live_owner(&layout), None);
}

#[test]
fn dead_holders_lease_does_not_block_the_instance_lock() {
    let temp = TempDir::new("instance-lock-stale-reclaim");
    let layout = temp_layout(&temp, "vm");
    let dead = locks::test_support::exited_process();
    locks::test_support::write_instance_lease(
        &layout,
        &locks::test_support::lease_for(LeaseRole::Owner, dead),
    );

    let mut seen_stale = None;
    let lock = InstanceLock::try_acquire(&layout, LeaseRole::Owner, |stale| {
        seen_stale = stale.cloned();
        Ok(())
    })
    .expect("try_acquire should not error");

    assert!(lock.is_some());
    assert_eq!(seen_stale.map(|lease| lease.process), Some(dead));
}

#[test]
fn live_foreign_holder_is_not_reclaimed() {
    let temp = TempDir::new("instance-lock-live");
    let layout = temp_layout(&temp, "vm");
    let mut holder = locks::test_support::spawn_foreign_holder(&layout, LeaseRole::Owner, "exit 0");

    let lock = InstanceLock::try_acquire(&layout, LeaseRole::Owner, |_| Ok(()))
        .expect("try_acquire should not error");

    assert!(lock.is_none());
    assert_eq!(
        live_owner(&layout).map(|lease| lease.process),
        Some(holder.process)
    );
    holder.kill();
}

#[test]
fn a_lock_file_moved_away_with_its_instance_is_not_the_lock_at_its_path() {
    let temp = TempDir::new("lock-file-identity");
    let path = temp.path().join("instance.lock");
    let file = locks::test_support::open_lock(&path);
    assert!(locks::test_support::is_file_at(&file, &path));

    fs::rename(&path, temp.path().join("detached.lock")).expect("detach lock file");
    assert!(!locks::test_support::is_file_at(&file, &path));

    drop(locks::test_support::open_lock(&path));
    assert!(!locks::test_support::is_file_at(&file, &path));
}

#[test]
fn killed_holder_releases_the_instance_lock_and_leaves_its_lease() {
    let temp = TempDir::new("instance-lock-crash-release");
    let layout = temp_layout(&temp, "vm");
    let mut holder = locks::test_support::spawn_foreign_holder(&layout, LeaseRole::Owner, "exit 0");

    holder.kill();

    let state = instance_lock_state(&layout).expect("inspect lock");
    assert_eq!(
        state.stale_lease().map(|lease| lease.process),
        Some(holder.process)
    );
    assert!(!state.is_held());
}

#[test]
fn recycled_pid_is_not_mistaken_for_the_recorded_holder() {
    let temp = TempDir::new("instance-lock-pid-reuse");
    let layout = temp_layout(&temp, "vm");
    // This process is alive, but it did not start at the recorded time: the
    // lease names an earlier process that had the same pid.
    let impostor = ProcessIdentity {
        started: ProcessIdentity::current().started.wrapping_sub(1),
        ..ProcessIdentity::current()
    };
    locks::test_support::write_instance_lease(
        &layout,
        &locks::test_support::lease_for(LeaseRole::Owner, impostor),
    );

    assert!(!impostor.is_running());
    assert_eq!(live_owner(&layout), None);
    let state = instance_lock_state(&layout).expect("inspect lock");
    assert_eq!(
        state.stale_lease().map(|lease| lease.process),
        Some(impostor)
    );
}

#[test]
fn guard_makes_inspect_then_act_atomic_with_acquisition() {
    let temp = TempDir::new("instance-lock-guard");
    let layout = temp_layout(&temp, "vm");
    fs::create_dir_all(&layout.instance_dir).expect("create instance");
    let (inspected_tx, inspected_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel::<()>();
    let inspector_layout = layout.clone();
    let inspector = thread::spawn(move || {
        with_instance_guard(&inspector_layout, |state| {
            inspected_tx.send(state.is_held()).expect("report state");
            finish_rx.recv().expect("wait to finish");
            Ok(())
        })
        .expect("guarded inspection");
    });
    assert!(!inspected_rx.recv().expect("inspected state"));

    let acquirer_layout = layout.clone();
    let acquirer = thread::spawn(move || {
        let lock = locks::test_support::hold_as_owner(&acquirer_layout);
        drop(lock);
        Instant::now()
    });
    // Give a broken guard the chance to let the acquirer in early.
    thread::sleep(Duration::from_millis(50));
    let released_at = Instant::now();
    finish_tx.send(()).expect("finish inspection");
    inspector.join().expect("join inspector");

    assert!(acquirer.join().expect("join acquirer") >= released_at);
}

#[test]
fn concurrent_instance_lock_acquisition_has_single_winner() {
    let temp = TempDir::new("instance-lock-concurrent");
    let layout = temp_layout(&temp, "vm");
    locks::test_support::write_instance_lease(
        &layout,
        &locks::test_support::lease_for(LeaseRole::Owner, locks::test_support::exited_process()),
    );

    const THREADS: usize = 8;
    let barrier = Arc::new(std::sync::Barrier::new(THREADS));
    let winners: Arc<Mutex<Vec<InstanceLock>>> = Arc::new(Mutex::new(Vec::new()));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let layout = layout.clone();
            let barrier = Arc::clone(&barrier);
            let winners = Arc::clone(&winners);
            thread::spawn(move || {
                barrier.wait();
                if let Some(lock) = InstanceLock::try_acquire(&layout, LeaseRole::Owner, |_| Ok(()))
                    .expect("try_acquire should not error")
                {
                    winners.lock().unwrap().push(lock);
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("thread should not panic");
    }

    assert_eq!(winners.lock().unwrap().len(), 1);
}

#[test]
fn concurrent_owner_start_lock_acquisition_has_single_winner() {
    let temp = TempDir::new("owner-start-lock-concurrent");
    let layout = temp_layout(&temp, "vm");

    const THREADS: usize = 8;
    let barrier = Arc::new(std::sync::Barrier::new(THREADS));
    let winners: Arc<Mutex<Vec<OwnerStartLock>>> = Arc::new(Mutex::new(Vec::new()));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let layout = layout.clone();
            let barrier = Arc::clone(&barrier);
            let winners = Arc::clone(&winners);
            thread::spawn(move || {
                barrier.wait();
                if let Some(lock) =
                    OwnerStartLock::try_acquire(&layout).expect("try_acquire should not error")
                {
                    winners.lock().unwrap().push(lock);
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("thread should not panic");
    }

    assert_eq!(winners.lock().unwrap().len(), 1);
}

#[test]
fn released_lock_is_reacquirable_and_carries_no_lease() {
    let temp = TempDir::new("instance-lock-release");
    let layout = temp_layout(&temp, "vm");

    drop(locks::test_support::hold_as_owner(&layout));

    assert_eq!(
        instance_lock_state(&layout).expect("inspect lock"),
        InstanceLockState::Free { stale: None }
    );
    drop(locks::test_support::hold_as_owner(&layout));
}

#[test]
fn failed_validation_releases_the_lock_and_keeps_the_stale_lease() {
    let temp = TempDir::new("instance-lock-validation");
    let layout = temp_layout(&temp, "vm");
    let dead = locks::test_support::exited_process();
    locks::test_support::write_instance_lease(
        &layout,
        &locks::test_support::lease_for(LeaseRole::Owner, dead),
    );

    let error = InstanceLock::try_acquire(&layout, LeaseRole::Owner, |_| bail!("not clean"))
        .expect_err("validation failure propagates");

    assert_eq!(error.to_string(), "not clean");
    let state = instance_lock_state(&layout).expect("inspect lock");
    assert_eq!(state.stale_lease().map(|lease| lease.process), Some(dead));
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
        rootfs: None,
        instance_dir: instance_dir.clone(),
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
        rootfs: None,
        instance_dir: instance_dir.clone(),
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
        rootfs: None,
        instance_dir: instance_dir.clone(),
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

/// A snapshot dir and a current stamp file holding the given stamps.
fn agent_stamps(temp: &TempDir, snapshot: &str, current: &str) -> (PathBuf, PathBuf) {
    let snapshot_dir = temp.path().join("snapshot");
    fs::create_dir_all(&snapshot_dir).expect("create snapshot");
    fs::write(snapshot_dir.join("initramfs.stamp"), snapshot).expect("write snapshot stamp");
    let current_stamp = temp.path().join("current.stamp");
    fs::write(&current_stamp, current).expect("write current stamp");
    (snapshot_dir, current_stamp)
}

#[test]
fn a_changed_agent_speaking_the_same_protocol_can_be_resumed() {
    let temp = TempDir::new("agent-same-protocol");
    let (snapshot, current) = agent_stamps(
        &temp,
        "source=old\nprotocol=11\n",
        "source=new\nprotocol=11\n",
    );

    assert_eq!(snapshot_agent_incompatibility(&snapshot, &current, false), None);
}

#[test]
fn an_agent_speaking_another_protocol_cannot_be_resumed() {
    let temp = TempDir::new("agent-other-protocol");
    let (snapshot, current) = agent_stamps(
        &temp,
        "source=old\nprotocol=10\n",
        "source=new\nprotocol=11\n",
    );

    assert_eq!(
        snapshot_agent_incompatibility(&snapshot, &current, false).as_deref(),
        Some("its guest agent speaks lnx protocol 10, this lnx speaks 11")
    );
}

#[test]
fn deterministic_runs_and_old_stamps_need_the_same_agent() {
    let temp = TempDir::new("agent-exact");
    let (snapshot, current) = agent_stamps(
        &temp,
        "source=old\nprotocol=11\n",
        "source=new\nprotocol=11\n",
    );
    assert_eq!(
        snapshot_agent_incompatibility(&snapshot, &current, true).as_deref(),
        Some("it was taken by a different version of the lnx guest agent")
    );

    let temp = TempDir::new("agent-legacy-stamp");
    let (snapshot, current) = agent_stamps(&temp, "source=old\n", "source=new\nprotocol=11\n");
    assert!(snapshot_agent_incompatibility(&snapshot, &current, false).is_some());
    let (snapshot, current) = agent_stamps(&temp, "source=same\n", "source=same\nprotocol=11\n");
    assert_eq!(snapshot_agent_incompatibility(&snapshot, &current, false), None);
}

fn test_run_config(layout: &Layout, cwd: &Path) -> RunConfig {
    RunConfig {
        layout: layout.clone(),
        command: vec!["true".to_string()],
        cwd: cwd.to_path_buf(),
        cpus: 2,
        memory_mib: 4096,
        nested_kvm: false,
        restore_snapshot: None,
        forwards: Vec::new(),
        exec: ExecOptions::default(),
        no_host_shares: true,
        vhost_user_fs: Vec::new(),
        reuse_owner: true,
        deterministic: None,
        trace_events: false,
    }
}

#[test]
fn an_unresumable_memory_snapshot_is_refused_with_a_non_destructive_remedy() {
    let temp = TempDir::new("snapshot-compatibility");
    let layout = temp_layout(&temp, "compat");
    fs::create_dir_all(&layout.run_dir).expect("create run dir");
    let snapshot = temp.path().join("snapshot");
    write_snapshot_state_files(&snapshot);
    let launch = test_launch_metadata(
        "/Users/test",
        None,
        true,
        test_host_share_cache(true),
        Vec::new(),
    );
    write_launch_metadata(&snapshot.join(LAUNCH_METADATA), &launch).expect("write launch");
    let current = temp.path().join("initramfs.stamp");
    fs::write(snapshot.join("initramfs.stamp"), "source=old\n").expect("write snapshot stamp");
    fs::write(&current, "source=new\n").expect("write current stamp");
    let config = test_run_config(&layout, temp.path());
    let deterministic = deterministic_stamp_content(None);
    let run_log = RunLog::open(&layout).expect("run log");
    let check = || {
        validate_restore_compatibility(
            &snapshot,
            &current,
            &launch,
            &deterministic,
            &config,
            &run_log,
        )
    };

    let message = format!("{:#}", check().expect_err("different agent"));
    assert!(
        message.contains("different version of the lnx guest agent"),
        "{message}"
    );
    assert!(
        message.contains("lnx --instance compat snapshots clear"),
        "{message}"
    );
    assert!(message.contains("boots from the saved disk"), "{message}");

    fs::write(&current, "source=old\n").expect("match agent stamp");
    write_vmstate_header(&snapshot, 4 * 1024 * 1024 * 1024, 4);
    let message = format!("{:#}", check().expect_err("different shape"));
    assert!(
        message.contains("it has 4 CPUs and 4096 MiB of memory, not 2 and 4096"),
        "{message}"
    );

    write_vmstate_header(&snapshot, 4 * 1024 * 1024 * 1024, 2);
    check().expect("compatible snapshot");
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

    let temp = TempDir::new("broker-stale-hello");
    let config = test_run_config(&temp_layout(&temp, "default"), Path::new("/"));
    let err = run_existing_broker_client(&socket, &config, None)
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

    let temp = TempDir::new("broker-missing-hello");
    let config = test_run_config(&temp_layout(&temp, "default"), Path::new("/"));
    let status = run_existing_broker_client(&socket, &config, None)
        .expect("missing hello is transient");
    server.join().expect("broker thread");
    let _ = fs::remove_file(&socket);

    assert_eq!(status, None);
}

#[test]
fn guest_cwd_uses_host_path_under_home() {
    assert_eq!(
        guest_cwd(Path::new("/Users/ramon/src/lnx")),
        "/Users/ramon/src/lnx"
    );
}

#[test]
fn guest_cwd_uses_host_path_outside_home() {
    assert_eq!(guest_cwd(Path::new("/tmp/build")), "/tmp/build");
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

#[test]
fn concurrent_run_log_writers_never_interleave_within_a_line() {
    let temp = TempDir::new("run-log-interleave");
    let layout = temp_layout(&temp, "vm");
    fs::create_dir_all(&layout.run_dir).expect("create run dir");
    // Separate handles stand in for the client and owner processes, which
    // each open the log themselves.
    let writers: Vec<_> = (0..4)
        .map(|writer| {
            let log = RunLog::open(&layout).expect("open run log");
            thread::spawn(move || {
                for line in 0..500 {
                    log.line(format!("writer={writer} line={line} {}", "x".repeat(64)));
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("join writer");
    }

    let content = fs::read_to_string(layout.run_dir.join("lnx.log")).expect("read log");
    let lines: Vec<_> = content.lines().collect();
    assert_eq!(lines.len(), 2000);
    for line in lines {
        let (timestamp, rest) = line.split_once(' ').expect("timestamp");
        assert!(
            timestamp.split_once('.').is_some(),
            "malformed line: {line}"
        );
        assert!(rest.starts_with("writer="), "malformed line: {line}");
        assert!(rest.ends_with(&"x".repeat(64)), "malformed line: {line}");
    }
}
