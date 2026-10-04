use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::fsutil::remove_path_if_exists;
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::{
    checkpoints, descriptor, host_share, ingress, init,
    paths::{
        Layout, ensure_instance_transaction_root, instance_transaction_roots,
        is_instance_transaction_root,
    },
    runner, status, store,
};

const DEFAULT_CPUS: u8 = 2;
const DEFAULT_MEMORY_MIB: u32 = 4096;

#[derive(Debug, Parser)]
#[command(
    name = "lnx",
    version,
    about = "Linux VM runner using Rust and libkrun"
)]
pub struct Cli {
    #[arg(short = 'C', value_name = "DIR", help = "Run as if started in DIR")]
    directory: Option<PathBuf>,

    #[arg(long, env = "LNX_INSTANCE", default_value = "default")]
    instance: String,

    #[arg(long)]
    kernel: Option<PathBuf>,

    #[arg(long)]
    rootfs: Option<PathBuf>,

    #[arg(long, help = "Virtual CPUs (default: per-instance setting, then 2)")]
    cpus: Option<u8>,

    #[arg(
        long,
        help = "Memory in MiB (default: per-instance setting, then 4096)"
    )]
    memory_mib: Option<u32>,

    #[arg(
        long,
        help = "Restore from an explicit libkrun memory snapshot directory"
    )]
    snapshot: Option<PathBuf>,

    #[arg(long, help = "Request nested KVM support for the guest")]
    nested_kvm: bool,

    #[arg(
        long,
        value_name = "SEED",
        num_args = 0..=1,
        default_missing_value = "default",
        help = "Run with deterministic VM compatibility settings and optional seed"
    )]
    deterministic: Option<String>,

    #[arg(long, help = "Emit deterministic replay trace events")]
    trace_events: bool,

    #[arg(
        long,
        help = "Do not mount host directories into the guest with virtio-fs"
    )]
    no_host_shares: bool,

    #[arg(
        long,
        help = "Run the guest command as root instead of the host-matching user"
    )]
    root: bool,

    #[arg(
        long = "forward",
        value_parser = parse_port_forward,
        help = "Forward Mac localhost to guest localhost, like 16081:6080"
    )]
    forwards: Vec<runner::PortForward>,

    #[arg(
        long = "vhost-user-fs",
        value_name = "tag=NAME,mount=/GUEST/PATH,socket=/HOST/SOCK[,ro]",
        value_parser = parse_vhost_user_fs_mount,
        help = "Mount a read-only external vhost-user virtio-fs backend inside the guest"
    )]
    vhost_user_fs: Vec<runner::VhostUserFsMount>,

    #[command(subcommand)]
    command: Option<Command>,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    guest_command: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Initialize an instance from an image, rootfs, or existing VM")]
    Init(InitArgs),
    #[command(about = "Run a command in the guest")]
    Run(RunArgs),
    #[command(about = "Print instance paths")]
    Paths,
    #[command(about = "Create a checkpoint of the current instance")]
    Checkpoint(CheckpointArgs),
    #[command(about = "List checkpoints")]
    Checkpoints(CheckpointsArgs),
    #[command(about = "Manage memory snapshots")]
    Snapshots(SnapshotsArgs),
    #[command(about = "Recover an instance whose VM stopped unexpectedly after running commands")]
    Recover(RecoverArgs),
    #[command(about = "Fork a checkpoint into a new instance")]
    Fork(ForkArgs),
    #[command(about = "Filesystem state commands")]
    Fs(FsArgs),
    #[command(about = "Run an lnx server or push this instance to one")]
    Server(ServerArgs),
    #[command(about = "Manage the ingress service")]
    Ingress(IngressArgs),
    #[command(about = "List instances")]
    Instances(InstancesArgs),
    #[command(about = "Persist per-instance settings, like: set cpus=4 memory-mib=8192")]
    Set(SetArgs),
    #[command(about = "Print instance state and configuration as JSON")]
    Inspect,
    #[command(about = "Print instance logs")]
    Logs(LogsArgs),
    #[command(hide = true)]
    #[command(name = "_ingress")]
    HiddenIngress(HiddenIngressArgs),
    #[command(hide = true)]
    #[command(name = "_oci-build")]
    HiddenOciBuild(HiddenOciBuildArgs),
    #[command(hide = true)]
    #[command(name = "_sparse-copy")]
    HiddenSparseCopy(HiddenSparseCopyArgs),
    #[command(hide = true)]
    #[command(name = "_vm-owner")]
    HiddenVmOwner(HiddenVmOwnerArgs),
}

#[derive(Debug, Args)]
struct InitArgs {
    #[arg(
        short = 'g',
        long,
        conflicts_with = "path",
        help = "Initialize the global lnx store"
    )]
    global: bool,

    #[arg(
        value_name = "PATH",
        required_unless_present = "global",
        help = "Initialize PATH/.lnx"
    )]
    path: Option<PathBuf>,

    #[arg(
        long,
        value_name = "VM_INSTANCE_NAME|DOCKER_IMAGE_AND_TAG",
        requires = "path",
        conflicts_with = "image",
        help = "Seed the local default instance from an existing VM instance or OCI image"
    )]
    default_instance: Option<String>,

    #[arg(long)]
    kernel: Option<PathBuf>,

    #[arg(long)]
    rootfs: Option<PathBuf>,

    #[arg(
        long,
        conflicts_with = "rootfs",
        help = "Build the instance rootfs from an OCI image reference, like alpine:3.21"
    )]
    image: Option<String>,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Debug, Args)]
struct CheckpointArgs {
    #[arg(short = 'm')]
    message: Option<String>,
}

#[derive(Debug, Args)]
struct CheckpointsArgs {
    #[command(subcommand)]
    command: Option<CheckpointsCommand>,
    #[arg(long, help = "Print the list as JSON")]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum CheckpointsCommand {
    #[command(about = "Delete a checkpoint by id or name")]
    Delete { identifier: String },
}

#[derive(Debug, Args)]
struct SnapshotsArgs {
    #[command(subcommand)]
    command: SnapshotsCommand,
}

#[derive(Debug, Subcommand)]
enum SnapshotsCommand {
    #[command(
        about = "Drop the saved memory; the next run boots from the saved disk, which is kept"
    )]
    Clear,
}

#[derive(Debug, Args)]
struct RecoverArgs {
    #[arg(
        long,
        conflicts_with = "discard",
        help = "Keep the crashed VM's disk, losing only its memory"
    )]
    keep: bool,

    #[arg(
        long,
        help = "Discard the crashed VM's changes and return to the last saved state"
    )]
    discard: bool,
}

#[derive(Debug, Args)]
struct ForkArgs {
    #[arg(long)]
    checkpoint: Option<String>,

    instance: String,
}

#[derive(Debug, Args)]
struct FsArgs {
    #[command(subcommand)]
    command: FsCommand,
}

#[derive(Debug, Subcommand)]
enum FsCommand {
    #[command(about = "Inspect or clear host-share copy-on-write state")]
    Unshare(FsUnshareArgs),
}

#[derive(Debug, Args)]
struct FsUnshareArgs {
    #[arg(long, conflicts_with = "path", help = "List copy-on-write paths")]
    list: bool,

    #[arg(
        long,
        conflicts_with = "list",
        help = "Remove PATH's copy-on-write state"
    )]
    remove: bool,

    #[arg(value_name = "PATH", required_unless_present = "list")]
    path: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ServerArgs {
    #[arg(long, default_value = "127.0.0.1:7777")]
    listen: String,

    #[command(subcommand)]
    command: Option<ServerCommand>,
}

#[derive(Debug, Subcommand)]
enum ServerCommand {
    #[command(about = "Transfer this instance to an lnx server")]
    Push(ServerPushArgs),
}

#[derive(Debug, Args)]
struct ServerPushArgs {
    #[arg(help = "Server URL, like http://host:7777")]
    url: String,

    #[arg(long, help = "Import under a different instance name on the server")]
    target_instance: Option<String>,

    #[arg(long, help = "Replace an existing target instance")]
    replace: bool,

    #[arg(long, help = "Ask the server to start the imported instance")]
    start: bool,

    #[arg(long, help = "Idle TTL for the server-started VM owner")]
    idle_ttl_ms: Option<u64>,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Debug, Args)]
struct SetArgs {
    #[arg(required = true, value_name = "KEY=VALUE")]
    settings: Vec<String>,
}

#[derive(Debug, Args)]
struct LogsArgs {
    #[arg(long, help = "Print the guest console log instead of the run log")]
    console: bool,

    #[arg(long, help = "Print the VM owner process log instead of the run log")]
    owner: bool,
}

#[derive(Debug, Args)]
struct IngressArgs {
    #[command(subcommand)]
    command: IngressCommand,
}

#[derive(Debug, Args)]
struct InstancesArgs {
    #[command(subcommand)]
    command: InstancesCommand,
}

