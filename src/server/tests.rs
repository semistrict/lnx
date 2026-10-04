use super::*;
use crate::store::{self, Store};
use std::sync::{Mutex, MutexGuard};
use tempfile::TempDir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct LnxBaseGuard {
    _guard: MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
}

impl LnxBaseGuard {
    fn set(path: &Path) -> Self {
        let guard = ENV_LOCK.lock().expect("env lock");
        let previous = std::env::var_os("LNX_BASE");
        unsafe {
            std::env::set_var("LNX_BASE", path);
        }
        Self {
            _guard: guard,
            previous,
        }
    }
}

impl Drop for LnxBaseGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var("LNX_BASE", previous);
            } else {
                std::env::remove_var("LNX_BASE");
            }
        }
    }
}

#[test]
fn server_instance_listing_keeps_valid_dot_names_and_hides_transactions() {
    let temp = tempfile::tempdir().expect("tempdir");
    let instances = temp.path().join("instances");
    fs::create_dir_all(instances.join(".dev")).expect("create dot instance");
    fs::create_dir_all(instances.join("@legacy-name")).expect("create legacy invalid name");
    let transaction_root = crate::paths::ensure_instance_transaction_root(&instances)
        .expect("create transaction root");
    fs::create_dir_all(transaction_root.join("delete/dev/1-0")).expect("create delete transaction");
    let mut names = BTreeSet::new();

    collect_child_dir_names(&instances, &mut names).expect("collect instances");

    assert_eq!(
        names,
        BTreeSet::from([".dev".to_string(), "@legacy-name".to_string()])
    );
}

#[test]
fn builds_sandbox_url() {
    let url = sandbox_url("http://127.0.0.1:7777/base", "remote").expect("url");
    assert_eq!(url.as_str(), "http://127.0.0.1:7777/v1/sandboxes/remote");
}

#[test]
fn imports_bundle_into_target_layout() {
    let source = TempDir::new().expect("source tempdir");
    let dest_base = TempDir::new().expect("dest tempdir");
    fs::create_dir_all(
        source
            .path()
            .join("instances/source/memory-snapshots/latest"),
    )
    .expect("create source dirs");
    fs::write(source.path().join("vmlinuz"), b"kernel").expect("kernel");
    fs::write(
        source.path().join("instances/source/rootfs.ext4"),
        b"rootfs",
    )
    .expect("rootfs");
    fs::write(
        source
            .path()
            .join("instances/source/memory-snapshots/latest/vmstate.bin"),
        b"vmstate",
    )
    .expect("vmstate");
    fs::write(
        source
            .path()
            .join("instances/source/memory-snapshots/latest/launch.json"),
        br#"{
  "version": 2,
  "owner_args": [],
  "compatibility": {
    "host_share_cache": {
      "dax": true
    }
  },
  "shares": {
    "no_host_shares": true,
    "host_home": null,
    "outside_home_cwd": null
  }
}
"#,
    )
    .expect("launch metadata");
    fs::write(
        source.path().join("instances/source/lnx.json"),
        br#"{"name":"source"}"#,
    )
    .expect("descriptor");
    fs::write(
        source.path().join("instances/source/vm-initialized"),
        b"1\n",
    )
    .expect("vm init");

    let archive = tempfile::NamedTempFile::new().expect("archive");
    let status = Command::new("tar")
        .arg("-C")
        .arg(source.path())
        .arg("-cf")
        .arg(archive.path())
        .arg("vmlinuz")
        .arg("instances/source")
        .status()
        .expect("tar");
    assert!(status.success());

    let dest = test_layout(dest_base.path(), "target");
    let response = import_archive_to_layout(
        archive.path(),
        &dest,
        "target",
        ImportOptions {
            source_instance: "source".to_string(),
            replace: false,
            start: false,
            idle_ttl_ms: None,
            command: Vec::new(),
        },
        AppState {
            cpus: 2,
            memory_mib: 1024,
            nested_kvm: false,
            no_host_shares: true,
        },
    )
    .expect("import");

    assert!(response.ok);
    assert_eq!(
        fs::read(dest.instance_dir.join(store::ROOTFS)).expect("read rootfs"),
        b"rootfs"
    );
    assert_eq!(fs::read(&dest.kernel).expect("read kernel"), b"kernel");
    assert!(
        dest.instance_dir
            .join("memory-snapshots/latest/vmstate.bin")
            .exists()
    );
    // A bundle from an older lnx lands in the old layout and moves into the
    // store on first use; its incomplete memory snapshot is not kept.
    runner::ensure_store(&dest).expect("migrate imported bundle");
    assert_eq!(crate::store::test_support::latest_disk(&dest), b"rootfs");
    assert_eq!(
        descriptor::load(&dest)
            .expect("load descriptor")
            .name
            .as_deref(),
        Some("target")
    );
}

