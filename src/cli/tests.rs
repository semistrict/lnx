use super::*;
use std::fs;

#[test]
fn parses_short_forward_spec_as_localhost_to_localhost() {
    let forward = parse_port_forward("16081:6080").expect("parse");
    assert_eq!(forward.listen_host, "127.0.0.1");
    assert_eq!(forward.listen_port, 16081);
    assert_eq!(forward.guest_host, "127.0.0.1");
    assert_eq!(forward.guest_port, 6080);
}

#[test]
fn parses_explicit_forward_spec() {
    let forward = parse_port_forward("127.0.0.1:18080:localhost:8080").expect("parse");
    assert_eq!(forward.listen_host, "127.0.0.1");
    assert_eq!(forward.listen_port, 18080);
    assert_eq!(forward.guest_host, "localhost");
    assert_eq!(forward.guest_port, 8080);
}

#[test]
fn parses_directory_before_guest_command() {
    let cli = Cli::try_parse_from(["lnx", "-C", "/tmp", "echo", "hi"]).expect("parse");

    assert_eq!(cli.directory, Some(PathBuf::from("/tmp")));
    assert!(cli.command.is_none());
    assert_eq!(cli.guest_command, ["echo", "hi"]);
}

#[test]
fn init_requires_path_or_global_flag() {
    assert!(Cli::try_parse_from(["lnx", "init"]).is_err());

    let local = Cli::try_parse_from(["lnx", "init", "."]).expect("parse local init");
    let Some(Command::Init(args)) = local.command else {
        panic!("expected init command");
    };
    assert!(!args.global);
    assert_eq!(args.path, Some(PathBuf::from(".")));

    let global = Cli::try_parse_from(["lnx", "init", "-g"]).expect("parse global init");
    let Some(Command::Init(args)) = global.command else {
        panic!("expected init command");
    };
    assert!(args.global);
    assert!(args.path.is_none());

    assert!(Cli::try_parse_from(["lnx", "init", "-g", "."]).is_err());
}

#[test]
fn init_path_accepts_default_instance_seed() {
    let cli = Cli::try_parse_from(["lnx", "init", ".", "--default-instance", "alpine:3.21"])
        .expect("parse local init seed");
    let Some(Command::Init(args)) = cli.command else {
        panic!("expected init command");
    };

    assert_eq!(args.path, Some(PathBuf::from(".")));
    assert_eq!(args.default_instance.as_deref(), Some("alpine:3.21"));
}

#[test]
fn default_package_bootstrap_skips_internal_builders() {
    assert!(should_bootstrap_default_package_store(
        "default", false, false
    ));
    assert!(!should_bootstrap_default_package_store(
        "default", true, false
    ));
    assert!(!should_bootstrap_default_package_store(
        "default", false, true
    ));
    assert!(!should_bootstrap_default_package_store(
        "nix-builder-oci-builder",
        false,
        false
    ));
}

#[test]
fn explicit_package_install_infers_binary_from_flake_attr() {
    assert_eq!(
        infer_package_binaries(&["nixpkgs#go".to_string()]),
        vec!["go".to_string()]
    );
    assert_eq!(
        infer_package_binaries(&["github:NixOS/nixpkgs/nixos-unstable#nodejs_latest".to_string()]),
        vec!["nodejs_latest".to_string()]
    );
}

#[test]
fn fs_unshare_parses_path() {
    let cli = Cli::try_parse_from(["lnx", "fs", "unshare", "/Users/test/project"])
        .expect("parse fs unshare");
    let Some(Command::Fs(args)) = cli.command else {
        panic!("expected fs command");
    };
    let FsCommand::Unshare(args) = args.command;

    assert_eq!(args.path, Some(PathBuf::from("/Users/test/project")));
}

#[test]
fn snapshots_clear_parses() {
    let cli = Cli::try_parse_from(["lnx", "snapshots", "clear"]).expect("parse snapshots clear");
    let Some(Command::Snapshots(args)) = cli.command else {
        panic!("expected snapshots command");
    };

    assert!(matches!(args.command, SnapshotsCommand::Clear));
}