#[derive(Debug, Subcommand)]
enum InstancesCommand {
    List {
        #[arg(long, help = "Print the list as JSON")]
        json: bool,
    },
    #[command(about = "Delete an instance and all its state")]
    Delete {
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum IngressCommand {
    Enable,
    Disable,
    Status,
    #[command(about = "Disable ingress and remove the trusted lnx CA")]
    Uninstall,
}

#[derive(Debug, Args)]
struct HiddenOciBuildArgs {
    staging: PathBuf,
}

#[derive(Debug, Args)]
struct HiddenSparseCopyArgs {
    source: PathBuf,
    dest: PathBuf,
}

#[derive(Debug, Args)]
struct HiddenVmOwnerArgs {
    #[arg(long)]
    cwd: PathBuf,

    #[arg(long)]
    restore: Option<PathBuf>,

    #[arg(long)]
    no_host_shares: bool,

    #[arg(long, value_name = "SEED")]
    deterministic: Option<String>,

    #[arg(long)]
    trace_events: bool,
}

#[derive(Debug, Args)]
struct HiddenIngressArgs {
    #[arg(long)]
    spawn: bool,

    #[arg(long)]
    cleanup: bool,

    #[arg(long)]
    install_service: bool,

    #[arg(long)]
    uninstall_service: bool,

    #[arg(long, requires = "uninstall_service")]
    purge_ca: bool,

    #[arg(long)]
    refresh_if_running: bool,
}

impl HiddenIngressArgs {
    fn action(&self) -> ingress::HiddenAction {
        if self.cleanup {
            ingress::HiddenAction::Cleanup
        } else if self.refresh_if_running {
            ingress::HiddenAction::RefreshIfRunning
        } else if self.uninstall_service {
            ingress::HiddenAction::UninstallService {
                purge_ca: self.purge_ca,
            }
        } else if self.install_service {
            ingress::HiddenAction::InstallService
        } else if self.spawn {
            ingress::HiddenAction::Spawn
        } else {
            ingress::HiddenAction::RunDaemon
        }
    }
}

impl Cli {
    pub fn run(self) -> Result<()> {
        let Cli {
            directory,
            instance,
            kernel,
            rootfs,
            cpus,
            memory_mib,
            snapshot: snapshot_path,
            nested_kvm,
            deterministic,
            trace_events,
            no_host_shares,
            root,
            forwards,
            vhost_user_fs,
            command,
            guest_command,
        } = self;

        if let Some(directory) = directory {
            std::env::set_current_dir(&directory)
                .with_context(|| format!("change directory to {}", directory.display()))?;
        }

        crate::paths::validate_instance_name(&instance)?;
        let explicit_kernel = kernel.is_some();
        let explicit_rootfs = rootfs.is_some();
        let deterministic = deterministic.map(|seed| runner::DeterministicConfig { seed });
        validate_deterministic_args(
            nested_kvm,
            &forwards,
            &vhost_user_fs,
            deterministic.as_ref(),
            trace_events,
        )?;
        validate_vhost_user_fs_mounts(&vhost_user_fs)?;
        maybe_auto_init_git_worktree(
            &instance,
            command.as_ref(),
            explicit_kernel,
            explicit_rootfs,
        )?;
        let init_target = match &command {
            Some(Command::Init(args)) => init_local_target(args.path.as_deref())?,
            _ => None,
        };
        let layout = match &init_target {
            Some(target) => Layout::resolve_in_base(
                &instance,
                target.dest_base.clone(),
                kernel.clone(),
                rootfs.clone(),
            ),
            None => Layout::resolve(&instance, kernel.clone(), rootfs.clone())?,
        };
        if is_instance_transaction_root(&layout.instance_dir) {
            bail!(
                "instance name resolves to internal transaction state: {}",
                layout.instance_dir.display()
            );
        }
        let persisted = descriptor::load(&layout)?;
        // An instance still in the layout of an older lnx keeps its snapshot
        // shape there; move it into the store first so the shape is found.
        if matches!(command, None | Some(Command::Run(_)) | Some(Command::Checkpoint(_))) {
            runner::ensure_store(&layout)?;
        }
        // Saved memory can only resume in the shape it was taken with, so
        // that shape wins over saved settings, which apply at cold boot. Only
        // an explicit flag can ask for something else (and then fails with
        // the remedy rather than silently dropping memory).
        let snapshot_shape = latest_snapshot_shape(&layout);
        let cpus = cpus
            .or(snapshot_shape.map(|shape| shape.cpus))
            .or(persisted.cpus)
            .unwrap_or(DEFAULT_CPUS);
        let memory_mib = memory_mib
            .or(snapshot_shape.map(|shape| shape.memory_mib))
            .or(persisted.memory_mib)
            .unwrap_or(DEFAULT_MEMORY_MIB);
        let nested_kvm = nested_kvm || snapshot_shape.is_some_and(|shape| shape.nested_kvm);
        let effective_no_host_shares = no_host_shares
            || deterministic.is_some()
            || snapshot_shape.is_some_and(|shape| shape.no_host_shares);
        let cpus = effective_cpus(cpus, deterministic.as_ref());
        match command {
            Some(Command::Init(args)) => run_init_command(
                &layout,
                init_target,
                &instance,
                args,
                explicit_kernel,
                explicit_rootfs,
            ),
            Some(Command::Run(args)) => {
                let macos_deterministic =
                    deterministic.as_ref().filter(|_| cfg!(target_os = "macos"));
                if let Some(det) = macos_deterministic {
                    run_nested_deterministic_on_macos(
                        &layout,
                        cpus,
                        memory_mib,
                        snapshot_path.as_deref(),
                        det,
                        trace_events,
                        root,
                        &args.command,
                        "run",
                        Vec::new(),
                        explicit_kernel,
                    )
                } else {
                    run_guest(
                        layout,
                        args.command,
                        cpus,
                        memory_mib,
                        snapshot_path,
                        nested_kvm,
                        effective_no_host_shares,
                        deterministic.clone(),
                        trace_events,
                        root,
                        forwards,
                        vhost_user_fs.clone(),
                        explicit_kernel,
                    )
                }
            }
            Some(Command::Paths) => {
                println!("kernel: {}", layout.kernel.display());
                match init::instance_rootfs(&layout) {
                    Some(rootfs) => println!("rootfs: {}", rootfs.display()),
                    None => println!("rootfs: none"),
                }
                println!("base: {}", layout.base.display());
                println!("name: {}", layout.instance);
                println!("instance: {}", layout.instance_dir.display());
                println!(
                    "generations: {}",
                    store::Store::new(&layout.instance_dir)
                        .generations_dir()
                        .display()
                );
                Ok(())
            }
            Some(Command::Checkpoint(args)) => {
                let macos_deterministic =
                    deterministic.as_ref().filter(|_| cfg!(target_os = "macos"));
                if let Some(det) = macos_deterministic {
                    let mut subcommand = vec!["checkpoint".to_string()];
                    if let Some(message) = args.message {
                        subcommand.push("-m".to_string());
                        subcommand.push(message);
                    }
                    run_nested_deterministic_on_macos(
                        &layout,
                        cpus,
                        memory_mib,
                        snapshot_path.as_deref(),
                        det,
                        trace_events,
                        root,
                        &[],
                        "checkpoint",
                        subcommand,
                        explicit_kernel,
                    )
                } else {
                    create_checkpoint(&layout, args.message.as_deref())
                }
            }
            Some(Command::Checkpoints(args)) => match args.command {
                None => list_checkpoints(&layout, args.json),
                Some(CheckpointsCommand::Delete { identifier }) => {
                    delete_checkpoint(&layout, &identifier)
                }
            },
            Some(Command::Snapshots(args)) => run_snapshots_command(&layout, args),
            Some(Command::Recover(args)) => recover_instance(&layout, &args),
            Some(Command::Fork(args)) => {
                let macos_deterministic =
                    deterministic.as_ref().filter(|_| cfg!(target_os = "macos"));
                if let Some(det) = macos_deterministic {
                    let mut subcommand = vec!["fork".to_string()];
                    if let Some(checkpoint) = args.checkpoint {
                        subcommand.push("--checkpoint".to_string());
                        subcommand.push(checkpoint);
                    }
                    subcommand.push(args.instance);
                    run_nested_deterministic_on_macos(
                        &layout,
                        cpus,
                        memory_mib,
                        snapshot_path.as_deref(),
                        det,
                        trace_events,
                        root,
                        &[],
                        "fork",
                        subcommand,
                        explicit_kernel,
                    )
                } else {
                    fork_checkpoint(layout, args.checkpoint.as_deref(), &args.instance)
                }
            }
            Some(Command::Fs(args)) => match args.command {
                FsCommand::Unshare(unshare) => run_fs_unshare(&layout, unshare),
            },
            Some(Command::Server(args)) => match args.command {
                Some(ServerCommand::Push(push)) => crate::server::push(crate::server::PushConfig {
                    source: layout,
                    url: push.url,
                    target_instance: push.target_instance.unwrap_or(instance),
                    replace: push.replace,
                    start: push.start,
                    idle_ttl_ms: push.idle_ttl_ms,
                    command: push.command,
                }),
                None => crate::server::serve(crate::server::ServeConfig {
                    listen: args.listen,
                    cpus,
                    memory_mib,
                    nested_kvm,
                    no_host_shares: effective_no_host_shares,
                }),
            },
            Some(Command::Ingress(args)) => {
                let config = ingress::load_config()?;
                match args.command {
                    IngressCommand::Enable => ingress::enable(&config),
                    IngressCommand::Disable => ingress::disable(&config),
                    IngressCommand::Status => ingress::print_status(&config),
                    IngressCommand::Uninstall => ingress::uninstall(&config),
                }
            }
            Some(Command::Instances(args)) => match args.command {
                InstancesCommand::List { json } => list_instances(&layout.base, json),
                InstancesCommand::Delete { name } => delete_instance(&layout.base, &name),
            },
            Some(Command::Set(args)) => set_instance_settings(&layout, &args.settings),
            Some(Command::Inspect) => inspect_instance(&layout, cpus, memory_mib),
            Some(Command::Logs(args)) => print_instance_logs(&layout, args.console, args.owner),
            Some(Command::HiddenIngress(args)) => {
                let config = ingress::load_config()?;
                ingress::run_hidden(args.action(), config)
            }
            Some(Command::HiddenOciBuild(args)) => crate::oci::build_rootfs(&args.staging),
            Some(Command::HiddenSparseCopy(args)) => {
                crate::sparse_copy::clone_or_copy_file(&args.source, &args.dest)
            }
            Some(Command::HiddenVmOwner(args)) => runner::run_owner(runner::RunConfig {
                layout,
                command: Vec::new(),
                cwd: args.cwd,
                cpus,
                memory_mib,
                nested_kvm,
                restore_snapshot: args.restore,
                forwards,
                run_as_root: false,
                no_host_shares: effective_no_host_shares || args.no_host_shares,
                vhost_user_fs: vhost_user_fs.clone(),
                reuse_owner: true,
                deterministic: args
                    .deterministic
                    .map(|seed| runner::DeterministicConfig { seed })
                    .or(deterministic),
                trace_events: trace_events || args.trace_events,
            }),
            None => {
                let macos_deterministic =
                    deterministic.as_ref().filter(|_| cfg!(target_os = "macos"));
                if let Some(det) = macos_deterministic {
                    run_nested_deterministic_on_macos(
                        &layout,
                        cpus,
                        memory_mib,
                        snapshot_path.as_deref(),
                        det,
                        trace_events,
                        root,
                        &guest_command,
                        "run",
                        Vec::new(),
                        explicit_kernel,
                    )
                } else {
                    run_guest(
                        layout,
                        guest_command,
                        cpus,
                        memory_mib,
                        snapshot_path,
                        nested_kvm,
                        effective_no_host_shares,
                        deterministic,
                        trace_events,
                        root,
                        forwards,
                        vhost_user_fs,
                        explicit_kernel,
                    )
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InitLocalTarget {
    dest_base: PathBuf,
    preferred_source_base: Option<PathBuf>,
}

fn init_local_target(path: Option<&Path>) -> Result<Option<InitLocalTarget>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("current directory")?
            .join(path)
    };
    let dest_base = path.join(".lnx");
    let preferred_source_base = if std::env::var_os("LNX_BASE").is_none() && path.exists() {
        linked_git_worktree(&path).and_then(|worktree| {
            let source_base = worktree.main_root.join(".lnx");
            source_base.is_dir().then_some(source_base)
        })
    } else {
        None
    };
    Ok(Some(InitLocalTarget {
        dest_base,
        preferred_source_base,
    }))
}

fn run_init_command(
    layout: &Layout,
    local_target: Option<InitLocalTarget>,
    instance: &str,
    args: InitArgs,
    explicit_kernel: bool,
    explicit_rootfs: bool,
) -> Result<()> {
    if let Some(default_instance) = args.default_instance.as_deref() {
        return init_local_default_instance(layout, default_instance, args.kernel.as_deref());
    }

    if let Some(image) = args.image {
        init::ensure_base_ignored(&layout.base)?;
        return crate::oci::import_image(layout, &image, args.kernel.as_deref());
    }

    if let Some(target) = local_target
        && should_init_local_fork(
            args.kernel.as_ref(),
            args.rootfs.as_ref(),
            explicit_kernel,
            explicit_rootfs,
        )
    {
        return init_local_fork_from_base(instance, target.dest_base, target.preferred_source_base);
    }

    init::run(layout, args.kernel.as_deref(), args.rootfs.as_deref())
}

fn init_local_default_instance(
    dest: &Layout,
    default_instance: &str,
    kernel: Option<&Path>,
) -> Result<()> {
    init::ensure_base_ignored(&dest.base)?;
    if let Some(source_base) = Layout::find_instance_base(default_instance)? {
        let source = Layout::resolve_in_base(default_instance, source_base, None, None);
        if same_path(&source.base, &dest.base) {
            bail!(
                "local instance already exists: {}",
                dest.instance_dir.display()
            );
        }
        if init::instance_has_state(&source) {
            checkpoints::fork(&source, checkpoints::ForkSource::Current, dest)?;
            eprintln!(
                "init: local base {} from instance {}",
                dest.base.display(),
                default_instance
            );
            return Ok(());
        }
    }

    crate::oci::import_image(dest, default_instance, kernel)
}

/// Changes an instance's persisted settings. Settings of an instance that
/// does not exist yet apply when its first run creates it.
fn set_instance_settings(layout: &Layout, settings: &[String]) -> Result<()> {
    fs::create_dir_all(&layout.instance_dir)
        .with_context(|| format!("create {}", layout.instance_dir.display()))?;
    let config = runner::with_instance_guard(layout, |state| {
        let maintenance = matches!(
            state,
            runner::InstanceLockState::Held { lease: Some(lease) }
                if lease.role == runner::LeaseRole::Maintenance
        );
        if maintenance {
            bail!(
                "cannot change settings while instance {} has a state operation in progress",
                layout.instance
            );
        }
        let mut config = descriptor::load(layout)?;
        for setting in settings {
            let (key, value) = setting
                .split_once('=')
                .with_context(|| format!("expected KEY=VALUE, got {setting}"))?;
            match key {
                "cpus" => {
                    let cpus: u8 = value
                        .parse()
                        .with_context(|| format!("parse cpus {value}"))?;
                    if cpus == 0 {
                        bail!("cpus must be at least 1");
                    }
                    config.cpus = Some(cpus);
                }
                "memory-mib" | "memory_mib" => {
                    let memory_mib: u32 = value
                        .parse()
                        .with_context(|| format!("parse memory-mib {value}"))?;
                    if memory_mib < 256 {
                        bail!("memory-mib must be at least 256");
                    }
                    config.memory_mib = Some(memory_mib);
                }
                other => bail!("unknown setting {other} (valid: cpus, memory-mib)"),
            }
        }
        if config.name.is_none() {
            config.name = Some(layout.instance.clone());
        }
        descriptor::save(layout, &config)?;
        Ok(config)
    })?;
    println!("{}", serde_json::to_string_pretty(&config)?);
    if let Some(notice) =
        settings_pending_notice(&layout.instance, &config, latest_snapshot_shape(layout))
    {
        eprintln!("{notice}");
    }
    Ok(())
}

/// Saved settings take effect at an instance's next cold boot; while it has
/// saved memory in another shape, it keeps resuming in that shape. Says so
/// when that is the case.
fn settings_pending_notice(
    instance: &str,
    config: &descriptor::InstanceDescriptor,
    snapshot: Option<SnapshotShape>,
) -> Option<String> {
    let shape = snapshot?;
    let differs = config.cpus.is_some_and(|cpus| cpus != shape.cpus)
        || config
            .memory_mib
            .is_some_and(|memory_mib| memory_mib != shape.memory_mib);
    differs.then(|| {
        format!(
            "lnx: {instance} resumes its saved memory with {} CPUs and {} MiB; the new settings apply at its next cold boot. `lnx --instance {instance} snapshots clear` drops the saved memory so the next run boots with them.",
            shape.cpus, shape.memory_mib
        )
    })
}

fn should_init_local_fork(
    init_kernel: Option<&PathBuf>,
    init_rootfs: Option<&PathBuf>,
    explicit_kernel: bool,
    explicit_rootfs: bool,
) -> bool {
    init_kernel.is_none()
        && init_rootfs.is_none()
        && !explicit_kernel
        && !explicit_rootfs
        && std::env::var_os("LNX_BASE").is_none()
}

fn init_local_fork_from_base(
    instance: &str,
    dest_base: PathBuf,
    preferred_source_base: Option<PathBuf>,
) -> Result<()> {
    let dest = Layout::resolve_in_base(instance, dest_base, None, None);
    init::ensure_base_ignored(&dest.base)?;
    let source_base = match preferred_source_base {
        Some(source_base) if !same_path(&source_base, &dest.base) => Some(source_base),
        _ => Layout::find_instance_base(instance)?,
    };
    let source = source_base.map(|base| Layout::resolve_in_base(instance, base, None, None));
    match &source {
        Some(source) if source.base == dest.base => bail!(
            "local instance already exists: {}",
            dest.instance_dir.display()
        ),
        Some(source) if init::instance_has_state(source) => {
            checkpoints::fork(source, checkpoints::ForkSource::Current, &dest)?;
        }
        Some(source) if init_from_source_base_files(&dest, &source.base)? => {}
        _ => {
            init::run(&dest, None, None)?;
        }
    }
    eprintln!("init: local base {}", dest.base.display());
    Ok(())
}

fn init_from_source_base_files(dest: &Layout, source_base: &Path) -> Result<bool> {
    let kernel = source_base.join("vmlinuz");
    let rootfs = source_base.join("cache").join("rootfs.ext4");
    if !kernel.exists() && !rootfs.exists() {
        return Ok(false);
    }
    init::run(
        dest,
        kernel.exists().then_some(kernel.as_path()),
        rootfs.exists().then_some(rootfs.as_path()),
    )?;
    Ok(true)
}

fn maybe_auto_init_git_worktree(
    instance: &str,
    command: Option<&Command>,
    explicit_kernel: bool,
    explicit_rootfs: bool,
) -> Result<()> {
    if std::env::var_os("LNX_BASE").is_some()
        || explicit_kernel
        || explicit_rootfs
        || !command_allows_worktree_auto_init(command)
    {
        return Ok(());
    }

    let cwd = std::env::current_dir().context("current directory")?;
    let Some(worktree) = linked_git_worktree(&cwd) else {
        return Ok(());
    };
    let Some(plan) = worktree_auto_init_plan(&worktree, instance) else {
        return Ok(());
    };

    eprintln!(
        "init: git worktree {} from {}",
        plan.dest_base.display(),
        plan.source_base.display()
    );
    init_local_fork_from_base(instance, plan.dest_base, Some(plan.source_base))
}

fn command_allows_worktree_auto_init(command: Option<&Command>) -> bool {
    match command {
        Some(Command::Init(_))
        | Some(Command::Ingress(_))
        | Some(Command::HiddenIngress(_))
        | Some(Command::HiddenOciBuild(_))
        | Some(Command::HiddenSparseCopy(_))
        | Some(Command::HiddenVmOwner(_)) => false,
        Some(Command::Server(args)) => args.command.is_some(),
        _ => true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkedGitWorktree {
    main_root: PathBuf,
    current_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeAutoInitPlan {
    dest_base: PathBuf,
    source_base: PathBuf,
}

fn linked_git_worktree(cwd: &Path) -> Option<LinkedGitWorktree> {
    let current_root = git_toplevel(cwd)?;
    let output = ProcessCommand::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_git_worktree_list(&text, &current_root)
}

fn git_toplevel(cwd: &Path) -> Option<PathBuf> {
    let output = ProcessCommand::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn parse_git_worktree_list(output: &str, current_root: &Path) -> Option<LinkedGitWorktree> {
    let roots = output
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    linked_git_worktree_from_roots(&roots, current_root)
}

fn linked_git_worktree_from_roots(
    roots: &[PathBuf],
    current_root: &Path,
) -> Option<LinkedGitWorktree> {
    if roots.len() < 2 {
        return None;
    }
    let main_root = roots.first()?.clone();
    let current_root = roots
        .iter()
        .find(|root| same_path(root, current_root))?
        .clone();
    if same_path(&main_root, &current_root) {
        return None;
    }
    Some(LinkedGitWorktree {
        main_root,
        current_root,
    })
}

fn worktree_auto_init_plan(
    worktree: &LinkedGitWorktree,
    instance: &str,
) -> Option<WorktreeAutoInitPlan> {
    let source_base = worktree.main_root.join(".lnx");
    if !source_base.is_dir() {
        return None;
    }
    let dest_base = worktree.current_root.join(".lnx");
    if same_path(&source_base, &dest_base) || dest_base.join("instances").join(instance).is_dir() {
        return None;
    }
    Some(WorktreeAutoInitPlan {
        dest_base,
        source_base,
    })
}

fn same_path(a: &Path, b: &Path) -> bool {
    let normalize = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    normalize(a) == normalize(b)
}

fn inspect_instance(layout: &Layout, cpus: u8, memory_mib: u32) -> Result<()> {
    let config = descriptor::load(layout)?;
    runner::ensure_store(layout)?;
    let store = store::Store::new(&layout.instance_dir);
    let latest = store.latest()?;
    let record = store.record()?;
    let checkpoints = store.checkpoints()?.len();
    let inspect = serde_json::json!({
        "name": layout.instance,
        "state": status::instance_state(layout),
        "pids": status::instance_pids(layout),
        "cpus": cpus,
        "memory_mib": memory_mib,
        "created": config.created,
        "image": config.image,
        "settings": config,
        "rootfs": latest.as_ref().map(|latest| latest.rootfs()),
        "rootfs_size_bytes": latest.as_ref().and_then(|latest| file_len(&latest.rootfs())),
        "rootfs_allocated_bytes": latest
            .as_ref()
            .and_then(|latest| allocated_bytes(&latest.rootfs())),
        "generation": latest.as_ref().map(|latest| latest.id().to_string()),
        "phase": record.map(|record| record.phase),
        "snapshot": latest.as_ref().filter(|latest| latest.manifest.has_memory()).map(|latest| {
            serde_json::json!({
                "path": latest.dir,
                "pages_allocated_bytes": allocated_bytes(&latest.dir.join(store::PAGES)),
            })
        }),
        "checkpoints": checkpoints,
        "descriptor": descriptor::path(layout),
        "logs": {
            "run": layout.run_dir.join("lnx.log"),
            "console": layout.console_log,
            "owner": layout.run_dir.join("owner.log"),
        },
    });
    println!("{}", serde_json::to_string_pretty(&inspect)?);
    Ok(())
}

fn file_len(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|meta| meta.len())
}

fn allocated_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|meta| meta.blocks() * 512)
}

fn print_instance_logs(layout: &Layout, console: bool, owner: bool) -> Result<()> {
    let path = if console {
        layout.console_log.clone()
    } else if owner {
        layout.run_dir.join("owner.log")
    } else {
        layout.run_dir.join("lnx.log")
    };
    let mut file = fs::File::open(&path)
        .with_context(|| format!("open {} (has the instance been started?)", path.display()))?;
    std::io::copy(&mut file, &mut std::io::stdout()).context("print log")?;
    Ok(())
}

fn list_instances(base: &Path, json: bool) -> Result<()> {
    let mut names = BTreeSet::new();
    collect_child_dir_names(&base.join("instances"), &mut names)?;

    let mut instances = names
        .into_iter()
        .map(|name| {
            let layout = Layout::resolve_in_base(&name, base.to_path_buf(), None, None);
            let state = status::instance_state(&layout);
            let pids = status::instance_pids(&layout);
            Ok(InstanceRow { name, state, pids })
        })
        .collect::<Result<Vec<_>>>()?;
    instances.sort_by_key(|row| (row.state, row.name.clone()));

    if json {
        println!("{}", serde_json::to_string_pretty(&instances)?);
        return Ok(());
    }
    println!("{:<36} {:<12} PIDS", "NAME", "STATE");
    for row in instances {
        let pids = row
            .pids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        println!("{:<36} {:<12} {pids}", row.name, row.state);
    }
    Ok(())
}

fn delete_instance(base: &Path, name: &str) -> Result<()> {
    if !crate::paths::is_instance_dir_name(name) {
        bail!("invalid instance name {name:?}");
    }
    let layout = Layout::resolve_in_base(name, base.to_path_buf(), None, None);
    delete_resolved_instance(base, name, &layout)
}

fn delete_resolved_instance(base: &Path, name: &str, layout: &Layout) -> Result<()> {
    let persistent_root = base.join("instances");
    let run_is_persistent =
        paths_refer_to_same_existing_entry(&layout.run_dir, &layout.instance_dir);
    let split_run_root = (!run_is_persistent)
        .then(|| {
            layout
                .run_dir
                .parent()
                .map(Path::to_path_buf)
                .context("split instance run directory has no instances parent")
        })
        .transpose()?;
    let mut stale_trash = find_detached_instance_state(&persistent_root, name)?;
    if let Some(run_root) = &split_run_root {
        stale_trash.extend(find_detached_instance_state(run_root, name)?);
    }
    if !layout.instance_dir.exists() && !layout.run_dir.exists() && stale_trash.is_empty() {
        bail!("instance not found: {name}");
    }
    if status::instance_state(layout).is_active() {
        terminate_instance_owner(layout)?;
    }

    let detached = runner::with_exclusive_instance_state(layout, |_, _| {
        let mut planned = Vec::new();
        if let Some(plan) =
            plan_contained_instance_detach(&layout.instance_dir, &persistent_root, name)?
        {
            planned.push(plan);
        }
        if let Some(run_root) = &split_run_root
            && let Some(plan) = plan_contained_instance_detach(&layout.run_dir, run_root, name)?
        {
            planned.push(plan);
        }
        detach_planned_instance_dirs(planned)
    })?;
    let Some(detached) = detached else {
        bail!(
            "instance {name} became busy while deletion was being reserved; stop it or wait for the state copy to finish"
        );
    };
    stale_trash.extend(detached);
    for path in stale_trash {
        remove_path_if_exists(&path)
            .with_context(|| format!("clean up detached instance state at {}", path.display()))?;
    }

    println!("deleted {name}");
    Ok(())
}

fn terminate_instance_owner(layout: &Layout) -> Result<()> {
    let Some(owner) = runner::live_owner(layout) else {
        return Ok(());
    };
    let owner = owner.process;
    let owner_alive = || runner::live_owner(layout).is_some_and(|lease| lease.process == owner);

    owner.signal_group(libc::SIGTERM)?;
    let term_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < term_deadline {
        if !owner_alive() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }

    owner.signal_group(libc::SIGKILL)?;
    let kill_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < kill_deadline {
        if !owner_alive() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }

    bail!(
        "owner process {} for instance {} did not exit after SIGTERM/SIGKILL; refusing to delete a live instance",
        owner.pid,
        layout.instance
    );
}

fn validate_contained_instance_dir(dir: &Path, instances_root: &Path, name: &str) -> Result<()> {
    let is_contained = dir.parent() == Some(instances_root)
        && dir.file_name().and_then(|n| n.to_str()) == Some(name);
    if !is_contained {
        bail!(
            "refusing to delete instance dir outside {}: {}",
            instances_root.display(),
            dir.display()
        );
    }
    Ok(())
}

fn paths_refer_to_same_existing_entry(left: &Path, right: &Path) -> bool {
    left == right
        || fs::canonicalize(left)
            .ok()
            .zip(fs::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

fn find_detached_instance_state(instances_root: &Path, name: &str) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for transaction_root in instance_transaction_roots(instances_root)? {
        let delete_root = transaction_root.join("delete").join(name);
        let entries = match fs::read_dir(&delete_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", delete_root.display()));
            }
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("read {}", delete_root.display()))?;
            if entry.file_type()?.is_dir() {
                paths.push(entry.path());
            }
        }
    }
    Ok(paths)
}

fn plan_contained_instance_detach(
    dir: &Path,
    instances_root: &Path,
    name: &str,
) -> Result<Option<(PathBuf, PathBuf, PathBuf)>> {
    validate_contained_instance_dir(dir, instances_root, name)?;
    match fs::symlink_metadata(dir) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("stat {}", dir.display())),
    }
    let transaction_root = ensure_instance_transaction_root(instances_root)?;
    let delete_root = transaction_root.join("delete").join(name);
    fs::create_dir_all(&delete_root)
        .with_context(|| format!("create {}", delete_root.display()))?;
    let mut attempt = 0_u64;
    let transaction = loop {
        let candidate = delete_root.join(format!("{}-{attempt}", std::process::id()));
        match fs::create_dir(&candidate) {
            Ok(()) => break candidate,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                attempt = attempt.saturating_add(1);
            }
            Err(error) => {
                return Err(error).with_context(|| format!("create {}", candidate.display()));
            }
        }
    };
    let detached_state = transaction.join("state");
    Ok(Some((dir.to_path_buf(), transaction, detached_state)))
}

fn detach_planned_instance_dirs(planned: Vec<(PathBuf, PathBuf, PathBuf)>) -> Result<Vec<PathBuf>> {
    let mut detached = Vec::<(PathBuf, PathBuf, PathBuf)>::new();
    for (original, transaction, detached_state) in planned {
        if let Err(error) = fs::rename(&original, &detached_state) {
            let _ = fs::remove_dir_all(&transaction);
            for (moved_original, moved_transaction, moved_state) in detached.iter().rev() {
                if let Err(rollback_error) = fs::rename(moved_state, moved_original) {
                    bail!(
                        "detach {} for deletion: {error}; rollback {} to {}: {rollback_error}",
                        original.display(),
                        moved_state.display(),
                        moved_original.display()
                    );
                }
                let _ = fs::remove_dir_all(moved_transaction);
            }
            return Err(error)
                .with_context(|| format!("detach {} for deletion", original.display()));
        }
        detached.push((original, transaction, detached_state));
    }
    Ok(detached
        .into_iter()
        .map(|(_, transaction, _)| transaction)
        .collect())
}

/// Removes `dir`, but only if it is exactly `<instances_root>/<name>`. Never
/// deletes anything outside that shape, even if callers pass a mismatched
/// `instances_root`/`name` pair.
#[cfg(test)]
fn remove_contained_instance_dir(dir: &Path, instances_root: &Path, name: &str) -> Result<()> {
    validate_contained_instance_dir(dir, instances_root, name)?;
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", dir.display())),
    }
}

#[derive(serde::Serialize)]
struct InstanceRow {
    name: String,
    state: status::InstanceState,
    pids: Vec<i32>,
}

fn collect_child_dir_names(parent: &Path, names: &mut BTreeSet<String>) -> Result<()> {
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("read {}", parent.display())),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() && !is_instance_transaction_root(&entry.path()) {
            names.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_guest(
    layout: Layout,
    command: Vec<String>,
    cpus: u8,
    memory_mib: u32,
    snapshot_path: Option<PathBuf>,
    nested_kvm: bool,
    no_host_shares: bool,
    deterministic: Option<runner::DeterministicConfig>,
    trace_events: bool,
    run_as_root: bool,
    forwards: Vec<runner::PortForward>,
    vhost_user_fs: Vec<runner::VhostUserFsMount>,
    explicit_kernel: bool,
) -> Result<()> {
    ensure_image_and_instance(&layout, explicit_kernel)?;

    // An empty command means "login shell"; the agent resolves which shell
    // the image actually ships.
    if command.first().map(String::as_str) == Some("cp")
        && command.iter().any(|arg| is_host_path(arg))
    {
        if deterministic.is_some() {
            bail!("--deterministic cannot copy host paths into or out of the guest");
        }
        copy_between_host_and_guest(
            &layout,
            &command[1..],
            ChildVmConfig {
                cpus,
                memory_mib,
                nested_kvm,
            },
            explicit_kernel.then_some(layout.kernel.as_path()),
            layout.rootfs.as_deref(),
        )?;
        return Ok(());
    }
    let cwd = std::env::current_dir().context("current directory")?;

    let config = runner::RunConfig {
        layout,
        command,
        cwd,
        cpus,
        memory_mib,
        nested_kvm,
        restore_snapshot: snapshot_path,
        forwards,
        run_as_root,
        no_host_shares,
        vhost_user_fs,
        reuse_owner: true,
        deterministic,
        trace_events,
    };

    let status = runner::run(config)?;
    std::process::exit(status);
}

fn run_fs_unshare(layout: &Layout, args: FsUnshareArgs) -> Result<()> {
    runner::ensure_store(layout)?;
    let state = store::Store::new(&layout.instance_dir)
        .current_host_share_state()?
        .unwrap_or_else(|| host_share::state_root(&layout.instance_dir));
    let cwd = std::env::current_dir().context("current directory")?;
    if args.list {
        let entries = host_share::list_state_entries(&state, &cwd)?;
        if entries.is_empty() {
            println!(
                "no host-share copy-on-write state for instance {}",
                layout.instance
            );
            return Ok(());
        }
        for entry in entries {
            println!(
                "{}\t{}\tshare={}\tstate={}",
                entry.kind.as_str(),
                entry.logical_path.display(),
                entry.tag,
                entry.state_path.display()
            );
        }
        return Ok(());
    }

    let Some(path) = args.path else {
        bail!("fs unshare requires PATH unless --list is used");
    };
    let targets = host_share::targets_for_path(&path, &cwd)?;
    if targets.is_empty() {
        bail!("path is not on a host share: {}", path.display());
    }
    if !args.remove {
        for target in targets {
            let path_state = host_share::path_state(&state, &target)?;
            print_host_share_path_state(&path_state);
        }
        return Ok(());
    }

    let cleared = runner::with_exclusive_instance_state(layout, |lock, _| {
        let store = store::Store::new(&layout.instance_dir);
        store.recover(lock)?;
        store.derive(lock, |staging| {
            let state = staging.join(store::HOST_SHARE_STATE);
            for target in &targets {
                host_share::remove_path_state(&state, target)?;
            }
            Ok(())
        })
    })?;
    if cleared.is_none() {
        bail!(
            "cannot remove host-share state while instance {} is running or another state operation is in progress",
            layout.instance
        );
    }
    for target in targets {
        println!("cleared {}", target.absolute.display());
    }
    Ok(())
}

fn print_host_share_path_state(state: &host_share::PathState) {
    let status = if let Some(covering) = &state.covering_whiteout {
        format!(
            "hidden by {}",
            state.target.share_root.join(covering).display()
        )
    } else if state.upper_exists {
        "copied".to_string()
    } else if state.descendant_state {
        "descendant-state".to_string()
    } else {
        "clean".to_string()
    };
    println!("path: {}", state.target.absolute.display());
    println!("share: {}", state.target.tag);
    println!("rule: gitignored writes are isolated from the host with copy-on-write");
    if let Some(reason) = git_ignore_reason(&state.target.absolute) {
        println!("match: {reason}");
    }
    println!("state: {status}");
    println!("upper: {}", state.upper_path.display());
    println!("whiteout: {}", state.whiteout_path.display());
    if let Some(covering) = &state.covering_whiteout {
        println!(
            "restore: lnx fs unshare --remove {}",
            state.target.share_root.join(covering).display()
        );
    } else {
        println!(
            "clear: lnx fs unshare --remove {}",
            state.target.absolute.display()
        );
    }
}

fn git_ignore_reason(path: &Path) -> Option<String> {
    let output = ProcessCommand::new("git")
        .arg("check-ignore")
        .arg("-v")
        .arg("--")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().next().map(str::to_string)
}

fn ensure_image_and_instance(layout: &Layout, explicit_kernel: bool) -> Result<()> {
    if !layout.kernel.exists() {
        if explicit_kernel {
            bail!("missing kernel: {}", layout.kernel.display());
        }
        eprintln!("first run: kernel missing, initializing lnx image files");
        init::ensure_kernel(layout).context("auto-init kernel")?;
    }
    if init::instance_has_state(layout) {
        return Ok(());
    }
    if let Some(rootfs) = &layout.rootfs {
        if !rootfs.exists() {
            bail!("missing rootfs: {}", rootfs.display());
        }
        return init::ensure_instance_from(layout, rootfs).context("create instance from rootfs");
    }
    eprintln!("first run: creating instance {}", layout.instance);
    init::run(layout, None, None).context("auto-init")
}

fn validate_deterministic_args(
    nested_kvm: bool,
    forwards: &[runner::PortForward],
    vhost_user_fs: &[runner::VhostUserFsMount],
    deterministic: Option<&runner::DeterministicConfig>,
    trace_events: bool,
) -> Result<()> {
    if trace_events && deterministic.is_none() {
        bail!("--trace-events requires --deterministic");
    }
    if deterministic.is_none() {
        return Ok(());
    }
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        bail!("--deterministic is only supported on the KVM backend");
    }
    if cfg!(target_os = "linux") && nested_kvm {
        bail!("--deterministic cannot be combined with --nested-kvm yet");
    }
    if !forwards.is_empty() {
        bail!("--deterministic cannot be combined with --forward yet");
    }
    if !vhost_user_fs.is_empty() {
        bail!("--deterministic cannot be combined with --vhost-user-fs yet");
    }
    Ok(())
}

/// The VM shape an instance's latest memory snapshot was taken with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SnapshotShape {
    cpus: u8,
    memory_mib: u32,
    nested_kvm: bool,
    no_host_shares: bool,
}