#[cfg(not(target_os = "macos"))]
const INCOMPATIBLE_LAUNCH_METADATA: &[u8] = br#"{
  "version": 2,
  "owner_args": [],
  "compatibility": {
    "host_share_cache": {
      "dax": true
    }
  },
  "shares": {
    "no_host_shares": false,
    "host_home": "/different",
    "outside_home_cwd": null
  }
}
"#;

#[cfg(not(target_os = "macos"))]
#[test]
fn rejects_sparse_bundle_with_incompatible_launch_metadata() {
    let source_base = TempDir::new().expect("source tempdir");
    let dest_base = TempDir::new().expect("dest tempdir");
    let source = test_layout(source_base.path(), "source");
    stored_instance(&source, |generation| {
        fs::write(generation.join(store::ROOTFS), b"rootfs").expect("write rootfs");
        fs::write(generation.join(store::VMSTATE), b"vmstate").expect("write vmstate");
        fs::write(generation.join(store::PAGES), b"pages").expect("write pages");
        fs::write(generation.join("launch.json"), INCOMPATIBLE_LAUNCH_METADATA)
            .expect("write launch metadata");
    });
    fs::write(&source.kernel, b"kernel").expect("write kernel");
    fs::write(
        source.instance_dir.join("lnx.json"),
        br#"{"name":"source","cpus":3,"memory_mib":3072}"#,
    )
    .expect("write descriptor");

    let bundle = SparseBundle::open(&source).expect("open sparse bundle");
    let bundle_file = tempfile::NamedTempFile::new().expect("bundle file");
    let mut reader = bundle.reader;
    let mut writer = fs::File::create(bundle_file.path()).expect("create bundle file");
    std::io::copy(&mut reader, &mut writer).expect("write bundle");
    drop(writer);

    let dest = test_layout(dest_base.path(), "target");
    let error = import_sparse_bundle_to_layout(
        bundle_file.path(),
        &dest,
        "target",
        ImportOptions {
            source_instance: "source".to_string(),
            replace: true,
            start: false,
            idle_ttl_ms: None,
            command: Vec::new(),
        },
        AppState {
            cpus: 2,
            memory_mib: 1024,
            nested_kvm: false,
            no_host_shares: true,
        },
    )
    .expect_err("incompatible snapshot should be rejected");

    assert!(
        error
            .to_string()
            .contains("snapshot cannot be restored on this server")
    );
    assert!(!dest.instance_dir.exists());
}

