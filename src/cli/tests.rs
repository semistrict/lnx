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
fn version_flag_prints_version_instead_of_running_a_guest_command() {
    let error = Cli::try_parse_from(["lnx", "--version"]).expect_err("version exits early");
    assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
    assert_eq!(
        error.to_string(),
        format!("lnx {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn version_flag_after_guest_command_is_forwarded_to_the_guest() {
    let cli = Cli::try_parse_from(["lnx", "node", "--version"]).expect("parse");
    assert_eq!(cli.guest_command, ["node", "--version"]);
}

#[test]
fn a_misspelled_lnx_flag_is_an_error_not_a_guest_command() {
    let error = Cli::try_parse_from(["lnx", "--memroy-mib", "8192", "true"])
        .expect_err("unknown flag");
    assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);

    let cli = Cli::try_parse_from(["lnx", "--", "--weird-binary"]).expect("parse");
    assert_eq!(cli.guest_command, ["--weird-binary"]);
}

#[test]
fn guest_command_flags_belong_to_the_guest() {
    let cli = Cli::try_parse_from(["lnx", "ls", "-la", "--instance", "x"]).expect("parse");
    assert_eq!(cli.guest_command, ["ls", "-la", "--instance", "x"]);
    assert_eq!(cli.instance, "default");
}

#[test]
fn lnx_options_work_after_run() {
    let cli = Cli::try_parse_from(["lnx", "run", "--instance", "t1", "--root", "pwd"])
        .expect("parse");
    assert_eq!(cli.instance, "t1");
    assert!(cli.root);
    let Some(Command::Run(args)) = cli.command else {
        panic!("expected run");
    };
    assert_eq!(args.command, ["pwd"]);
}

#[test]
fn fork_destination_is_not_the_source_instance() {
    let cli = Cli::try_parse_from(["lnx", "--instance", "src", "fork", "dst"]).expect("parse");
    assert_eq!(cli.instance, "src");
    let Some(Command::Fork(args)) = cli.command else {
        panic!("expected fork");
    };
    assert_eq!(args.destination, "dst");
}

#[test]
fn parses_exec_options() {
    let cli = Cli::try_parse_from([
        "lnx", "-e", "A=1", "--env", "B=x=y", "-w", "src", "--timeout", "90s", "-d", "make",
    ])
    .expect("parse");
    assert_eq!(
        cli.env,
        [
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "x=y".to_string())
        ]
    );
    assert_eq!(cli.workdir.as_deref(), Some("src"));
    assert_eq!(cli.timeout, Some(Duration::from_secs(90)));
    assert!(cli.detach);
    assert_eq!(cli.guest_command, ["make"]);

    assert!(Cli::try_parse_from(["lnx", "-e", "NOEQUALS", "true"]).is_err());
}

#[test]
fn parses_durations() {
    assert_eq!(parse_duration("30"), Ok(Duration::from_secs(30)));
    assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
    assert_eq!(parse_duration("1.5s"), Ok(Duration::from_millis(1500)));
    assert_eq!(parse_duration("5m"), Ok(Duration::from_secs(300)));
    assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7200)));
    assert!(parse_duration("0").is_err());
    assert!(parse_duration("5d").is_err());
    assert!(parse_duration("soon").is_err());
}

#[test]
fn exec_options_round_trip_through_flags() {
    let exec = runner::ExecOptions {
        run_as_root: true,
        env: vec![("A".to_string(), "1".to_string())],
        workdir: Some("/srv".to_string()),
        timeout: Some(Duration::from_millis(1500)),
        detach: false,
    };
    let mut argv = vec!["lnx".to_string()];
    argv.extend(exec_option_args(&exec));
    argv.push("true".to_string());

    let cli = Cli::try_parse_from(argv).expect("parse");
    assert!(cli.root);
    assert_eq!(cli.env, exec.env);
    assert_eq!(cli.workdir, exec.workdir);
    assert_eq!(cli.timeout, exec.timeout);
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
    // With the nix package store removed, the flag is unknown and
    // `packages` is a plain guest command.
    let error = Cli::try_parse_from(["lnx", "--package-store", "disabled", "run", "true"])
        .expect_err("unknown lnx flags are errors");
    assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);

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

    assert!(matches!(args.command, InstancesCommand::List { json: false }));
}