/// Restoring a snapshot needs the shape it was taken with, so a command that
/// asks for no particular CPUs, memory, nested virtualization or host shares
/// resumes the instance however it was booted, instead of failing with a
/// mismatch against defaults or saved settings.
fn latest_snapshot_shape(layout: &Layout) -> Option<SnapshotShape> {
    let latest = store::Store::new(&layout.instance_dir).latest().ok()??.dir;
    let config = runner::snapshot_vm_config(&latest).ok()??;
    let metadata = runner::read_launch_metadata(&latest).ok();
    Some(SnapshotShape {
        cpus: u8::try_from(config.vcpu_count).ok()?,
        memory_mib: u32::try_from(config.memory_mib()).ok()?,
        nested_kvm: metadata
            .as_ref()
            .is_some_and(|metadata| metadata.owner_args.iter().any(|arg| arg == "--nested-kvm")),
        no_host_shares: metadata.is_some_and(|metadata| metadata.shares.no_host_shares),
    })
}

fn effective_cpus(configured: u8, deterministic: Option<&runner::DeterministicConfig>) -> u8 {
    if deterministic.is_some() {
        1
    } else {
        configured
    }
}

#[allow(clippy::too_many_arguments)]
fn run_nested_deterministic_on_macos(
    layout: &Layout,
    cpus: u8,
    memory_mib: u32,
    snapshot_path: Option<&Path>,
    deterministic: &runner::DeterministicConfig,
    trace_events: bool,
    run_as_root: bool,
    guest_command: &[String],
    command_label: &str,
    subcommand: Vec<String>,
    explicit_kernel: bool,
) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("nested deterministic dispatch is only available on macOS");
    }

    let linux_lnx = find_linux_lnx_binary(&layout.base)?;
    let outer_instance = nested_deterministic_outer_instance(&layout.instance);
    let outer_layout = Layout::resolve(&outer_instance, Some(layout.kernel.clone()), None)?;
    ensure_image_and_instance(&outer_layout, explicit_kernel)?;

    let inner_args = nested_deterministic_inner_args(
        layout,
        cpus,
        memory_mib,
        snapshot_path,
        deterministic,
        trace_events,
        run_as_root,
        guest_command,
        subcommand,
    );
    let script = nested_deterministic_script(
        &linux_lnx,
        &layout.base,
        std::env::var_os("LNX_RUN_BASE")
            .map(PathBuf::from)
            .as_deref(),
        &inner_args,
    );
    let cwd = std::env::current_dir().context("current directory")?;
    let status = runner::run(runner::RunConfig {
        layout: outer_layout,
        command: vec!["bash".to_string(), "-lc".to_string(), script],
        cwd,
        cpus: 2,
        memory_mib: memory_mib.max(DEFAULT_MEMORY_MIB),
        nested_kvm: true,
        restore_snapshot: None,
        forwards: Vec::new(),
        run_as_root: false,
        no_host_shares: false,
        vhost_user_fs: Vec::new(),
        reuse_owner: true,
        deterministic: None,
        trace_events: false,
    })
    .with_context(|| format!("run deterministic {command_label} in nested Linux"))?;
    std::process::exit(status);
}

