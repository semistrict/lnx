use super::*;
use std::fs;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("lnx-paths-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn resolve_builds_per_instance_paths() {
    let home = PathBuf::from("/Users/test");
    let layout = Layout::resolve_with_env("dev", None, None, None, None, home.clone());

    assert_eq!(layout.base, home.join(".lnx"));
    assert_eq!(layout.instance, "dev");
    assert_eq!(layout.kernel, home.join(".lnx").join("vmlinuz"));
    assert_eq!(
        layout.rootfs,
        home.join(".lnx")
            .join("instances")
            .join("dev")
            .join("rootfs.ext4")
    );
    assert_eq!(
        layout.snapshot_dir,
        home.join(".lnx")
            .join("instances")
            .join("dev")
            .join("memory-snapshots")
    );
    assert_eq!(
        layout.checkpoint_dir,
        home.join(".lnx")
            .join("instances")
            .join("dev")
            .join("checkpoints")
    );
    assert_eq!(
        layout.vm_initialized,
        home.join(".lnx")
            .join("instances")
            .join("dev")
            .join("vm-initialized")
    );
    assert_eq!(
        layout.run_dir,
        home.join(".lnx").join("instances").join("dev")
    );
    assert_eq!(layout.console_log, layout.run_dir.join("console.log"));
}

#[test]
fn resolve_honors_explicit_kernel_and_rootfs() {
    let kernel = PathBuf::from("/tmp/lnx-test-kernel");
    let rootfs = PathBuf::from("/tmp/lnx-test-rootfs.ext4");

    let layout = Layout::resolve_with_env(
        "custom",
        Some(kernel.clone()),
        Some(rootfs.clone()),
        None,
        None,
        PathBuf::from("/Users/test"),
    );

    assert_eq!(layout.kernel, kernel);
    assert_eq!(layout.rootfs, rootfs);
    assert_eq!(layout.instance, "custom");
}

#[test]
fn resolve_honors_lnx_base_env() {
    let base = std::env::temp_dir().join(format!("lnx-base-test-{}", std::process::id()));
    let layout = Layout::resolve_with_env(
        "envbase",
        None,
        None,
        Some(base.clone()),
        None,
        PathBuf::from("/Users/test"),
    );

    assert_eq!(layout.base, base);
    assert_eq!(layout.kernel, layout.base.join("vmlinuz"));
    assert_eq!(
        layout.rootfs,
        layout
            .base
            .join("instances")
            .join("envbase")
            .join("rootfs.ext4")
    );
}

#[test]
fn resolve_honors_lnx_run_base_env() {
    let base = std::env::temp_dir().join(format!("lnx-base-test-{}", std::process::id()));
    let run_base = std::env::temp_dir().join(format!("lnx-run-base-test-{}", std::process::id()));
    let layout = Layout::resolve_with_env(
        "dev",
        None,
        None,
        Some(base.clone()),
        Some(run_base.clone()),
        PathBuf::from("/Users/test"),
    );

    assert_eq!(layout.instance_dir, base.join("instances/dev"));
    assert_eq!(
        layout.snapshot_dir,
        base.join("instances/dev/memory-snapshots")
    );
    assert_eq!(layout.run_dir, run_base.join("instances/dev"));
    assert_eq!(layout.console_log, layout.run_dir.join("console.log"));
}

#[test]
fn resolve_prefers_nearest_ancestor_with_requested_instance() {
    let temp = TempDir::new("ancestor");
    let home = temp.path.join("home");
    let project = home.join("work/project");
    let nested = project.join("src/module");
    fs::create_dir_all(project.join(".lnx").join("instances").join("dev"))
        .expect("create project instance");
    fs::create_dir_all(&nested).expect("create nested cwd");

    let layout =
        Layout::resolve_with_env_and_cwd("dev", None, None, None, None, home.clone(), nested);

    assert_eq!(layout.base, project.join(".lnx"));
    assert_eq!(layout.kernel, home.join(".lnx/vmlinuz"));
    assert_eq!(
        layout.rootfs,
        project.join(".lnx/instances/dev/rootfs.ext4")
    );
}

