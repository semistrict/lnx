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
fn package_store_flag_and_packages_subcommand_are_gone() {
    // With the nix package store removed, both parse as plain guest commands.
    let cli = Cli::try_parse_from(["lnx", "--package-store", "disabled", "run", "true"])
        .expect("unknown flags fall through to the guest command");
    assert!(cli.command.is_none());
    assert_eq!(
        cli.guest_command,
        vec!["--package-store", "disabled", "run", "true"]
    );

    let cli = Cli::try_parse_from(["lnx", "packages", "list"])
        .expect("`packages` is no longer a subcommand");
    assert_eq!(cli.guest_command, vec!["packages", "list"]);
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
fn checkpoints_bare_parses_as_list() {
    let cli = Cli::try_parse_from(["lnx", "checkpoints"]).expect("parse checkpoints");
    let Some(Command::Checkpoints(args)) = cli.command else {
        panic!("expected checkpoints command");
    };

    assert!(args.command.is_none());
}

#[test]
fn checkpoints_delete_parses_identifier() {
    let cli = Cli::try_parse_from(["lnx", "checkpoints", "delete", "abc"])
        .expect("parse checkpoints delete");
    let Some(Command::Checkpoints(args)) = cli.command else {
        panic!("expected checkpoints command");
    };
    let Some(CheckpointsCommand::Delete { identifier }) = args.command else {
        panic!("expected checkpoints delete command");
    };

    assert_eq!(identifier, "abc");
}

#[test]
fn instances_list_parses() {
    let cli = Cli::try_parse_from(["lnx", "instances", "list"]).expect("parse instances list");
    let Some(Command::Instances(args)) = cli.command else {
        panic!("expected instances command");
    };

    assert!(matches!(args.command, InstancesCommand::List));
}

#[test]
fn instances_delete_parses_name() {
    let cli =
        Cli::try_parse_from(["lnx", "instances", "delete", "abc"]).expect("parse instances delete");
    let Some(Command::Instances(args)) = cli.command else {
        panic!("expected instances command");
    };
    let InstancesCommand::Delete { name } = args.command else {
        panic!("expected instances delete command");
    };

    assert_eq!(name, "abc");
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
fn default_restore_version_mismatch_is_a_hard_actionable_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    let snapshot = layout.snapshot_dir.join("latest");
    fs::create_dir_all(&snapshot).expect("create snapshot");
    fs::write(
        snapshot.join("launch.json"),
        r#"{
            "version": 1,
            "owner_args": [],
            "compatibility": {"host_share_cache": {"dax": true}},
            "shares": {
                "no_host_shares": true,
                "host_home": null,
                "outside_home_cwd": null
            }
        }"#,
    )
    .expect("write legacy launch metadata");

    let error =
        require_default_restore_version_compatibility(Some(snapshot.clone()), false, &layout)
            .expect_err("reject incompatible default snapshot");
    let message = error.to_string();
    assert!(message.contains("incompatible with this lnx version"));
    assert!(message.contains("lnx --instance dev snapshots clear"));
    assert!(snapshot.exists(), "rejected snapshot must remain intact");
}

#[test]
fn running_owner_skips_unused_default_snapshot_version_check() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    let snapshot = layout.snapshot_dir.join("latest");
    fs::create_dir_all(&snapshot).expect("create snapshot");
    fs::write(snapshot.join("launch.json"), r#"{"version":1}"#)
        .expect("write legacy launch metadata");
    let owner = runner::BootstrapLock::try_acquire(&layout.run_dir.join("bootstrap.lock.d"))
        .expect("acquire owner lock")
        .expect("owner lock");

    let selected =
        require_default_restore_version_compatibility(Some(snapshot.clone()), false, &layout)
            .expect("running owner does not use on-disk snapshot");

    assert_eq!(selected, Some(snapshot));
    drop(owner);
}