#[test]
fn listings_take_json() {
    let cli = Cli::try_parse_from(["lnx", "instances", "list", "--json"]).expect("parse");
    let Some(Command::Instances(args)) = cli.command else {
        panic!("expected instances command");
    };
    assert!(matches!(args.command, InstancesCommand::List { json: true }));

    let cli = Cli::try_parse_from(["lnx", "checkpoints", "--json"]).expect("parse");
    let Some(Command::Checkpoints(args)) = cli.command else {
        panic!("expected checkpoints command");
    };
    assert!(args.json);
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

use crate::store::test_support::{crash_after_serving, latest_disk};

fn store_instance(layout: &Layout, disk: &[u8], memory: bool) -> store::GenerationId {
    crate::store::test_support::initialized(layout, disk, memory)
}

fn latest_has_memory(layout: &Layout) -> bool {
    store::Store::new(&layout.instance_dir)
        .latest()
        .expect("read latest")
        .expect("latest")
        .manifest
        .has_memory()
}

#[test]
fn dropping_saved_memory_keeps_the_disk() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    store_instance(&layout, b"saved disk", true);

    drop_saved_memory(&layout).expect("drop memory");

    assert!(!latest_has_memory(&layout));
    assert_eq!(latest_disk(&layout), b"saved disk");
}

#[test]
fn dropping_saved_memory_refuses_while_the_vm_runs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    store_instance(&layout, b"saved disk", true);
    let owner = runner::test_support::hold_as_owner(&layout);

    let error = drop_saved_memory(&layout).expect_err("a live owner blocks dropping memory");

    assert!(error.to_string().contains("is running"));
    assert!(latest_has_memory(&layout));
    drop(owner);
}

#[test]
fn dropping_memory_of_an_instance_without_any_is_a_no_op() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    let image = store_instance(&layout, b"cold disk", false);

    drop_saved_memory(&layout).expect("nothing to drop");

    assert_eq!(
        store::Store::new(&layout.instance_dir)
            .latest()
            .unwrap()
            .map(|latest| latest.id().clone()),
        Some(image)
    );
}

#[test]
fn dropping_memory_of_a_missing_instance_creates_nothing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());

    let error = drop_saved_memory(&layout).expect_err("missing instance is rejected");

    assert_eq!(
        error.to_string(),
        "no instance named dev; running a command in it creates it"
    );
    assert!(!layout.instance_dir.exists());
}

#[test]
fn forking_a_missing_instance_creates_neither() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());

    let error = fork_checkpoint(layout.clone(), None, "copy").expect_err("missing source");

    assert_eq!(
        error.to_string(),
        "no instance named dev; running a command in it creates it"
    );
    assert!(!layout.instance_dir.exists());
    assert!(!temp.path().join("instances/copy").exists());
}

fn create_args(name: &str, from: Option<&str>) -> CreateArgs {
    CreateArgs {
        name: name.to_string(),
        from: from.map(str::to_string),
    }
}

#[test]
fn create_from_another_instance_copies_its_state_and_saves_settings() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = test_layout(temp.path());
    store_instance(&source, b"source disk", false);

    create_instance(&source, &create_args("copy", Some("dev")), (Some(3), None))
        .expect("create from dev");

    let copy = Layout::resolve_in_base("copy", temp.path().to_path_buf(), None, None);
    assert_eq!(latest_disk(&copy), b"source disk");
    assert_eq!(descriptor::load(&copy).expect("descriptor").cpus, Some(3));
}

