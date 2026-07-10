use super::*;
use std::sync::{Mutex, MutexGuard};
use tempfile::TempDir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct ChildGuard(std::process::Child);

impl ChildGuard {
    fn new(child: std::process::Child) -> Self {
        Self(child)
    }

    fn id(&self) -> u32 {
        self.0.id()
    }

    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.0.wait()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

async fn wait_for_ready(path: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if path.exists() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

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
fn rejects_path_like_instance_names() {
    assert!(validate_instance_name("ok-name_1.2").is_ok());
    assert!(validate_instance_name("../nope").is_err());
    assert!(validate_instance_name("bad/name").is_err());
    assert!(validate_instance_name("").is_err());
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
    assert_eq!(fs::read(&dest.rootfs).expect("read rootfs"), b"rootfs");
    assert_eq!(fs::read(&dest.kernel).expect("read kernel"), b"kernel");
    assert!(dest.snapshot_dir.join("latest/vmstate.bin").exists());
    assert_eq!(
        descriptor::load(&dest)
            .expect("load descriptor")
            .name
            .as_deref(),
        Some("target")
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn rejects_sparse_bundle_with_incompatible_launch_metadata() {
    let source_base = TempDir::new().expect("source tempdir");
    let dest_base = TempDir::new().expect("dest tempdir");
    let source = test_layout(source_base.path(), "source");
    fs::create_dir_all(source.snapshot_dir.join("latest")).expect("create snapshot");
    fs::write(&source.kernel, b"kernel").expect("write kernel");
    fs::write(&source.rootfs, b"rootfs").expect("write rootfs");
    fs::write(
        source.instance_dir.join("lnx.json"),
        br#"{"name":"source","cpus":3,"memory_mib":3072}"#,
    )
    .expect("write descriptor");
    fs::write(&source.vm_initialized, b"1\n").expect("vm initialized");
    fs::write(source.snapshot_dir.join("latest/vmstate.bin"), b"vmstate").expect("write vmstate");
    fs::write(
        source.snapshot_dir.join("latest/launch.json"),
        br#"{
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
"#,
    )
    .expect("write launch metadata");

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
fn materializes_checkpoint_bundle_for_running_push() {
    let source_base = TempDir::new().expect("source tempdir");
    let bundle_base = TempDir::new().expect("bundle tempdir");
    let source = test_layout(source_base.path(), "source");
    fs::create_dir_all(&source.instance_dir).expect("create source instance");
    fs::write(&source.rootfs, b"stale-rootfs").expect("write source rootfs");
    fs::write(
        source.instance_dir.join("lnx.json"),
        br#"{"name":"source","cpus":1,"memory_mib":512}"#,
    )
    .expect("write descriptor");
    fs::write(
        source.instance_dir.join(".lnx-descriptor-partial"),
        b"partial descriptor",
    )
    .expect("write stale descriptor staging file");
    fs::write(source.instance_dir.join(".lnx-fork-lease"), b"").expect("write stale fork lease");
    let checkpoint = checkpoints::Checkpoint {
        id: "checkpoint-for-push".to_string(),
        name: None,
        created_unix: 123,
        path: source.checkpoint_dir.join("checkpoint-for-push"),
    };
    fs::create_dir_all(&checkpoint.path).expect("create checkpoint");
    fs::write(checkpoint.path.join("rootfs.ext4"), b"checkpoint-rootfs")
        .expect("write checkpoint rootfs");
    let mut vmstate = [0u8; 40];
    vmstate[0..8].copy_from_slice(b"LKRNSS01");
    vmstate[8..12].copy_from_slice(&runner::SNAPSHOT_VMSTATE_VERSION.to_le_bytes());
    vmstate[16..24].copy_from_slice(&(512u64 * 1024 * 1024).to_le_bytes());
    vmstate[32..36].copy_from_slice(&1u32.to_le_bytes());
    fs::write(checkpoint.path.join("vmstate.bin"), vmstate).expect("write checkpoint vmstate");
    fs::write(checkpoint.path.join("pages.img"), b"pages").expect("write checkpoint pages");
    fs::write(
        checkpoint.path.join("initramfs.stamp"),
        b"source=test-agent\n",
    )
    .expect("write checkpoint initramfs stamp");
    fs::write(
        checkpoint.path.join("launch.json"),
        br#"{
  "version": 2,
  "owner_args": [],
  "compatibility": { "host_share_cache": { "dax": false } },
  "shares": {
    "no_host_shares": true,
    "host_home": null,
    "outside_home_cwd": null
  }
}"#,
    )
    .expect("write checkpoint launch metadata");
    checkpoints::write_metadata(&source, &checkpoint).expect("write checkpoint metadata");

    let bundle = checkpoint_bundle_layout(&source, bundle_base.path());
    materialize_checkpoint_bundle(&source, &checkpoint, &bundle).expect("materialize bundle");

    assert_eq!(
        fs::read(&bundle.rootfs).expect("read bundle rootfs"),
        b"checkpoint-rootfs"
    );
    assert_eq!(
        fs::read(bundle.snapshot_dir.join("latest/vmstate.bin")).expect("read bundle vmstate"),
        vmstate
    );
    assert!(!bundle.kernel.exists());
    assert!(bundle.vm_initialized.exists());
    assert_eq!(
        descriptor::load(&bundle)
            .expect("load bundle descriptor")
            .name
            .as_deref(),
        Some("source")
    );
    let bundle_descriptor = descriptor::load(&bundle).expect("load bundle descriptor");
    assert_eq!(bundle_descriptor.cpus, Some(1));
    assert_eq!(bundle_descriptor.memory_mib, Some(512));
}

#[test]
fn stopped_push_refuses_failed_final_snapshot_outcome() {
    let source_base = TempDir::new().expect("source tempdir");
    let source = test_layout(source_base.path(), "source");
    fs::create_dir_all(&source.instance_dir).expect("create source instance");
    fs::write(&source.rootfs, b"newer-canonical-rootfs").expect("write source rootfs");
    runner::write_final_snapshot_outcome(&source, &Err(anyhow::anyhow!("snapshot failed")))
        .expect("write failed outcome");

    let error = prepare_push_source(&source)
        .err()
        .expect("failed snapshot blocks stopped push");

    assert!(error.to_string().contains("final snapshot failed"));
    assert!(error.to_string().contains("snapshots clear"));
}

#[test]
fn push_refuses_live_owner_without_broker() {
    let source_base = TempDir::new().expect("source tempdir");
    let source = test_layout(source_base.path(), "source");
    fs::create_dir_all(&source.instance_dir).expect("create source instance");
    fs::write(&source.rootfs, b"live-rootfs").expect("write source rootfs");
    let owner = runner::BootstrapLock::try_acquire(&source.run_dir.join("bootstrap.lock.d"))
        .expect("acquire owner lock")
        .expect("owner lock");

    let error = runner::request_coherent_checkpoint_awaiting_owner(
        &source,
        &source.checkpoint_dir.join("test"),
        Duration::from_millis(25),
    )
    .expect_err("unavailable live broker blocks push");

    assert!(error.to_string().contains("timed out waiting"));
    drop(owner);
}

#[test]
fn stopped_push_uses_stable_copy_without_runtime_leases() {
    let source_base = TempDir::new().expect("source tempdir");
    let source = test_layout(source_base.path(), "source");
    fs::create_dir_all(&source.instance_dir).expect("create source instance");
    fs::write(&source.rootfs, b"stable-rootfs").expect("write source rootfs");
    fs::write(
        source.instance_dir.join("lnx.json"),
        br#"{"name":"source","cpus":1,"memory_mib":512}"#,
    )
    .expect("write descriptor");
    let deferred_clear = source.snapshot_dir.join(".latest.clear-123-0");
    fs::create_dir_all(&deferred_clear).expect("create deferred clear trash");
    fs::write(deferred_clear.join("discarded-secret"), b"discarded")
        .expect("write deferred clear trash");
    let nested_reserved_name = source
        .instance_dir
        .join("host-share-state/home/upper/project/lnx-agent.sock");
    fs::create_dir_all(nested_reserved_name.parent().unwrap())
        .expect("create nested state directory");
    fs::write(&nested_reserved_name, b"guest-visible-state")
        .expect("write nested reserved-name file");
    let stale_agent_socket = source.run_dir.join("lnx-agent.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&stale_agent_socket)
        .expect("create stale runtime socket");
    let stale_checkpoint_broker = source.run_dir.join("checkpoint-broker.sock");
    let _checkpoint_listener = std::os::unix::net::UnixListener::bind(&stale_checkpoint_broker)
        .expect("create stale checkpoint broker socket");

    let prepared = prepare_push_source(&source).expect("prepare stopped push");

    assert_eq!(
        fs::read(&prepared.layout.rootfs).expect("read stable rootfs copy"),
        b"stable-rootfs"
    );
    assert!(!prepared.layout.run_dir.join("bootstrap.lock.d").exists());
    assert!(
        !prepared
            .layout
            .run_dir
            .join("bootstrap.lock.d.guard")
            .exists()
    );
    assert!(!prepared.layout.run_dir.join("lnx-agent.sock").exists());
    assert!(
        !prepared
            .layout
            .instance_dir
            .join(".lnx-descriptor-partial")
            .exists()
    );
    assert!(
        !prepared
            .layout
            .instance_dir
            .join(".lnx-fork-lease")
            .exists()
    );
    assert!(
        !prepared
            .layout
            .run_dir
            .join("checkpoint-broker.sock")
            .exists()
    );
    assert!(
        !prepared
            .layout
            .snapshot_dir
            .join(".latest.clear-123-0")
            .exists()
    );
    assert_eq!(
        fs::read(
            prepared
                .layout
                .instance_dir
                .join("host-share-state/home/upper/project/lnx-agent.sock")
        )
        .expect("nested reserved-name file is preserved"),
        b"guest-visible-state"
    );
}

#[test]
fn sparse_bundle_round_trips_sparse_rootfs() {
    let source_base = TempDir::new().expect("source tempdir");
    let dest_base = TempDir::new().expect("dest tempdir");
    let source = test_layout(source_base.path(), "source");
    fs::create_dir_all(&source.instance_dir).expect("create source instance");
    fs::write(&source.kernel, b"kernel").expect("write kernel");
    fs::write(
        source.instance_dir.join("lnx.json"),
        br#"{"name":"source","cpus":1,"memory_mib":512}"#,
    )
    .expect("write descriptor");
    fs::write(&source.vm_initialized, b"1\n").expect("vm initialized");

    let mut rootfs = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&source.rootfs)
        .expect("create rootfs");
    rootfs.set_len(64 * 1024 * 1024).expect("sparse rootfs");
    rootfs
        .seek(SeekFrom::Start(48 * 1024 * 1024))
        .expect("seek rootfs");
    rootfs.write_all(b"SPARSE_DATA").expect("write sparse data");
    drop(rootfs);

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
        fs::metadata(&dest.rootfs)
            .expect("stat imported rootfs")
            .len(),
        64 * 1024 * 1024
    );
    let mut imported = fs::File::open(&dest.rootfs).expect("open imported rootfs");
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
    fs::create_dir_all(&source.instance_dir).expect("create source instance");
    fs::write(&source.kernel, b"kernel").expect("write kernel");
    fs::write(
        source.instance_dir.join("lnx.json"),
        br#"{"name":"source","cpus":1,"memory_mib":512}"#,
    )
    .expect("write descriptor");
    fs::write(&source.vm_initialized, b"1\n").expect("vm initialized");
    let mut rootfs = fs::File::create(&source.rootfs).expect("create rootfs");
    let block = vec![0x42; CAS_BLOCK_SIZE as usize];
    rootfs.write_all(&block).expect("write block 1");
    rootfs.write_all(&block).expect("write block 2");
    rootfs.write_all(b"tail").expect("write tail");
    drop(rootfs);

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
        .find(|file| file.path == "instances/source/rootfs.ext4")
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
    fs::create_dir_all(&source.instance_dir).expect("create source instance");
    fs::write(&source.kernel, b"kernel").expect("write kernel");
    fs::write(&source.rootfs, b"rootfs-data").expect("write rootfs");
    fs::write(
        source.instance_dir.join("lnx.json"),
        br#"{"name":"source","cpus":1,"memory_mib":512}"#,
    )
    .expect("write descriptor");
    fs::write(&source.vm_initialized, b"1\n").expect("vm initialized");
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
    assert_eq!(fs::read(&dest.rootfs).expect("read rootfs"), b"rootfs-data");
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

fn test_layout(base: &Path, instance: &str) -> Layout {
    Layout {
        base: base.to_path_buf(),
        instance: instance.to_string(),
        kernel: base.join("vmlinuz"),
        rootfs: base.join("instances").join(instance).join("rootfs.ext4"),
        instance_dir: base.join("instances").join(instance),
        snapshot_dir: base
            .join("instances")
            .join(instance)
            .join("memory-snapshots"),
        checkpoint_dir: base.join("instances").join(instance).join("checkpoints"),
        vm_initialized: base.join("instances").join(instance).join("vm-initialized"),
        run_dir: base.join("instances").join(instance),
        console_log: base.join("instances").join(instance).join("console.log"),
    }
}

#[tokio::test]
async fn stop_reports_final_snapshot_failure_from_recovery_marker() {
    let temp = TempDir::new().expect("tempdir");
    let layout = test_layout(temp.path(), "stop-failed-snapshot");
    let lock = layout.run_dir.join("bootstrap.lock.d");
    fs::create_dir_all(&lock).expect("create owner lock");
    fs::create_dir_all(layout.snapshot_dir.join(runner::RESTORE_WORK_SNAPSHOT))
        .expect("create restore work");
    fs::write(
        layout.snapshot_dir.join(runner::RESTORE_WORK_ACTIVE_MARKER),
        b"generation_id=recovery\n",
    )
    .expect("write recovery marker");
    let ready = layout.run_dir.join("owner-ready");
    let mut owner = ChildGuard::new(
        Command::new("/bin/sh")
        .arg("-c")
        .arg(
            "trap 'rm -rf \"$LOCK_PATH\"; exit 1' TERM; touch \"$READY_PATH\"; while :; do sleep 1; done",
        )
        .env("LOCK_PATH", &lock)
        .env("READY_PATH", &ready)
        .spawn()
        .expect("spawn fake owner"),
    );
    fs::write(lock.join("owner.pid"), owner.id().to_string()).expect("write owner pid");
    wait_for_ready(&ready).await;

    let error = stop_existing_instance_with_timeout(&layout, Duration::from_secs(2))
        .await
        .expect_err("recovery marker reports failed shutdown snapshot");

    assert!(
        error
            .to_string()
            .contains("without publishing its final snapshot")
    );
    assert!(error.to_string().contains("snapshots clear"));
    assert!(runner::restore_work_is_active(&layout));
    let _ = owner.wait();
}

#[tokio::test]
async fn stop_reports_missing_fresh_owner_snapshot_outcome() {
    let temp = TempDir::new().expect("tempdir");
    let layout = test_layout(temp.path(), "stop-missing-outcome");
    let lock = layout.run_dir.join("bootstrap.lock.d");
    fs::create_dir_all(&lock).expect("create owner lock");
    let ready = layout.run_dir.join("owner-ready");
    let mut owner = ChildGuard::new(
        Command::new("/bin/sh")
        .arg("-c")
        .arg(
            "trap 'rm -rf \"$LOCK_PATH\"; exit 1' TERM; touch \"$READY_PATH\"; while :; do sleep 1; done",
        )
        .env("LOCK_PATH", &lock)
        .env("READY_PATH", &ready)
        .spawn()
        .expect("spawn fake owner"),
    );
    fs::write(lock.join("owner.pid"), owner.id().to_string()).expect("write owner pid");
    wait_for_ready(&ready).await;

    let error = stop_existing_instance_with_timeout(&layout, Duration::from_secs(2))
        .await
        .expect_err("missing fresh-owner outcome is a stop failure");

    assert!(error.to_string().contains("without reporting whether"));
    let _ = owner.wait();
    let persisted = runner::read_final_snapshot_outcome(&layout)
        .expect("read persisted failure")
        .expect("missing outcome is replaced by a failure tombstone");
    assert_eq!(persisted.pid, owner.id());
    assert!(!persisted.succeeded);
    let retry = runner::validate_restore_work_for_command(&layout)
        .expect_err("the next start remains blocked");
    assert!(retry.to_string().contains("final snapshot failed"));
}

#[tokio::test]
async fn stop_reports_missing_snapshot_outcome_after_owner_crash() {
    let temp = TempDir::new().expect("tempdir");
    let layout = test_layout(temp.path(), "stop-crashed-owner");
    let lock = layout.run_dir.join("bootstrap.lock.d");
    fs::create_dir_all(&lock).expect("create stale owner lock");
    let mut exited = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("spawn short-lived owner");
    let stale_pid = exited.id();
    exited.wait().expect("reap short-lived owner");
    fs::write(lock.join("owner.pid"), stale_pid.to_string()).expect("write stale owner pid");

    let error = stop_existing_instance_with_timeout(&layout, Duration::from_millis(50))
        .await
        .expect_err("a crashed owner without an outcome is a stop failure");

    assert!(error.to_string().contains("without reporting whether"));
    assert!(error.to_string().contains(&stale_pid.to_string()));
    assert!(
        lock.exists(),
        "failed verification must retain PID evidence"
    );
    let retry = runner::validate_restore_work_for_command(&layout)
        .expect_err("a later start must remain blocked");
    assert!(retry.to_string().contains("final snapshot failed"));
    let persisted = runner::read_final_snapshot_outcome(&layout)
        .expect("read persisted crash outcome")
        .expect("crash outcome is persisted");
    assert_eq!(persisted.pid, stale_pid);
    assert!(!persisted.succeeded);
}

#[tokio::test]
async fn stop_timeout_leaves_unresponsive_owner_running() {
    let temp = TempDir::new().expect("tempdir");
    let layout = test_layout(temp.path(), "stop-timeout");
    let lock = layout.run_dir.join("bootstrap.lock.d");
    fs::create_dir_all(&lock).expect("create owner lock");
    let ready = layout.run_dir.join("owner-ready");
    let mut owner = ChildGuard::new(
        Command::new("/bin/sh")
            .arg("-c")
            .arg("trap '' TERM; touch \"$READY_PATH\"; while :; do sleep 1; done")
            .env("READY_PATH", &ready)
            .spawn()
            .expect("spawn fake owner"),
    );
    fs::write(lock.join("owner.pid"), owner.id().to_string()).expect("write owner pid");
    wait_for_ready(&ready).await;

    let error = stop_existing_instance_with_timeout(&layout, Duration::from_millis(50))
        .await
        .expect_err("unresponsive owner times out");

    assert!(error.to_string().contains("left running"));
    assert!(process_alive(owner.id() as i32));
    unsafe { libc::kill(owner.id() as i32, libc::SIGKILL) };
    let _ = owner.wait();
}