#[test]
fn resolve_honors_lnx_base_for_kernel_store() {
    let base = PathBuf::from("/tmp/lnx-explicit-base");
    let layout = Layout::resolve_with_env(
        "dev",
        None,
        None,
        Some(base.clone()),
        None,
        PathBuf::from("/Users/test"),
    );

    assert_eq!(layout.kernel, base.join("vmlinuz"));
}

#[test]
fn resolve_walks_past_ancestor_without_requested_instance() {
    let temp = TempDir::new("ancestor-miss");
    let home = temp.path.join("home");
    let project = home.join("work/project");
    let nested = project.join("src/module");
    fs::create_dir_all(project.join(".lnx").join("instances").join("other"))
        .expect("create project instance");
    fs::create_dir_all(home.join(".lnx").join("instances").join("dev"))
        .expect("create home instance");
    fs::create_dir_all(&nested).expect("create nested cwd");

    let layout =
        Layout::resolve_with_env_and_cwd("dev", None, None, None, None, home.clone(), nested);

    assert_eq!(layout.base, home.join(".lnx"));
}

#[test]
fn resolve_in_base_places_new_instances_in_selected_store() {
    let base = PathBuf::from("/tmp/lnx-selected-base");
    let layout = Layout::resolve_in_base("new", base.clone(), None, None);

    assert_eq!(layout.base, base);
    assert_eq!(
        layout.rootfs,
        PathBuf::from("/tmp/lnx-selected-base/instances/new/rootfs.ext4")
    );
}

#[test]
fn transaction_root_coexists_with_a_legacy_reserved_name() {
    let temp = TempDir::new("transaction-root-collision");
    let instances = temp.path.join("instances");
    let legacy = instances.join(INSTANCE_TRANSACTION_DIR);
    fs::create_dir_all(&legacy).expect("create legacy instance");
    fs::write(legacy.join("rootfs.ext4"), b"legacy").expect("write legacy state");

    let transaction =
        ensure_instance_transaction_root(&instances).expect("create suffixed transaction root");

    assert_ne!(transaction, legacy);
    assert!(transaction.starts_with(&instances));
    assert!(is_instance_transaction_root(&transaction));
    assert!(!is_instance_transaction_root(&legacy));
    assert_eq!(fs::read(legacy.join("rootfs.ext4")).unwrap(), b"legacy");
}

#[test]
fn sockets_stay_in_a_short_run_dir() {
    let run_dir = PathBuf::from("/Users/test/.lnx/instances/dev");
    let socket_dir = socket_dir_for(&run_dir, Path::new("/short/lnx"));
    assert_eq!(socket_dir, run_dir);
}

#[test]
fn sockets_move_to_a_stable_short_dir_when_the_run_dir_is_too_deep() {
    let run_dir = PathBuf::from(format!(
        "/Users/test/{}/.lnx/instances/dev",
        "deeply-nested-project-directory/".repeat(3)
    ));
    let socket_dir = socket_dir_for(&run_dir, Path::new("/short/lnx"));
    assert_eq!(socket_dir, PathBuf::from("/short/lnx/c3682337bb288ec3"));
    assert_eq!(
        socket_dir,
        socket_dir_for(&run_dir, Path::new("/short/lnx"))
    );
    assert!(unix_socket_path_fits(&longest_runtime_socket_path(
        &socket_dir
    )));
}

#[test]
fn relocated_socket_dir_is_private() {
    let temp = TempDir::new("socket-dir");
    let deep = temp.path.join("d".repeat(UNIX_SOCKET_PATH_CAPACITY));
    let layout = Layout::resolve_for_base("dev", None, None, deep.clone(), None, deep.clone());
    let socket_dir = layout.socket_dir();
    assert_ne!(socket_dir, layout.run_dir);
    layout.create_runtime_dirs().expect("create runtime dirs");
    let mode = fs::metadata(&socket_dir).expect("stat").permissions();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(mode.mode() & 0o777, 0o700);
    let _ = fs::remove_dir(&socket_dir);
}