#[test]
fn orphaned_restore_work_is_reported_before_snapshot_clear_advice() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    let snapshot = layout.snapshot_dir.join("latest");
    fs::create_dir_all(&snapshot).expect("create snapshot");
    fs::write(snapshot.join("launch.json"), r#"{"version":1}"#)
        .expect("write legacy launch metadata");
    fs::create_dir_all(layout.snapshot_dir.join(runner::RESTORE_WORK_SNAPSHOT))
        .expect("create restore work");
    fs::write(
        layout.snapshot_dir.join(runner::RESTORE_WORK_ACTIVE_MARKER),
        b"generation_id=recovery\n",
    )
    .expect("write active marker");

    let error = require_default_restore_version_compatibility(Some(snapshot), false, &layout)
        .expect_err("orphaned work takes precedence");

    assert!(error.to_string().contains("recoverable state"));
    assert!(
        !error
            .to_string()
            .contains("incompatible with this lnx version")
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
        layout.snapshot_dir.join(".restore-work.active"),
    ];
    for path in &paths {
        fs::create_dir_all(path).expect("create snapshot path");
        fs::write(path.join("marker"), b"x").expect("write marker");
    }
    let stale_clear_trash = layout.snapshot_dir.join(".latest.clear-999999-0");
    fs::create_dir_all(&stale_clear_trash).expect("create stale clear trash");
    fs::write(stale_clear_trash.join("large-snapshot-page"), b"x")
        .expect("write stale clear trash");
    runner::write_final_snapshot_outcome(&layout, &Err(anyhow::anyhow!("snapshot failed")))
        .expect("write failed snapshot outcome");

    clear_latest_snapshot(&layout).expect("clear latest snapshot");

    for path in &paths {
        assert!(!path.exists(), "{} should be removed", path.display());
    }
    assert!(
        !stale_clear_trash.exists(),
        "a retry should clean trash left by an interrupted clear"
    );
    let acknowledged = runner::read_final_snapshot_outcome(&layout)
        .expect("read acknowledged outcome")
        .expect("acknowledgement remains for concurrent stop verification");
    assert!(acknowledged.succeeded);
    assert!(!acknowledged.pending);
}

#[test]
fn clear_latest_snapshot_refuses_while_owner_is_live() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    fs::create_dir_all(&layout.run_dir).expect("create run dir");
    let latest = layout.snapshot_dir.join("latest");
    let work = layout.snapshot_dir.join(runner::RESTORE_WORK_SNAPSHOT);
    fs::create_dir_all(&latest).expect("create latest");
    fs::create_dir_all(&work).expect("create restore work");
    fs::write(
        layout.snapshot_dir.join(runner::RESTORE_WORK_ACTIVE_MARKER),
        b"generation_id=active\n",
    )
    .expect("write restore marker");
    runner::write_final_snapshot_outcome(&layout, &Err(anyhow::anyhow!("snapshot failed")))
        .expect("write failed snapshot outcome");
    let outcome_before = fs::read(layout.snapshot_dir.join(runner::FINAL_SNAPSHOT_OUTCOME))
        .expect("read outcome before clear");
    let owner = runner::BootstrapLock::try_acquire(&layout.run_dir.join("bootstrap.lock.d"))
        .expect("acquire owner lock")
        .expect("owner lock");

    let error = clear_latest_snapshot(&layout).expect_err("running owner blocks snapshot clear");

    assert!(error.to_string().contains("running VM owner"));
    assert!(latest.exists());
    assert!(work.exists());
    assert!(
        layout
            .snapshot_dir
            .join(runner::RESTORE_WORK_ACTIVE_MARKER)
            .exists()
    );
    assert_eq!(
        fs::read(layout.snapshot_dir.join(runner::FINAL_SNAPSHOT_OUTCOME))
            .expect("live-owner outcome remains"),
        outcome_before
    );
    drop(owner);
}

#[test]
fn clear_snapshot_recovery_works_after_split_run_directory_loss() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut layout = test_layout(temp.path());
    layout.run_dir = temp.path().join("ephemeral-run/default");
    fs::create_dir_all(layout.snapshot_dir.join("latest")).expect("create latest snapshot");
    runner::write_final_snapshot_outcome(
        &layout,
        &Err(anyhow::anyhow!("failed before run dir loss")),
    )
    .expect("write persistent failed outcome");
    assert!(!layout.run_dir.exists());

    clear_latest_snapshot(&layout).expect("clear with missing split run dir");

    assert!(!layout.snapshot_dir.join("latest").exists());
    assert!(
        layout.run_dir.exists(),
        "coordination directory is recreated"
    );
    let outcome = runner::read_final_snapshot_outcome(&layout)
        .expect("read clear acknowledgement")
        .expect("clear acknowledgement");
    assert!(outcome.succeeded);
}