fn nested_deterministic_outer_instance(instance: &str) -> String {
    format!("{instance}-deterministic-outer")
}

#[allow(clippy::too_many_arguments)]
fn nested_deterministic_inner_args(
    layout: &Layout,
    cpus: u8,
    memory_mib: u32,
    snapshot_path: Option<&Path>,
    deterministic: &runner::DeterministicConfig,
    trace_events: bool,
    run_as_root: bool,
    guest_command: &[String],
    subcommand: Vec<String>,
) -> Vec<String> {
    let mut args = vec![
        "--instance".to_string(),
        layout.instance.clone(),
        "--kernel".to_string(),
        layout.kernel.display().to_string(),
        "--cpus".to_string(),
        cpus.to_string(),
        "--memory-mib".to_string(),
        memory_mib.to_string(),
        "--no-host-shares".to_string(),
        "--deterministic".to_string(),
        deterministic.seed.clone(),
    ];
    if let Some(snapshot) = snapshot_path {
        args.push("--snapshot".to_string());
        args.push(snapshot.display().to_string());
    }
    if trace_events {
        args.push("--trace-events".to_string());
    }
    if run_as_root {
        args.push("--root".to_string());
    }
    args.extend(subcommand);
    args.extend(guest_command.iter().cloned());
    args
}