#[test]
fn vhost_user_fs_mount_parses() {
    let cli = Cli::try_parse_from([
        "lnx",
        "--vhost-user-fs",
        "tag=testfs,mount=/mnt/testfs,socket=/tmp/testfs.sock",
        "true",
    ])
    .expect("parse vhost-user fs mount");

    assert_eq!(
        cli.vhost_user_fs,
        vec![runner::VhostUserFsMount {
            tag: "testfs".to_string(),
            mountpoint: "/mnt/testfs".to_string(),
            socket: PathBuf::from("/tmp/testfs.sock"),
            read_only: true,
        }]
    );
}

#[test]
fn vhost_user_fs_rejects_writable_mounts() {
    let err = Cli::try_parse_from([
        "lnx",
        "--vhost-user-fs",
        "tag=testfs,mount=/mnt/testfs,socket=/tmp/testfs.sock,rw",
        "true",
    ])
    .expect_err("reject writable vhost-user fs mount");

    assert!(
        err.to_string()
            .contains("vhost-user fs mounts are read-only only"),
        "{err}"
    );
}

#[test]
fn init_local_target_normalizes_relative_path() {
    let target = init_local_target(Some(Path::new("project")))
        .expect("target")
        .expect("local target");

    assert_eq!(
        target.dest_base,
        std::env::current_dir()
            .expect("cwd")
            .join("project")
            .join(".lnx")
    );
}

#[test]
fn parse_git_worktree_list_detects_linked_worktree() {
    let output = "\
worktree /repo/main
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /repo/feature
HEAD 2222222222222222222222222222222222222222
branch refs/heads/feature
";

    let worktree =
        parse_git_worktree_list(output, Path::new("/repo/feature")).expect("linked worktree");

    assert_eq!(worktree.main_root, PathBuf::from("/repo/main"));
    assert_eq!(worktree.current_root, PathBuf::from("/repo/feature"));
}

#[test]
fn parse_git_worktree_list_ignores_main_checkout() {
    let output = "\
worktree /repo/main
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /repo/feature
HEAD 2222222222222222222222222222222222222222
branch refs/heads/feature
";

    assert!(parse_git_worktree_list(output, Path::new("/repo/main")).is_none());
}

#[test]
fn worktree_auto_init_plan_uses_main_checkout_lnx() {
    let temp = tempfile::tempdir().expect("tempdir");
    let main = temp.path().join("main");
    let linked = temp.path().join("linked");
    fs::create_dir_all(main.join(".lnx")).expect("create source base");
    fs::create_dir_all(&linked).expect("create linked worktree");

    let plan = worktree_auto_init_plan(
        &LinkedGitWorktree {
            main_root: main.clone(),
            current_root: linked.clone(),
        },
        "dev",
    )
    .expect("auto init plan");

    assert_eq!(plan.source_base, main.join(".lnx"));
    assert_eq!(plan.dest_base, linked.join(".lnx"));
}

#[test]
fn worktree_auto_init_plan_skips_existing_linked_instance() {
    let temp = tempfile::tempdir().expect("tempdir");
    let main = temp.path().join("main");
    let linked = temp.path().join("linked");
    fs::create_dir_all(main.join(".lnx")).expect("create source base");
    fs::create_dir_all(linked.join(".lnx/instances/dev")).expect("create dest instance");

    assert!(
        worktree_auto_init_plan(
            &LinkedGitWorktree {
                main_root: main,
                current_root: linked,
            },
            "dev",
        )
        .is_none()
    );
}

#[test]
fn cp_transfer_operands_allow_basic_recursive_flags() {
    let args = ["-a", "-R", "host:file", "/guest"]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let operands = cp_transfer_operands(&args).expect("operands");

    assert_eq!(operands, ["host:file", "/guest"]);
}