#[test]
fn clear_nonexistent_instance_does_not_create_phantom_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());

    let error = clear_latest_snapshot(&layout).expect_err("missing instance is rejected");

    assert!(error.to_string().contains("instance does not exist"));
    assert!(!layout.instance_dir.exists());
    assert!(!layout.run_dir.join("bootstrap.lock.d.guard").exists());
}

#[test]
fn clear_never_snapshotted_instance_is_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    fs::create_dir_all(&layout.instance_dir).expect("create cold instance");
    fs::write(&layout.rootfs, b"cold-rootfs").expect("write cold rootfs");

    clear_latest_snapshot(&layout).expect("clear cold instance");

    assert_eq!(
        fs::read(&layout.rootfs).expect("rootfs remains"),
        b"cold-rootfs"
    );
    assert!(layout.snapshot_dir.exists());
}

#[test]
fn delete_instance_missing_instance_reports_not_found() {
    let temp = tempfile::tempdir().expect("tempdir");

    let err = delete_instance(temp.path(), "missing-instance").expect_err("expect not found");

    assert_eq!(err.to_string(), "instance not found: missing-instance");
}

#[test]
fn delete_instance_refuses_a_concurrent_state_copy() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    fs::create_dir_all(&layout.instance_dir).expect("create instance");
    fs::write(&layout.rootfs, b"rootfs").expect("write rootfs");
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder_layout = layout.clone();
    let holder = std::thread::spawn(move || {
        runner::with_exclusive_instance_state(&holder_layout, || {
            held_tx.send(()).expect("signal held lease");
            release_rx.recv().expect("wait for release");
            Ok(())
        })
        .expect("reserve state")
        .expect("exclusive state lease");
    });
    held_rx.recv().expect("wait for held lease");

    let error = delete_instance(temp.path(), "dev").expect_err("state copy blocks deletion");

    assert!(error.to_string().contains("became busy"));
    assert!(layout.rootfs.exists());
    release_tx.send(()).expect("release state lease");
    holder.join().expect("join state holder");
}

#[test]
fn set_settings_refuses_a_concurrent_state_operation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    fs::create_dir_all(&layout.instance_dir).expect("create instance");
    descriptor::save(
        &layout,
        &descriptor::InstanceDescriptor {
            name: Some("dev".to_string()),
            cpus: Some(2),
            ..Default::default()
        },
    )
    .expect("write descriptor");
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder_layout = layout.clone();
    let holder = std::thread::spawn(move || {
        runner::with_exclusive_instance_state(&holder_layout, || {
            held_tx.send(()).expect("signal held lease");
            release_rx.recv().expect("wait for release");
            Ok(())
        })
        .expect("reserve state")
        .expect("exclusive state lease");
    });
    held_rx.recv().expect("wait for held lease");

    let error = set_instance_settings(&layout, &["cpus=4".to_string()])
        .expect_err("state operation blocks settings");

    assert!(error.to_string().contains("state operation in progress"));
    assert_eq!(descriptor::load(&layout).unwrap().cpus, Some(2));
    release_tx.send(()).expect("release state lease");
    holder.join().expect("join state holder");
}

#[test]
fn delete_instance_atomically_detaches_and_removes_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    fs::create_dir_all(layout.instance_dir.join("nested")).expect("create instance");
    fs::write(&layout.rootfs, b"rootfs").expect("write rootfs");
    fs::write(layout.instance_dir.join("nested/state"), b"state").expect("write state");

    delete_instance(temp.path(), "dev").expect("delete instance");

    assert!(!layout.instance_dir.exists());
    assert!(
        find_detached_instance_state(&temp.path().join("instances"), "dev")
            .expect("scan detached state")
            .is_empty()
    );
}

#[test]
fn delete_instance_retries_cleanup_after_a_committed_detach() {
    let temp = tempfile::tempdir().expect("tempdir");
    let instances = temp.path().join("instances");
    let transaction_root = crate::paths::ensure_instance_transaction_root(&instances)
        .expect("create transaction root");
    let trash = transaction_root
        .join("delete/dev")
        .join(format!("{}-0", std::process::id()));
    fs::create_dir_all(trash.join("state")).expect("create detached state");
    fs::write(trash.join("state/rootfs.ext4"), b"rootfs").expect("write detached rootfs");

    delete_instance(temp.path(), "dev").expect("retry detached cleanup");

    assert!(!trash.exists());
}