fn nested_deterministic_script(
    linux_lnx: &Path,
    base: &Path,
    run_base: Option<&Path>,
    inner_args: &[String],
) -> String {
    let mut lines = vec![
        "set -euo pipefail".to_string(),
        "test -c /dev/kvm".to_string(),
        "test -r /dev/kvm".to_string(),
        "nested_tools=/tmp/lnx-deterministic-tools".to_string(),
        "rm -rf \"$nested_tools\"".to_string(),
        "mkdir -p \"$nested_tools\"".to_string(),
        format!(
            "cp {} \"$nested_tools/lnx\"",
            shell_quote(&linux_lnx.display().to_string())
        ),
        "chmod +x \"$nested_tools\"/*".to_string(),
        "export LNX_BIN=\"$nested_tools/lnx\"".to_string(),
        format!(
            "export LNX_BASE={}",
            shell_quote(&base.display().to_string())
        ),
    ];
    if let Some(run_base) = run_base {
        lines.push(format!(
            "export LNX_RUN_BASE={}",
            shell_quote(&run_base.display().to_string())
        ));
    }
    let inner = inner_args
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    lines.push(format!("exec \"$LNX_BIN\" {inner}"));
    lines.join("\n")
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn find_linux_lnx_binary(base: &Path) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("LNX_LINUX_BIN").map(PathBuf::from) {
        return require_executable_file(path, "Linux lnx binary");
    }
    let exe = std::env::current_exe().context("current executable")?;
    for candidate in linux_lnx_candidates(&exe) {
        if candidate.exists() {
            return require_executable_file(candidate, "Linux lnx binary");
        }
    }
    let cache_path = base.join("cache").join("lnx-linux-aarch64");
    crate::init::ensure_nested_linux_lnx(&cache_path)?;
    require_executable_file(cache_path, "Linux lnx binary")
}