#[test]
fn create_refuses_an_existing_instance_and_a_missing_source() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    store_instance(&layout, b"disk", false);

    let error = create_instance(&layout, &create_args("dev", None), (None, None))
        .expect_err("exists");
    assert_eq!(error.to_string(), "instance dev already exists");

    let error = create_instance(&layout, &create_args("copy", Some("ghost:v1")), (None, None))
        .expect_err("missing source");
    assert_eq!(
        error.to_string(),
        "no instance named ghost; running a command in it creates it"
    );
    assert!(!temp.path().join("instances/copy").exists());
}

#[test]
fn forking_into_an_invalid_name_is_refused() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    store_instance(&layout, b"disk", false);

    let error = fork_checkpoint(layout, None, "../escape").expect_err("invalid name");

    assert!(error.to_string().starts_with("invalid instance name"));
    assert!(!temp.path().join("escape").exists());
}

#[test]
fn a_crashed_run_must_be_recovered_before_memory_can_be_dropped() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    store_instance(&layout, b"saved disk", true);
    crash_after_serving(&layout, b"acknowledged writes");

    let error = drop_saved_memory(&layout).expect_err("crashed run needs a decision");

    let message = format!("{error:#}");
    assert!(message.contains("recover --keep"), "{message}");
    assert!(message.contains("recover --discard"), "{message}");
    assert!(latest_has_memory(&layout));
}

#[test]
fn recover_keep_saves_the_crashed_vms_disk() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    store_instance(&layout, b"saved disk", true);
    crash_after_serving(&layout, b"acknowledged writes");

    recover_instance(
        &layout,
        &RecoverArgs {
            keep: true,
            discard: false,
        },
    )
    .expect("keep crashed disk");

    assert_eq!(latest_disk(&layout), b"acknowledged writes");
    assert!(!latest_has_memory(&layout));
    runner::refuse_crashed_run(&layout).expect("instance is usable again");
}

#[test]
fn recover_discard_returns_to_the_last_saved_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    store_instance(&layout, b"saved disk", true);
    crash_after_serving(&layout, b"acknowledged writes");

    recover_instance(
        &layout,
        &RecoverArgs {
            keep: false,
            discard: true,
        },
    )
    .expect("discard crashed run");

    assert_eq!(latest_disk(&layout), b"saved disk");
    assert!(latest_has_memory(&layout));
    runner::refuse_crashed_run(&layout).expect("instance is usable again");
}

#[test]
fn recover_without_a_choice_explains_both() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    store_instance(&layout, b"saved disk", true);
    crash_after_serving(&layout, b"acknowledged writes");

    let error = recover_instance(
        &layout,
        &RecoverArgs {
            keep: false,
            discard: false,
        },
    )
    .expect_err("a choice is required");

    let message = format!("{error:#}");
    assert!(message.contains("recover --keep"), "{message}");
    assert!(message.contains("recover --discard"), "{message}");
}