#[test]
fn cp_transfer_operands_reject_unsupported_flags() {
    let args = ["-f", "host:file", "/guest"]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();

    assert!(cp_transfer_operands(&args).is_err());
}

#[test]
fn deterministic_implies_one_cpu() {
    let deterministic = runner::DeterministicConfig {
        seed: "seed42".to_string(),
    };

    assert_eq!(effective_cpus(8, Some(&deterministic)), 1);
    assert_eq!(effective_cpus(8, None), 8);
}

#[test]
fn restore_snapshot_uses_latest_by_default() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    let latest = layout.snapshot_dir.join("latest");
    std::fs::create_dir_all(&latest).expect("create latest snapshot");

    assert_eq!(
        restore_snapshot_for_run(&layout, None, false, false),
        Some(latest)
    );
}

#[test]
fn restore_snapshot_skips_latest_for_explicit_image_inputs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    std::fs::create_dir_all(layout.snapshot_dir.join("latest")).expect("create latest snapshot");

    assert_eq!(restore_snapshot_for_run(&layout, None, true, false), None);
    assert_eq!(restore_snapshot_for_run(&layout, None, false, true), None);
}

#[test]
fn restore_snapshot_preserves_explicit_snapshot() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    let snapshot = temp.path().join("requested-snapshot");

    assert_eq!(
        restore_snapshot_for_run(&layout, Some(snapshot.clone()), true, true),
        Some(snapshot)
    );
}

#[test]
fn clear_latest_snapshot_removes_snapshot_runtime_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    let paths = [
        layout.snapshot_dir.join("latest"),
        layout.snapshot_dir.join(".latest.next"),
        layout.snapshot_dir.join(".latest.previous"),
        layout.snapshot_dir.join(".restore-work"),
    ];
    for path in &paths {
        fs::create_dir_all(path).expect("create snapshot path");
        fs::write(path.join("marker"), b"x").expect("write marker");
    }

    clear_latest_snapshot(&layout).expect("clear latest snapshot");

    for path in &paths {
        assert!(!path.exists(), "{} should be removed", path.display());
    }
}