fn linux_lnx_candidates(current_exe: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = current_exe.parent() {
        candidates.push(dir.join("lnx-linux-aarch64"));
    }
    let mut cursor = current_exe.parent();
    while let Some(dir) = cursor {
        if dir.file_name().and_then(|name| name.to_str()) == Some("target") {
            candidates.push(
                dir.join("aarch64-unknown-linux-musl")
                    .join("debug")
                    .join("lnx"),
            );
            candidates.push(
                dir.join("aarch64-unknown-linux-musl")
                    .join("release")
                    .join("lnx"),
            );
            break;
        }
        if matches!(
            dir.file_name().and_then(|name| name.to_str()),
            Some("debug" | "release")
        ) && dir
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str())
            == Some("target")
        {
            let profile = dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("debug");
            if let Some(target_dir) = dir.parent() {
                candidates.push(
                    target_dir
                        .join("aarch64-unknown-linux-musl")
                        .join(profile)
                        .join("lnx"),
                );
            }
        }
        cursor = dir.parent();
    }
    candidates
}

fn require_executable_file(path: PathBuf, label: &str) -> Result<PathBuf> {
    if path.is_file() {
        Ok(path)
    } else {
        bail!("{label} not found: {}", path.display())
    }
}

fn copy_between_host_and_guest(
    layout: &Layout,
    args: &[String],
    vm_config: ChildVmConfig,
    explicit_kernel: Option<&Path>,
    explicit_rootfs: Option<&Path>,
) -> Result<()> {
    let operands = cp_transfer_operands(args)?;
    if operands.len() < 2 {
        bail!("usage: lnx cp host:SOURCE... GUEST_DIR or lnx cp GUEST_SOURCE... host:DEST_DIR");
    }
    let host_flags = operands
        .iter()
        .map(|arg| is_host_path(arg))
        .collect::<Vec<_>>();
    let dest_is_host = *host_flags.last().unwrap_or(&false);
    let sources_are_host = host_flags[..host_flags.len() - 1]
        .iter()
        .all(|value| *value);
    let sources_are_guest = host_flags[..host_flags.len() - 1]
        .iter()
        .all(|value| !*value);

    match (sources_are_host, dest_is_host, sources_are_guest) {
        (true, false, _) => copy_host_to_guest(
            layout,
            &operands,
            vm_config,
            explicit_kernel,
            explicit_rootfs,
        ),
        (false, true, true) => copy_guest_to_host(
            layout,
            &operands,
            vm_config,
            explicit_kernel,
            explicit_rootfs,
        ),
        _ => bail!(
            "host transfers must copy only host: sources to one guest destination, or only guest sources to one host: destination"
        ),
    }
}