#[test]
fn a_push_bundle_from_a_checkpoint_carries_its_memory_and_vm_shape() {
    let source_base = TempDir::new().expect("source tempdir");
    let bundle_base = TempDir::new().expect("bundle tempdir");
    let source = test_layout(source_base.path(), "source");
    fs::create_dir_all(&source.instance_dir).expect("create source instance");
    fs::write(
        source.instance_dir.join("lnx.json"),
        br#"{"name":"source","cpus":2,"memory_mib":1024}"#,
    )
    .expect("write descriptor");
    let lock = runner::test_support::hold_as_owner(&source);
    let store = Store::new(&source.instance_dir);
    let staging = store.stage(&lock).expect("stage checkpoint");
    fs::write(staging.dir().join(store::ROOTFS), b"checkpoint-rootfs").expect("write rootfs");
    let mut vmstate = [0u8; 40];
    vmstate[0..8].copy_from_slice(b"LKRNSS01");
    vmstate[8..12].copy_from_slice(&runner::SNAPSHOT_VMSTATE_VERSION.to_le_bytes());
    vmstate[16..24].copy_from_slice(&(512u64 * 1024 * 1024).to_le_bytes());
    vmstate[32..36].copy_from_slice(&1u32.to_le_bytes());
    fs::write(staging.dir().join(store::VMSTATE), vmstate).expect("write vmstate");
    fs::write(staging.dir().join(store::PAGES), b"pages").expect("write pages");
    let generation = store.initialize(&lock, staging).expect("initialize source");
    store
        .add_checkpoint(
            &lock,
            &crate::store::CheckpointRef {
                id: "checkpoint-for-push".to_string(),
                name: None,
                generation,
                created_unix: 123,
            },
        )
        .expect("add checkpoint");
    drop(lock);
    let checkpoint = checkpoints::resolve(&source, "checkpoint-for-push").expect("resolve");

    let bundle = checkpoint_bundle_layout(&source, bundle_base.path());
    checkpoints::fork(
        &source,
        checkpoints::ForkSource::Checkpoint(&checkpoint),
        &bundle,
    )
    .expect("materialize bundle");

    let latest = Store::new(&bundle.instance_dir)
        .latest()
        .expect("read bundle")
        .expect("bundle latest");
    assert_eq!(fs::read(latest.rootfs()).unwrap(), b"checkpoint-rootfs");
    assert_eq!(fs::read(latest.dir.join(store::VMSTATE)).unwrap(), vmstate);
    assert!(!bundle.kernel.exists());
    let bundle_descriptor = descriptor::load(&bundle).expect("load bundle descriptor");
    assert_eq!(bundle_descriptor.name.as_deref(), Some("source"));
    assert_eq!(bundle_descriptor.cpus, Some(1));
    assert_eq!(bundle_descriptor.memory_mib, Some(512));
}

#[test]
fn pushing_an_instance_with_a_crashed_vm_asks_for_recovery_first() {
    let source_base = TempDir::new().expect("source tempdir");
    let source = test_layout(source_base.path(), "source");
    crate::store::test_support::initialized(&source, b"saved disk", true);
    crate::store::test_support::crash_after_serving(&source, b"acknowledged writes");

    let error = prepare_push_source(&source)
        .err()
        .expect("crashed run blocks push");

    let message = format!("{error:#}");
    assert!(message.contains("recover --keep"), "{message}");
}

#[test]
fn push_refuses_live_owner_without_broker() {
    let source_base = TempDir::new().expect("source tempdir");
    let source = test_layout(source_base.path(), "source");
    crate::store::test_support::initialized(&source, b"live-rootfs", false);
    let owner = runner::test_support::hold_as_owner(&source);

    let error = runner::request_live_checkpoint(
        &source,
        &runner::CheckpointSpec::default(),
        None,
        Duration::from_millis(25),
    )
    .expect_err("unavailable live broker blocks push");

    assert!(error.to_string().contains("timed out waiting"));
    drop(owner);
}