#[test]
fn split_delete_recovers_after_only_persistent_state_was_detached() {
    let temp = tempfile::tempdir().expect("tempdir");
    let persistent_base = temp.path().join("persistent");
    let run_base = temp.path().join("runtime");
    let mut layout = test_layout(&persistent_base);
    layout.run_dir = run_base.join("instances/dev");
    layout.console_log = layout.run_dir.join("console.log");
    let persistent_transaction_root =
        crate::paths::ensure_instance_transaction_root(&persistent_base.join("instances"))
            .expect("create persistent transaction root");
    let persistent_trash = persistent_transaction_root
        .join("delete/dev")
        .join(format!("{}-0", std::process::id()));
    fs::create_dir_all(persistent_trash.join("state")).expect("create detached persistent state");
    fs::write(persistent_trash.join("state/rootfs.ext4"), b"old rootfs")
        .expect("write detached rootfs");
    let lock = layout.run_dir.join("bootstrap.lock.d");
    fs::create_dir_all(&lock).expect("create interrupted maintenance lease");
    let mut exited = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("spawn short-lived maintenance process");
    let stale_pid = exited.id();
    exited.wait().expect("reap maintenance process");
    fs::write(lock.join("maintenance.pid"), stale_pid.to_string())
        .expect("write stale maintenance pid");

    delete_resolved_instance(&persistent_base, "dev", &layout)
        .expect("recover interrupted split deletion");

    assert!(!persistent_trash.exists());
    assert!(!layout.run_dir.exists());
    assert!(
        find_detached_instance_state(&run_base.join("instances"), "dev")
            .expect("scan runtime trash")
            .is_empty()
    );
}

#[test]
fn instance_listing_keeps_valid_dot_names_and_hides_transactions() {
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
fn existing_path_aliases_are_not_treated_as_split_runtime_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let real = temp.path().join("real");
    let alias = temp.path().join("alias");
    fs::create_dir_all(&real).expect("create real directory");
    std::os::unix::fs::symlink(&real, &alias).expect("create alias");

    assert!(paths_refer_to_same_existing_entry(&real, &alias));
}

#[test]
fn remove_contained_instance_dir_removes_matching_dir() {
    let temp = tempfile::tempdir().expect("tempdir");
    let instances_root = temp.path().join("instances");
    let dir = instances_root.join("dev");
    fs::create_dir_all(dir.join("nested")).expect("create instance dir");
    fs::write(dir.join("nested/marker"), b"x").expect("write marker");

    remove_contained_instance_dir(&dir, &instances_root, "dev").expect("remove instance dir");

    assert!(!dir.exists());
}

#[test]
fn remove_contained_instance_dir_refuses_name_mismatch() {
    let temp = tempfile::tempdir().expect("tempdir");
    let instances_root = temp.path().join("instances");
    let dir = instances_root.join("dev");
    fs::create_dir_all(&dir).expect("create instance dir");

    let err = remove_contained_instance_dir(&dir, &instances_root, "other")
        .expect_err("expect containment guard to reject mismatched name");

    assert_eq!(
        err.to_string(),
        format!(
            "refusing to delete instance dir outside {}: {}",
            instances_root.display(),
            dir.display()
        )
    );
    assert!(dir.exists());
}

#[test]
fn remove_contained_instance_dir_refuses_nested_path() {
    let temp = tempfile::tempdir().expect("tempdir");
    let instances_root = temp.path().join("instances");
    let nested_dir = instances_root.join("sub").join("dev");
    fs::create_dir_all(&nested_dir).expect("create nested dir");

    let err = remove_contained_instance_dir(&nested_dir, &instances_root, "dev")
        .expect_err("expect containment guard to reject nested path");

    assert_eq!(
        err.to_string(),
        format!(
            "refusing to delete instance dir outside {}: {}",
            instances_root.display(),
            nested_dir.display()
        )
    );
    assert!(nested_dir.exists());
}

#[test]
fn remove_contained_instance_dir_refuses_outside_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    let instances_root = temp.path().join("instances");
    let outside_dir = temp.path().join("dev");
    fs::create_dir_all(&outside_dir).expect("create outside dir");

    let err = remove_contained_instance_dir(&outside_dir, &instances_root, "dev")
        .expect_err("expect containment guard to reject dir outside instances root");

    assert_eq!(
        err.to_string(),
        format!(
            "refusing to delete instance dir outside {}: {}",
            instances_root.display(),
            outside_dir.display()
        )
    );
    assert!(outside_dir.exists());
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