fn cp_transfer_operands(args: &[String]) -> Result<Vec<String>> {
    let mut operands = Vec::new();
    let mut parsing_options = true;
    for arg in args {
        if parsing_options && arg == "--" {
            parsing_options = false;
            continue;
        }
        if parsing_options && arg.starts_with('-') && arg != "-" {
            if is_supported_cp_transfer_option(arg) {
                continue;
            }
            bail!("lnx cp host transfers support only -R, -r, and -a");
        }
        parsing_options = false;
        operands.push(arg.clone());
    }
    Ok(operands)
}

fn is_supported_cp_transfer_option(arg: &str) -> bool {
    matches!(arg, "-R" | "-r" | "-a") || {
        arg.starts_with('-')
            && arg.len() > 1
            && arg[1..].chars().all(|c| matches!(c, 'R' | 'r' | 'a'))
    }
}

fn is_host_path(value: &str) -> bool {
    value.starts_with("host:")
}

fn strip_host_prefix(value: &str) -> Result<&str> {
    value
        .strip_prefix("host:")
        .filter(|path| !path.is_empty())
        .context("host: path must include a path after the colon")
}

fn copy_host_to_guest(
    layout: &Layout,
    args: &[String],
    vm_config: ChildVmConfig,
    explicit_kernel: Option<&Path>,
    explicit_rootfs: Option<&Path>,
) -> Result<()> {
    let guest_dest = args.last().context("missing guest destination")?;
    let mut tar_args = vec!["-cf".to_string(), "-".to_string()];
    for source in &args[..args.len() - 1] {
        let source = PathBuf::from(strip_host_prefix(source)?);
        let parent = source.parent().unwrap_or_else(|| Path::new("."));
        let name = source
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("host source has no file name: {}", source.display()))?;
        tar_args.push("-C".to_string());
        tar_args.push(parent.display().to_string());
        tar_args.push(name.to_string());
    }
    let tar_output = ProcessCommand::new("tar")
        .args(&tar_args)
        .output()
        .context("archive host sources with tar")?;
    if !tar_output.status.success() {
        std::io::stderr().write_all(&tar_output.stderr)?;
        bail!("host tar failed");
    }
    run_lnx_child(
        layout,
        explicit_kernel,
        explicit_rootfs,
        Some(vm_config),
        &[
            "sh",
            "-lc",
            "mkdir -p \"$1\" && tar -C \"$1\" -xf -",
            "lnx-cp",
            guest_dest,
        ],
        Some(&tar_output.stdout),
        false,
    )
    .context("extract archive in guest")?;
    Ok(())
}