#[test]
fn stopped_push_copies_only_the_saved_state() {
    let source_base = TempDir::new().expect("source tempdir");
    let source = test_layout(source_base.path(), "source");
    fs::create_dir_all(&source.instance_dir).expect("create source instance");
    fs::write(
        source.instance_dir.join("lnx.json"),
        br#"{"name":"source","cpus":1,"memory_mib":512}"#,
    )
    .expect("write descriptor");
    {
        let lock = runner::test_support::hold_as_owner(&source);
        let store = Store::new(&source.instance_dir);
        let staging = store.stage(&lock).expect("stage");
        fs::write(staging.dir().join(store::ROOTFS), b"stable-rootfs").expect("write rootfs");
        let nested = staging
            .dir()
            .join("host-share-state/home/upper/project/lnx-agent.sock");
        fs::create_dir_all(nested.parent().unwrap()).expect("create nested state directory");
        fs::write(&nested, b"guest-visible-state").expect("write nested reserved-name file");
        store.initialize(&lock, staging).expect("initialize source");
    }
    fs::write(
        source.instance_dir.join(".lnx-descriptor-partial"),
        b"partial descriptor",
    )
    .expect("write stale descriptor staging file");
    let stale_agent_socket = source.run_dir.join("lnx-agent.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&stale_agent_socket)
        .expect("create stale runtime socket");

    let prepared = prepare_push_source(&source).expect("prepare stopped push");

    let latest = Store::new(&prepared.layout.instance_dir)
        .latest()
        .expect("read bundle")
        .expect("bundle latest");
    assert_eq!(fs::read(latest.rootfs()).unwrap(), b"stable-rootfs");
    assert_eq!(
        fs::read(
            latest
                .dir
                .join("host-share-state/home/upper/project/lnx-agent.sock")
        )
        .expect("nested reserved-name file is preserved"),
        b"guest-visible-state"
    );
    for leftover in [".lnx-descriptor-partial", "lnx-agent.sock", "runs"] {
        assert!(
            !prepared.layout.instance_dir.join(leftover).exists(),
            "{leftover}"
        );
    }
    for lock in runner::LOCK_FILES {
        assert!(!prepared.layout.instance_dir.join(lock).exists(), "{lock}");
    }
}

#[test]
fn sparse_bundle_round_trips_sparse_rootfs() {
    let source_base = TempDir::new().expect("source tempdir");
    let dest_base = TempDir::new().expect("dest tempdir");
    let source = test_layout(source_base.path(), "source");
    stored_instance(&source, |generation| {
        let mut rootfs = fs::File::create(generation.join(store::ROOTFS)).expect("create rootfs");
        rootfs.set_len(64 * 1024 * 1024).expect("sparse rootfs");
        rootfs
            .seek(SeekFrom::Start(48 * 1024 * 1024))
            .expect("seek rootfs");
        rootfs.write_all(b"SPARSE_DATA").expect("write sparse data");
    });
    fs::write(&source.kernel, b"kernel").expect("write kernel");
    fs::write(
        source.instance_dir.join("lnx.json"),
        br#"{"name":"source","cpus":1,"memory_mib":512}"#,
    )
    .expect("write descriptor");

    let bundle = SparseBundle::open(&source).expect("open sparse bundle");
    assert!(
        bundle.total_len < 32 * 1024 * 1024,
        "bundle should send extents, not the full sparse rootfs: {}",
        bundle.total_len
    );
    let bundle_file = tempfile::NamedTempFile::new().expect("bundle file");
    let mut reader = bundle.reader;
    let mut writer = fs::File::create(bundle_file.path()).expect("create bundle file");
    std::io::copy(&mut reader, &mut writer).expect("write bundle");
    drop(writer);

    let dest = test_layout(dest_base.path(), "target");
    let response = import_sparse_bundle_to_layout(
        bundle_file.path(),
        &dest,
        "target",
        ImportOptions {
            source_instance: "source".to_string(),
            replace: false,
            start: false,
            idle_ttl_ms: None,
            command: Vec::new(),
        },
        AppState {
            cpus: 2,
            memory_mib: 1024,
            nested_kvm: false,
            no_host_shares: false,
        },
    )
    .expect("import sparse bundle");

    assert!(response.ok);
    assert_eq!(
        fs::metadata(latest_rootfs(&dest))
            .expect("stat imported rootfs")
            .len(),
        64 * 1024 * 1024
    );
    let mut imported = fs::File::open(latest_rootfs(&dest)).expect("open imported rootfs");
    imported
        .seek(SeekFrom::Start(48 * 1024 * 1024))
        .expect("seek imported rootfs");
    let mut marker = vec![0u8; "SPARSE_DATA".len()];
    imported.read_exact(&mut marker).expect("read marker");
    assert_eq!(marker, b"SPARSE_DATA");
    assert_eq!(fs::read(&dest.kernel).expect("read kernel"), b"kernel");
    assert_eq!(
        descriptor::load(&dest)
            .expect("load descriptor")
            .name
            .as_deref(),
        Some("target")
    );
}

#[test]
fn formats_upload_progress_bytes() {
    assert_eq!(human_bytes(0), "0 B");
    assert_eq!(human_bytes(1023), "1023 B");
    assert_eq!(human_bytes(1024), "1.00 KiB");
    assert_eq!(human_bytes(10 * 1024 * 1024), "10.0 MiB");
    assert_eq!(upload_progress_frame(Duration::from_millis(0)).len(), 20);
}

#[test]
fn cas_manifest_splits_and_deduplicates_blocks() {
    let source_base = TempDir::new().expect("source tempdir");
    let source = test_layout(source_base.path(), "source");
    stored_instance(&source, |generation| {
        let mut rootfs = fs::File::create(generation.join(store::ROOTFS)).expect("create rootfs");
        let block = vec![0x42; CAS_BLOCK_SIZE as usize];
        rootfs.write_all(&block).expect("write block 1");
        rootfs.write_all(&block).expect("write block 2");
        rootfs.write_all(b"tail").expect("write tail");
    });
    fs::write(&source.kernel, b"kernel").expect("write kernel");
    fs::write(
        source.instance_dir.join("lnx.json"),
        br#"{"name":"source","cpus":1,"memory_mib":512}"#,
    )
    .expect("write descriptor");
    let rootfs_path = format!(
        "instances/source/{}",
        latest_rootfs(&source)
            .strip_prefix(&source.instance_dir)
            .expect("rootfs under instance")
            .display()
    );

    let bundle = CasPushBundle::open(
        &source,
        &PushConfig {
            source: source.clone(),
            url: "http://127.0.0.1:7777".to_string(),
            target_instance: "target".to_string(),
            replace: true,
            start: false,
            idle_ttl_ms: None,
            command: Vec::new(),
        },
    )
    .expect("open CAS bundle");
    let rootfs_file = bundle
        .manifest
        .files
        .iter()
        .find(|file| file.path == rootfs_path)
        .expect("rootfs file");
    assert_eq!(rootfs_file.blocks.len(), 3);
    assert_eq!(rootfs_file.blocks[0].sha256, rootfs_file.blocks[1].sha256);
    assert_eq!(rootfs_file.blocks[2].len, 4);
    assert!(
        bundle.blocks.len() < rootfs_file.blocks.len() + bundle.manifest.files.len(),
        "repeated blocks should be stored once in the upload map"
    );
}

#[test]
fn cas_upload_negotiation_requests_only_missing_blocks() {
    let base = TempDir::new().expect("base tempdir");
    let _env = LnxBaseGuard::set(base.path());
    let known = sha256_hex(b"known");
    let missing = sha256_hex(b"missing");
    store_cas_block(&known, b"known").expect("store known block");
    let manifest = CasUploadManifest {
        version: 2,
        source_instance: "source".to_string(),
        replace: true,
        start: false,
        idle_ttl_ms: None,
        command: Vec::new(),
        files: vec![CasManifestFile {
            path: "instances/source/rootfs.ext4".to_string(),
            len: 12,
            mode: 0o644,
            blocks: vec![
                CasManifestBlock {
                    offset: 0,
                    len: 5,
                    sha256: known,
                },
                CasManifestBlock {
                    offset: 5,
                    len: 7,
                    sha256: missing.clone(),
                },
            ],
        }],
    };

    let response = start_cas_upload_blocking(
        "target",
        manifest,
        AppState {
            cpus: 1,
            memory_mib: 512,
            nested_kvm: false,
            no_host_shares: true,
        },
    )
    .expect("start CAS upload");

    assert_eq!(response.known_blocks, 1);
    assert_eq!(response.missing, vec![missing]);
    assert_eq!(response.missing_bytes, 7);
}

#[test]
fn cas_block_stream_stores_multiple_blocks() {
    let base = TempDir::new().expect("base tempdir");
    let _env = LnxBaseGuard::set(base.path());
    let first = b"first-block";
    let second = b"second-block";
    let first_hash = sha256_hex(first);
    let second_hash = sha256_hex(second);
    let mut encoded = Vec::new();
    write_cas_block_frame(&mut encoded, &first_hash, first).expect("first frame");
    write_cas_block_frame(&mut encoded, &second_hash, second).expect("second frame");

    let count = store_cas_block_stream(&mut Cursor::new(encoded)).expect("store stream");

    assert_eq!(count, 2);
    assert_eq!(
        fs::read(cas_block_path(&first_hash).expect("first path")).expect("first"),
        first
    );
    assert_eq!(
        fs::read(cas_block_path(&second_hash).expect("second path")).expect("second"),
        second
    );
}

#[test]
fn cas_commit_reconstructs_imported_instance() {
    let source_base = TempDir::new().expect("source tempdir");
    let server_base = TempDir::new().expect("server tempdir");
    let _env = LnxBaseGuard::set(server_base.path());
    let source = test_layout(source_base.path(), "source");
    stored_instance(&source, |generation| {
        fs::write(generation.join(store::ROOTFS), b"rootfs-data").expect("write rootfs");
    });
    fs::write(&source.kernel, b"kernel").expect("write kernel");
    fs::write(
        source.instance_dir.join("lnx.json"),
        br#"{"name":"source","cpus":1,"memory_mib":512}"#,
    )
    .expect("write descriptor");
    let config = PushConfig {
        source: source.clone(),
        url: "http://127.0.0.1:7777".to_string(),
        target_instance: "target".to_string(),
        replace: true,
        start: false,
        idle_ttl_ms: None,
        command: Vec::new(),
    };
    let bundle = CasPushBundle::open(&source, &config).expect("open CAS bundle");
    let start = start_cas_upload_blocking(
        "target",
        bundle.manifest.clone(),
        AppState {
            cpus: 1,
            memory_mib: 512,
            nested_kvm: false,
            no_host_shares: true,
        },
    )
    .expect("start CAS upload");
    for hash in &start.missing {
        let block = bundle.blocks.get(hash).expect("local block");
        store_cas_block(hash, &block.read().expect("read block")).expect("store block");
    }

    let response = commit_cas_upload_blocking(
        &start.session,
        AppState {
            cpus: 1,
            memory_mib: 512,
            nested_kvm: false,
            no_host_shares: true,
        },
    )
    .expect("commit CAS upload");

    let dest = test_layout(server_base.path(), "target");
    assert!(response.ok);
    assert_eq!(fs::read(latest_rootfs(&dest)).expect("read rootfs"), b"rootfs-data");
    assert_eq!(
        descriptor::load(&dest)
            .expect("load descriptor")
            .name
            .as_deref(),
        Some("target")
    );
    assert!(
        !cas_session_dir(&start.session)
            .expect("session dir")
            .exists()
    );
}

/// Creates `layout` as a stopped instance whose first generation holds the
/// files `write` puts in the directory it is given.
fn stored_instance(layout: &Layout, write: impl FnOnce(&Path)) {
    fs::create_dir_all(&layout.instance_dir).expect("create instance");
    let lock = runner::test_support::hold_as_owner(layout);
    let store = Store::new(&layout.instance_dir);
    let staging = store.stage(&lock).expect("stage generation");
    write(staging.dir());
    store.initialize(&lock, staging).expect("initialize instance");
}

fn latest_rootfs(layout: &Layout) -> PathBuf {
    Store::new(&layout.instance_dir)
        .latest()
        .expect("read store")
        .expect("latest generation")
        .rootfs()
}

fn test_layout(base: &Path, instance: &str) -> Layout {
    Layout {
        base: base.to_path_buf(),
        instance: instance.to_string(),
        kernel: base.join("vmlinuz"),
        rootfs: None,
        instance_dir: base.join("instances").join(instance),
        run_dir: base.join("instances").join(instance),
        console_log: base.join("instances").join(instance).join("console.log"),
    }
}

#[tokio::test]
async fn stop_reports_a_vm_that_died_after_serving_commands() {
    let temp = TempDir::new().expect("tempdir");
    let layout = test_layout(temp.path(), "stop-crashed-run");
    crate::store::test_support::initialized(&layout, b"saved disk", true);
    crate::store::test_support::crash_after_serving(&layout, b"acknowledged writes");
    let mut owner =
        runner::test_support::spawn_foreign_holder(&layout, runner::LeaseRole::Owner, "exit 1");

    let error = stop_existing_instance_with_timeout(&layout, Duration::from_secs(2))
        .await
        .expect_err("a crashed run is reported");

    let message = format!("{error:#}");
    assert!(message.contains("stopped unexpectedly"), "{message}");
    assert!(message.contains("recover --keep"), "{message}");
    let _ = owner.wait();
}

#[tokio::test]
async fn stopping_an_owner_that_never_served_a_command_leaves_a_usable_instance() {
    let temp = TempDir::new().expect("tempdir");
    let layout = test_layout(temp.path(), "stop-idle-run");
    crate::store::test_support::initialized(&layout, b"saved disk", true);
    crate::store::test_support::crash_before_serving(&layout);
    let mut owner =
        runner::test_support::spawn_foreign_holder(&layout, runner::LeaseRole::Owner, "exit 1");

    stop_existing_instance_with_timeout(&layout, Duration::from_secs(2))
        .await
        .expect("an idle run needs no recovery");

    let _ = owner.wait();
    runner::refuse_crashed_run(&layout).expect("the next start is not blocked");
}

#[tokio::test]
async fn stop_reports_an_owner_that_already_crashed_after_serving() {
    let temp = TempDir::new().expect("tempdir");
    let layout = test_layout(temp.path(), "stop-crashed-owner");
    crate::store::test_support::initialized(&layout, b"saved disk", true);
    crate::store::test_support::crash_after_serving(&layout, b"acknowledged writes");
    let crashed = runner::test_support::exited_process();
    runner::test_support::write_instance_lease(
        &layout,
        &runner::test_support::lease_for(runner::LeaseRole::Owner, crashed),
    );

    let error = stop_existing_instance_with_timeout(&layout, Duration::from_millis(50))
        .await
        .expect_err("a crashed run is reported");

    assert!(format!("{error:#}").contains("recover --keep"));
    assert_eq!(
        runner::instance_lock_state(&layout)
            .expect("inspect lock")
            .stale_lease()
            .map(|lease| lease.process),
        Some(crashed),
        "the crashed owner's lease is kept as evidence"
    );
}

#[tokio::test]
async fn stop_timeout_leaves_unresponsive_owner_running() {
    let temp = TempDir::new().expect("tempdir");
    let layout = test_layout(temp.path(), "stop-timeout");
    let mut owner =
        runner::test_support::spawn_foreign_holder(&layout, runner::LeaseRole::Owner, "1");
    let owner_process = owner.process;

    let error = stop_existing_instance_with_timeout(&layout, Duration::from_millis(50))
        .await
        .expect_err("unresponsive owner times out");

    assert!(error.to_string().contains("left running"));
    assert!(owner_process.is_running());
    owner.kill();
}