#[test]
fn starting_after_a_crash_reports_how_to_recover() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    store_instance(&layout, b"saved disk", true);
    crash_after_serving(&layout, b"acknowledged writes");

    let error = runner::refuse_crashed_run(&layout).expect_err("crashed run blocks starts");

    let message = format!("{error:#}");
    assert!(message.contains("stopped unexpectedly"), "{message}");
    assert!(message.contains("recover --keep"), "{message}");
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
    store_instance(&layout, b"rootfs", false);
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder_layout = layout.clone();
    let holder = std::thread::spawn(move || {
        runner::with_exclusive_instance_state(&holder_layout, |_, _| {
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
    assert_eq!(latest_disk(&layout), b"rootfs");
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
        runner::with_exclusive_instance_state(&holder_layout, |_, _| {
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
    store_instance(&layout, b"rootfs", false);
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
    runner::test_support::write_instance_lease(
        &layout,
        &runner::test_support::lease_for(
            runner::LeaseRole::Maintenance,
            runner::test_support::exited_process(),
        ),
    );

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
        rootfs: None,
        instance_dir: PathBuf::from("/Users/test/.lnx/instances/dev"),
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
        &runner::ExecOptions {
            run_as_root: true,
            ..Default::default()
        },
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
            "--",
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
        rootfs: None,
        instance_dir: PathBuf::from("/Users/test/.lnx/instances/dev"),
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
        &runner::ExecOptions::default(),
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
        rootfs: None,
        instance_dir: base.join("instances/dev"),
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

fn write_snapshot_shape(snapshot: &Path, cpus: u32, memory_mib: u64, owner_args: &[&str]) {
    fs::create_dir_all(snapshot).expect("create snapshot");
    let mut header = [0u8; 40];
    header[0..8].copy_from_slice(b"LKRNSS01");
    header[8..12].copy_from_slice(&runner::SNAPSHOT_VMSTATE_VERSION.to_le_bytes());
    header[16..24].copy_from_slice(&(memory_mib * 1024 * 1024).to_le_bytes());
    header[32..36].copy_from_slice(&cpus.to_le_bytes());
    fs::write(snapshot.join("vmstate.bin"), header).expect("write vmstate header");
    let owner_args: Vec<_> = owner_args.iter().map(|arg| format!("{arg:?}")).collect();
    fs::write(
        snapshot.join("launch.json"),
        format!(
            r#"{{"version":2,"owner_args":[{}],"compatibility":{{"host_share_cache":{{"dax":true}}}},"shares":{{"no_host_shares":false,"host_home":"/Users/test","outside_home_cwd":null}}}}"#,
            owner_args.join(",")
        ),
    )
    .expect("write launch metadata");
}

#[test]
fn latest_snapshot_shape_reads_the_booted_vm_shape() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());
    let lock = runner::test_support::hold_as_owner(&layout);
    let store = store::Store::new(&layout.instance_dir);
    let staging = store.stage(&lock).expect("stage snapshot");
    fs::write(staging.dir().join(store::ROOTFS), b"disk").expect("write disk");
    fs::write(staging.dir().join(store::PAGES), b"pages").expect("write pages");
    write_snapshot_shape(
        staging.dir(),
        8,
        16384,
        &[
            "--cpus",
            "8",
            "--memory-mib",
            "16384",
            "--nested-kvm",
            "_vm-owner",
        ],
    );
    store.initialize(&lock, staging).expect("initialize store");

    assert_eq!(
        latest_snapshot_shape(&layout),
        Some(SnapshotShape {
            cpus: 8,
            memory_mib: 16384,
            nested_kvm: true,
            no_host_shares: false,
        })
    );
}

const SAVED_SHAPE: SnapshotShape = SnapshotShape {
    cpus: 2,
    memory_mib: 4096,
    nested_kvm: false,
    no_host_shares: false,
};

#[test]
fn changed_settings_on_an_instance_with_saved_memory_say_when_they_apply() {
    let config = descriptor::InstanceDescriptor {
        cpus: Some(4),
        ..Default::default()
    };

    assert_eq!(
        settings_pending_notice("dev", &config, Some(SAVED_SHAPE)).as_deref(),
        Some(
            "lnx: dev resumes its saved memory with 2 CPUs and 4096 MiB; the new settings apply at its next cold boot. `lnx --instance dev snapshots clear` drops the saved memory so the next run boots with them."
        )
    );
}

#[test]
fn settings_matching_the_saved_memory_or_without_it_need_no_notice() {
    let matching = descriptor::InstanceDescriptor {
        cpus: Some(2),
        memory_mib: Some(4096),
        ..Default::default()
    };
    let changed = descriptor::InstanceDescriptor {
        memory_mib: Some(8192),
        ..Default::default()
    };

    assert_eq!(settings_pending_notice("dev", &matching, Some(SAVED_SHAPE)), None);
    assert_eq!(settings_pending_notice("dev", &changed, None), None);
}

#[test]
fn instance_without_a_snapshot_has_no_snapshot_shape() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = test_layout(temp.path());

    assert_eq!(latest_snapshot_shape(&layout), None);
}