fn copy_guest_to_host(
    layout: &Layout,
    args: &[String],
    vm_config: ChildVmConfig,
    explicit_kernel: Option<&Path>,
    explicit_rootfs: Option<&Path>,
) -> Result<()> {
    let host_dest = PathBuf::from(strip_host_prefix(
        args.last().context("missing host destination")?,
    )?);
    std::fs::create_dir_all(&host_dest)
        .with_context(|| format!("create host destination {}", host_dest.display()))?;
    let mut guest_tar_args = vec!["tar".to_string(), "-cf".to_string(), "-".to_string()];
    guest_tar_args.extend(args[..args.len() - 1].iter().cloned());
    let archive = run_lnx_child(
        layout,
        explicit_kernel,
        explicit_rootfs,
        Some(vm_config),
        &guest_tar_args
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        None,
        true,
    )
    .context("archive guest sources")?;
    let mut tar = ProcessCommand::new("tar")
        .arg("-C")
        .arg(&host_dest)
        .arg("-xf")
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .context("start host tar extract")?;
    tar.stdin
        .as_mut()
        .context("open host tar stdin")?
        .write_all(&archive)?;
    drop(tar.stdin.take());
    let status = tar.wait().context("wait for host tar extract")?;
    if !status.success() {
        bail!("host tar extract failed with status {status}");
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct ChildVmConfig {
    cpus: u8,
    memory_mib: u32,
    nested_kvm: bool,
}

fn run_lnx_child(
    layout: &Layout,
    explicit_kernel: Option<&Path>,
    explicit_rootfs: Option<&Path>,
    vm_config: Option<ChildVmConfig>,
    command: &[&str],
    stdin: Option<&[u8]>,
    capture_stdout: bool,
) -> Result<Vec<u8>> {
    let exe = std::env::current_exe().context("current executable")?;
    let mut child = ProcessCommand::new(exe);
    child.arg("--instance").arg(&layout.instance);
    if let Some(kernel) = explicit_kernel {
        child.arg("--kernel").arg(kernel);
    }
    if let Some(rootfs) = explicit_rootfs {
        child.arg("--rootfs").arg(rootfs);
    }
    if let Some(config) = vm_config {
        child
            .arg("--cpus")
            .arg(config.cpus.to_string())
            .arg("--memory-mib")
            .arg(config.memory_mib.to_string());
        if config.nested_kvm {
            child.arg("--nested-kvm");
        }
    }
    child.args(command);
    child.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    child.stdout(if capture_stdout {
        Stdio::piped()
    } else {
        Stdio::inherit()
    });
    child.stderr(Stdio::inherit());
    let mut child = child.spawn().context("spawn lnx child")?;
    if let Some(stdin) = stdin {
        child
            .stdin
            .as_mut()
            .context("open lnx child stdin")?
            .write_all(stdin)?;
        drop(child.stdin.take());
    }
    if capture_stdout {
        let output = child.wait_with_output().context("wait for lnx child")?;
        if !output.status.success() {
            bail!("lnx child failed with status {}", output.status);
        }
        Ok(output.stdout)
    } else {
        let status = child.wait().context("wait for lnx child")?;
        if !status.success() {
            bail!("lnx child failed with status {status}");
        }
        Ok(Vec::new())
    }
}

fn create_checkpoint(layout: &Layout, name: Option<&str>) -> Result<()> {
    require_instance(layout)?;
    let checkpoint = checkpoints::create(layout, name)?;
    println!("{}", checkpoint.name.as_deref().unwrap_or(&checkpoint.id));
    Ok(())
}

fn list_checkpoints(layout: &Layout, json: bool) -> Result<()> {
    let checkpoints = checkpoints::list(layout)?;
    if json {
        let rows: Vec<_> = checkpoints
            .iter()
            .map(|checkpoint| {
                serde_json::json!({
                    "id": checkpoint.id,
                    "name": checkpoint.name,
                    "created": checkpoints::display_time(checkpoint.created_unix),
                    "generation": checkpoint.generation.to_string(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    for checkpoint in checkpoints {
        match checkpoint.name.as_deref() {
            Some(name) => println!(
                "{}\t{}\t{}",
                checkpoint.id,
                name,
                checkpoints::display_time(checkpoint.created_unix)
            ),
            None => println!(
                "{}\t{}",
                checkpoint.id,
                checkpoints::display_time(checkpoint.created_unix)
            ),
        }
    }
    Ok(())
}

fn delete_checkpoint(layout: &Layout, identifier: &str) -> Result<()> {
    let checkpoint = checkpoints::resolve(layout, identifier)?;
    checkpoints::delete(layout, &checkpoint)?;
    println!("deleted {}", checkpoint.id);
    Ok(())
}

fn run_snapshots_command(layout: &Layout, args: SnapshotsArgs) -> Result<()> {
    match args.command {
        SnapshotsCommand::Clear => drop_saved_memory(layout),
    }
}

/// Resolves a VM run that crashed after serving commands: keep its disk or
/// return to the last saved state. Without a choice, explains both.
fn recover_instance(layout: &Layout, args: &RecoverArgs) -> Result<()> {
    require_instance(layout)?;
    runner::ensure_store(layout)?;
    let outcome = runner::with_exclusive_instance_state(layout, |lock, _| {
        let store = store::Store::new(&layout.instance_dir);
        let Some(run) = store.crashed_run()? else {
            return Ok(None);
        };
        if args.keep {
            store.salvage(lock)?;
            Ok(Some(format!(
                "kept the disk of the crashed VM (run {run}); its memory is lost and the next run boots"
            )))
        } else if args.discard {
            store.discard_run(lock)?;
            Ok(Some(format!(
                "discarded the crashed VM (run {run}); the next run resumes the last saved state"
            )))
        } else {
            Err(store
                .crashed()?
                .context("crashed run disappeared under the instance lock")?
                .into())
        }
    })?;
    match outcome {
        None => bail!(
            "instance {} is running or busy; recovery is only needed after a crash",
            layout.instance
        ),
        Some(None) => println!("instance {} needs no recovery", layout.instance),
        Some(Some(message)) => println!("{message}"),
    }
    Ok(())
}

/// Drops the instance's saved memory so the next run boots from its saved
/// disk. The disk itself is never discarded.
fn drop_saved_memory(layout: &Layout) -> Result<()> {
    require_instance(layout)?;
    runner::ensure_store(layout)?;
    let dropped = runner::with_exclusive_instance_state(layout, |lock, _| {
        let store = store::Store::new(&layout.instance_dir);
        store.recover(lock)?;
        store.drop_memory(lock)
    })?;
    match dropped {
        None => bail!(
            "cannot drop the saved memory while instance {} is running; stop it first",
            layout.instance
        ),
        Some(Some(_)) => println!(
            "dropped the saved memory of {}; the next run boots from its saved disk",
            layout.instance
        ),
        Some(None) => println!("{} has no saved memory", layout.instance),
    }
    Ok(())
}

/// Commands about an instance's saved state never create it.
fn require_instance(layout: &Layout) -> Result<()> {
    if init::instance_has_state(layout) {
        return Ok(());
    }
    bail!(
        "no instance named {}; running a command in it creates it",
        layout.instance
    )
}

/// Forks `source` (its current state, or one of its checkpoints) into a new
/// instance named `instance` in the same base.
fn fork_checkpoint(source: Layout, checkpoint: Option<&str>, instance: &str) -> Result<()> {
    crate::paths::validate_instance_name(instance)?;
    require_instance(&source)?;
    let checkpoint = checkpoint
        .map(|checkpoint| checkpoints::resolve(&source, checkpoint))
        .transpose()?;
    let from = match &checkpoint {
        Some(checkpoint) => checkpoints::ForkSource::Checkpoint(checkpoint),
        None => checkpoints::ForkSource::Current,
    };
    let dest = Layout::resolve_in_base(instance, source.base.clone(), None, None);
    init::ensure_base_ignored(&dest.base)?;
    checkpoints::fork(&source, from, &dest)?;
    println!("{instance}");
    Ok(())
}

fn parse_port_forward(value: &str) -> Result<runner::PortForward, String> {
    let parts = value.split(':').collect::<Vec<_>>();
    match parts.as_slice() {
        [listen_port, guest_port] => Ok(runner::PortForward {
            listen_host: "127.0.0.1".to_string(),
            listen_port: parse_port(listen_port)?,
            guest_host: "127.0.0.1".to_string(),
            guest_port: parse_port(guest_port)?,
        }),
        [listen_host, listen_port, guest_host, guest_port] => Ok(runner::PortForward {
            listen_host: (*listen_host).to_string(),
            listen_port: parse_port(listen_port)?,
            guest_host: (*guest_host).to_string(),
            guest_port: parse_port(guest_port)?,
        }),
        _ => Err("expected HOSTPORT:GUESTPORT or LISTEN_HOST:HOSTPORT:GUEST_HOST:GUESTPORT".into()),
    }
}

fn parse_port(value: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .map_err(|_| format!("invalid port: {value}"))
}

fn parse_vhost_user_fs_mount(value: &str) -> Result<runner::VhostUserFsMount, String> {
    let mut tag = None;
    let mut mountpoint = None;
    let mut socket = None;
    let read_only = true;

    for part in value.split(',').filter(|part| !part.is_empty()) {
        match part {
            "ro" | "readonly" => {}
            "rw" => return Err("vhost-user fs mounts are read-only only".to_string()),
            _ => {
                let Some((key, raw_value)) = part.split_once('=') else {
                    return Err(format!(
                        "invalid vhost-user fs option {part:?}; expected key=value or ro"
                    ));
                };
                match key {
                    "tag" => tag = Some(raw_value.to_string()),
                    "mount" => mountpoint = Some(raw_value.to_string()),
                    "socket" => socket = Some(PathBuf::from(raw_value)),
                    _ => {
                        return Err(format!(
                            "invalid vhost-user fs key {key:?}; expected tag, mount, or socket"
                        ));
                    }
                }
            }
        }
    }

    let mount = runner::VhostUserFsMount {
        tag: tag.ok_or_else(|| "missing vhost-user fs tag=NAME".to_string())?,
        mountpoint: mountpoint
            .ok_or_else(|| "missing vhost-user fs mount=/GUEST/PATH".to_string())?,
        socket: socket.ok_or_else(|| "missing vhost-user fs socket=/HOST/SOCK".to_string())?,
        read_only,
    };
    validate_vhost_user_fs_mount(&mount)?;
    Ok(mount)
}

fn validate_vhost_user_fs_mounts(mounts: &[runner::VhostUserFsMount]) -> Result<()> {
    let mut tags = BTreeSet::new();
    for mount in mounts {
        validate_vhost_user_fs_mount(mount).map_err(anyhow::Error::msg)?;
        if !tags.insert(mount.tag.as_str()) {
            bail!("duplicate vhost-user fs tag: {}", mount.tag);
        }
    }
    Ok(())
}

fn validate_vhost_user_fs_mount(mount: &runner::VhostUserFsMount) -> Result<(), String> {
    if !mount.read_only {
        return Err("vhost-user fs mounts are read-only only".to_string());
    }
    if mount.tag.is_empty() || mount.tag.len() > 36 {
        return Err("vhost-user fs tag must be 1-36 bytes".to_string());
    }
    if !mount
        .tag
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err("vhost-user fs tag may only contain letters, numbers, '.', '_', or '-'".into());
    }
    if matches!(mount.tag.as_str(), "home" | "cwd") {
        return Err(format!("vhost-user fs tag is reserved: {}", mount.tag));
    }
    if !mount.mountpoint.starts_with('/')
        || mount.mountpoint.contains(':')
        || mount.mountpoint.contains(';')
        || mount.mountpoint.contains(',')
    {
        return Err(
            "vhost-user fs mount must be an absolute guest path without ':', ';', or ','"
                .to_string(),
        );
    }
    let socket = mount.socket.to_string_lossy();
    if !mount.socket.is_absolute() || socket.contains(',') || socket.contains(';') {
        return Err(
            "vhost-user fs socket must be an absolute host path without ',' or ';'".to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