#[test]
fn package_store_stamp_from_launch_defaults_to_disabled() {
    assert_eq!(
        package_store_stamp_from_launch(r#"{"compatibility":{"host_share_cache":{"dax":false}}}"#),
        "disabled-v1"
    );
    assert_eq!(
        package_store_stamp_from_launch(
            r#"{"compatibility":{"host_share_cache":{"dax":false},"packages":{"mode":"readonly","root":"/Users/ramon/.lnx/stores/nix-linux-aarch64"}}}"#
        ),
        "readonly-v1 root=/Users/ramon/.lnx/stores/nix-linux-aarch64"
    );
}

#[test]
fn nested_deterministic_inner_args_preserve_requested_run() {
    let layout = Layout {
        base: PathBuf::from("/Users/test/.lnx"),
        instance: "dev".to_string(),
        kernel: PathBuf::from("/Users/test/.lnx/vmlinuz"),
        rootfs: PathBuf::from("/Users/test/.lnx/instances/dev/rootfs.ext4"),
        instance_dir: PathBuf::from("/Users/test/.lnx/instances/dev"),
        snapshot_dir: PathBuf::from("/Users/test/.lnx/instances/dev/memory-snapshots"),
        checkpoint_dir: PathBuf::from("/Users/test/.lnx/instances/dev/checkpoints"),
        vm_initialized: PathBuf::from("/Users/test/.lnx/instances/dev/vm-initialized"),
        run_dir: PathBuf::from("/Users/test/.lnx/instances/dev"),
        console_log: PathBuf::from("/Users/test/.lnx/instances/dev/console.log"),
    };
    let args = nested_deterministic_inner_args(
        &layout,
        1,
        768,
        Some(Path::new(
            "/Users/test/.lnx/instances/dev/memory-snapshots/latest",
        )),
        &runner::DeterministicConfig {
            seed: "seed42".to_string(),
        },
        true,
        true,
        &["bash".to_string(), "-lc".to_string(), "date".to_string()],
        Vec::new(),
    );

    assert_eq!(
        args,
        vec![
            "--instance",
            "dev",
            "--kernel",
            "/Users/test/.lnx/vmlinuz",
            "--rootfs",
            "/Users/test/.lnx/instances/dev/rootfs.ext4",
            "--cpus",
            "1",
            "--memory-mib",
            "768",
            "--no-host-shares",
            "--deterministic",
            "seed42",
            "--snapshot",
            "/Users/test/.lnx/instances/dev/memory-snapshots/latest",
            "--trace-events",
            "--root",
            "bash",
            "-lc",
            "date",
        ]
    );
}

#[test]
fn nested_deterministic_inner_args_preserve_checkpoint_subcommand() {
    let layout = Layout {
        base: PathBuf::from("/Users/test/.lnx"),
        instance: "dev".to_string(),
        kernel: PathBuf::from("/Users/test/.lnx/vmlinuz"),
        rootfs: PathBuf::from("/Users/test/.lnx/instances/dev/rootfs.ext4"),
        instance_dir: PathBuf::from("/Users/test/.lnx/instances/dev"),
        snapshot_dir: PathBuf::from("/Users/test/.lnx/instances/dev/memory-snapshots"),
        checkpoint_dir: PathBuf::from("/Users/test/.lnx/instances/dev/checkpoints"),
        vm_initialized: PathBuf::from("/Users/test/.lnx/instances/dev/vm-initialized"),
        run_dir: PathBuf::from("/Users/test/.lnx/instances/dev"),
        console_log: PathBuf::from("/Users/test/.lnx/instances/dev/console.log"),
    };
    let args = nested_deterministic_inner_args(
        &layout,
        1,
        512,
        None,
        &runner::DeterministicConfig {
            seed: "default".to_string(),
        },
        false,
        false,
        &[],
        vec![
            "checkpoint".to_string(),
            "-m".to_string(),
            "deterministic-base".to_string(),
        ],
    );

    assert!(args.ends_with(&[
        "checkpoint".to_string(),
        "-m".to_string(),
        "deterministic-base".to_string(),
    ]));
}

fn test_layout(base: &Path) -> Layout {
    Layout {
        base: base.to_path_buf(),
        instance: "dev".to_string(),
        kernel: base.join("vmlinuz"),
        rootfs: base.join("instances/dev/rootfs.ext4"),
        instance_dir: base.join("instances/dev"),
        snapshot_dir: base.join("instances/dev/memory-snapshots"),
        checkpoint_dir: base.join("instances/dev/checkpoints"),
        vm_initialized: base.join("instances/dev/vm-initialized"),
        run_dir: base.join("instances/dev"),
        console_log: base.join("instances/dev/console.log"),
    }
}

#[test]
fn nested_deterministic_script_quotes_paths_and_exports_inner_base() {
    let script = nested_deterministic_script(
        Path::new("/Users/test/src/target/aarch64-unknown-linux-musl/debug/lnx"),
        Path::new("/Users/test/.lnx"),
        Some(Path::new("/tmp/lnx run")),
        &["--instance".to_string(), "dev one".to_string()],
    );

    assert!(!script.contains("LNX_ROOTFS_BACKEND"));
    assert!(script.contains("export LNX_BASE='/Users/test/.lnx'"));
    assert!(script.contains("export LNX_RUN_BASE='/tmp/lnx run'"));
    assert!(!script.contains("GVPROXY_PATH"));
    assert!(script.contains("exec \"$LNX_BIN\" '--instance' 'dev one'"));
}

#[test]
fn linux_lnx_candidates_use_current_profile() {
    let candidates = linux_lnx_candidates(Path::new("/Users/test/src/target/release/lnx"));

    assert!(candidates.contains(&PathBuf::from(
        "/Users/test/src/target/aarch64-unknown-linux-musl/release/lnx"
    )));
}
