use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::{checkpoints, descriptor, ingress, init, paths::Layout, runner};

const DEFAULT_CPUS: u8 = 2;
const DEFAULT_MEMORY_MIB: u32 = 4096;

#[derive(Debug, Parser)]
#[command(name = "lnx", about = "Linux VM runner using Rust and libkrun")]
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

    #[command(subcommand)]
    command: Option<Command>,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    guest_command: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init(InitArgs),
    Run(RunArgs),
    Paths,
    Checkpoint(CheckpointArgs),
    Checkpoints,
    Fork(ForkArgs),
    Server(ServerArgs),
    Ingress(IngressArgs),
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
    #[command(name = "_vm-init")]
    HiddenVmInit,
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
struct ForkArgs {
    #[arg(long)]
    checkpoint: Option<String>,

    instance: String,
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
    List,
}

#[derive(Debug, Subcommand)]
enum IngressCommand {
    Enable,
    Disable,
    Status,
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
            command,
            guest_command,
        } = self;

        if let Some(directory) = directory {
            std::env::set_current_dir(&directory)
                .with_context(|| format!("change directory to {}", directory.display()))?;
        }

        let explicit_kernel = kernel.is_some();
        let explicit_rootfs = rootfs.is_some();
        let deterministic = deterministic.map(|seed| runner::DeterministicConfig { seed });
        validate_deterministic_args(nested_kvm, &forwards, deterministic.as_ref(), trace_events)?;
        let effective_no_host_shares = no_host_shares || deterministic.is_some();
        maybe_auto_init_git_worktree(
            &instance,
            command.as_ref(),
            explicit_kernel,
            explicit_rootfs,
            cpus,
            memory_mib,
            snapshot_path.clone(),
            forwards.clone(),
            effective_no_host_shares,
            deterministic.clone(),
            trace_events,
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
        let persisted = descriptor::load(&layout)?;
        let cpus = cpus.or(persisted.cpus).unwrap_or(DEFAULT_CPUS);
        let memory_mib = memory_mib
            .or(persisted.memory_mib)
            .unwrap_or(DEFAULT_MEMORY_MIB);
        let cpus = effective_cpus(cpus, deterministic.as_ref());
        match command {
            Some(Command::Init(args)) => run_init_command(
                &layout,
                init_target,
                &instance,
                args,
                cpus,
                memory_mib,
                snapshot_path,
                forwards,
                explicit_kernel,
                explicit_rootfs,
                effective_no_host_shares,
                deterministic.clone(),
                trace_events,
            ),
            Some(Command::Run(args)) => {
                if cfg!(target_os = "macos") && deterministic.is_some() {
                    run_nested_deterministic_on_macos(
                        &layout,
                        cpus,
                        memory_mib,
                        snapshot_path.as_deref(),
                        deterministic.as_ref().unwrap(),
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
                        explicit_kernel,
                        explicit_rootfs,
                    )
                }
            }
            Some(Command::Paths) => {
                println!("kernel: {}", layout.kernel.display());
                println!("rootfs: {}", layout.rootfs.display());
                println!("base: {}", layout.base.display());
                println!("name: {}", layout.instance);
                println!("instance: {}", layout.instance_dir.display());
                println!("snapshots: {}", layout.snapshot_dir.display());
                Ok(())
            }
            Some(Command::Checkpoint(args)) => {
                if cfg!(target_os = "macos") && deterministic.is_some() {
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
                        deterministic.as_ref().unwrap(),
                        trace_events,
                        root,
                        &[],
                        "checkpoint",
                        subcommand,
                        explicit_kernel,
                    )
                } else {
                    create_checkpoint(
                        layout,
                        args.message.as_deref(),
                        cpus,
                        memory_mib,
                        snapshot_path,
                        forwards,
                        explicit_kernel,
                        explicit_rootfs,
                        effective_no_host_shares,
                        deterministic.clone(),
                        trace_events,
                    )
                }
            }
            Some(Command::Checkpoints) => list_checkpoints(&layout),
            Some(Command::Fork(args)) => {
                if cfg!(target_os = "macos") && deterministic.is_some() {
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
                        deterministic.as_ref().unwrap(),
                        trace_events,
                        root,
                        &[],
                        "fork",
                        subcommand,
                        explicit_kernel,
                    )
                } else {
                    fork_checkpoint(
                        layout,
                        args.checkpoint.as_deref(),
                        &args.instance,
                        cpus,
                        memory_mib,
                        snapshot_path,
                        forwards,
                        explicit_kernel,
                        explicit_rootfs,
                        effective_no_host_shares,
                        deterministic.clone(),
                        trace_events,
                    )
                }
            }
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
                }
            }
            Some(Command::Instances(args)) => match args.command {
                InstancesCommand::List => list_instances(&layout.base),
            },
            Some(Command::Set(args)) => set_instance_settings(&layout, &args.settings),
            Some(Command::Inspect) => inspect_instance(&layout, cpus, memory_mib),
            Some(Command::Logs(args)) => print_instance_logs(&layout, args.console, args.owner),
            Some(Command::HiddenIngress(args)) => {
                let config = ingress::load_config()?;
                ingress::run_hidden(
                    args.spawn,
                    args.cleanup,
                    args.install_service,
                    args.uninstall_service,
                    config,
                )
            }
            Some(Command::HiddenVmInit) => initialize_vm_instance(
                layout,
                cpus,
                memory_mib,
                nested_kvm,
                effective_no_host_shares,
                deterministic.clone(),
                trace_events,
            ),
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
                snapshot_output: None,
                run_as_root: false,
                no_host_shares: effective_no_host_shares || args.no_host_shares,
                deterministic: args
                    .deterministic
                    .map(|seed| runner::DeterministicConfig { seed })
                    .or(deterministic),
                trace_events: trace_events || args.trace_events,
            }),
            None => {
                if cfg!(target_os = "macos") && deterministic.is_some() {
                    run_nested_deterministic_on_macos(
                        &layout,
                        cpus,
                        memory_mib,
                        snapshot_path.as_deref(),
                        deterministic.as_ref().unwrap(),
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
                        explicit_kernel,
                        explicit_rootfs,
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
    cpus: u8,
    memory_mib: u32,
    snapshot_path: Option<PathBuf>,
    forwards: Vec<runner::PortForward>,
    explicit_kernel: bool,
    explicit_rootfs: bool,
    no_host_shares: bool,
    deterministic: Option<runner::DeterministicConfig>,
    trace_events: bool,
) -> Result<()> {
    if let Some(default_instance) = args.default_instance.as_deref() {
        return init_local_default_instance(
            layout,
            default_instance,
            args.kernel.as_deref(),
            cpus,
            memory_mib,
            snapshot_path,
            forwards,
            explicit_kernel,
            explicit_rootfs,
            no_host_shares,
            deterministic,
            trace_events,
        );
    }

    if let Some(image) = args.image {
        init::ensure_base_ignored(&layout.base)?;
        return crate::oci::import_image(layout, &image, args.kernel.as_deref());
    }

    if let Some(target) = local_target {
        if should_init_local_fork(
            args.kernel.as_ref(),
            args.rootfs.as_ref(),
            explicit_kernel,
            explicit_rootfs,
        ) {
            return init_local_fork_from_base(
                instance,
                target.dest_base,
                target.preferred_source_base,
                cpus,
                memory_mib,
                snapshot_path,
                forwards,
                explicit_kernel,
                explicit_rootfs,
                no_host_shares,
                deterministic,
                trace_events,
            );
        }
    }

    init::run(layout, args.kernel.as_deref(), args.rootfs.as_deref())
}

fn init_local_default_instance(
    dest: &Layout,
    default_instance: &str,
    kernel: Option<&Path>,
    cpus: u8,
    memory_mib: u32,
    snapshot_path: Option<PathBuf>,
    forwards: Vec<runner::PortForward>,
    explicit_kernel: bool,
    explicit_rootfs: bool,
    no_host_shares: bool,
    deterministic: Option<runner::DeterministicConfig>,
    trace_events: bool,
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
        if source.rootfs.exists() {
            let checkpoint = create_internal_fork_checkpoint(
                &source,
                cpus,
                memory_mib,
                snapshot_path,
                forwards,
                explicit_kernel,
                explicit_rootfs,
                no_host_shares,
                deterministic,
                trace_events,
            )?;
            checkpoints::fork(&source, &checkpoint, dest)?;
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

fn set_instance_settings(layout: &Layout, settings: &[String]) -> Result<()> {
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
    println!("{}", serde_json::to_string_pretty(&config)?);
    Ok(())
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
    cpus: u8,
    memory_mib: u32,
    snapshot_path: Option<PathBuf>,
    forwards: Vec<runner::PortForward>,
    explicit_kernel: bool,
    explicit_rootfs: bool,
    no_host_shares: bool,
    deterministic: Option<runner::DeterministicConfig>,
    trace_events: bool,
) -> Result<()> {
    let dest = Layout::resolve_in_base(instance, dest_base, None, None);
    init::ensure_base_ignored(&dest.base)?;
    let source_base = match preferred_source_base {
        Some(source_base) if !same_path(&source_base, &dest.base) => Some(source_base),
        _ => Layout::find_instance_base(instance)?,
    };
    match source_base {
        Some(source_base) => {
            let source = Layout::resolve_in_base(instance, source_base, None, None);
            if source.base == dest.base {
                bail!(
                    "local instance already exists: {}",
                    dest.instance_dir.display()
                );
            }
            if source.rootfs.exists() {
                let checkpoint = create_internal_fork_checkpoint(
                    &source,
                    cpus,
                    memory_mib,
                    snapshot_path,
                    forwards,
                    explicit_kernel,
                    explicit_rootfs,
                    no_host_shares,
                    deterministic,
                    trace_events,
                )?;
                checkpoints::fork(&source, &checkpoint, &dest)?;
            } else if init_from_source_base_files(&dest, &source.base)? {
                initialize_vm_instance(
                    dest.clone(),
                    cpus,
                    memory_mib,
                    false,
                    no_host_shares,
                    deterministic,
                    trace_events,
                )?;
            } else {
                init::run(&dest, None, None)?;
                init::ensure_instance(&dest)?;
                initialize_vm_instance(
                    dest.clone(),
                    cpus,
                    memory_mib,
                    false,
                    no_host_shares,
                    deterministic,
                    trace_events,
                )?;
            }
        }
        None => {
            init::run(&dest, None, None)?;
            init::ensure_instance(&dest)?;
            initialize_vm_instance(
                dest.clone(),
                cpus,
                memory_mib,
                false,
                no_host_shares,
                deterministic,
                trace_events,
            )?;
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
    init::ensure_instance(dest)?;
    Ok(true)
}

fn maybe_auto_init_git_worktree(
    instance: &str,
    command: Option<&Command>,
    explicit_kernel: bool,
    explicit_rootfs: bool,
    cpus: Option<u8>,
    memory_mib: Option<u32>,
    snapshot_path: Option<PathBuf>,
    forwards: Vec<runner::PortForward>,
    no_host_shares: bool,
    deterministic: Option<runner::DeterministicConfig>,
    trace_events: bool,
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

    let source = Layout::resolve_in_base(instance, plan.source_base.clone(), None, None);
    let source_config = descriptor::load(&source)?;
    let cpus = effective_cpus(
        cpus.or(source_config.cpus).unwrap_or(DEFAULT_CPUS),
        deterministic.as_ref(),
    );
    let memory_mib = memory_mib
        .or(source_config.memory_mib)
        .unwrap_or(DEFAULT_MEMORY_MIB);
    eprintln!(
        "init: git worktree {} from {}",
        plan.dest_base.display(),
        plan.source_base.display()
    );
    init_local_fork_from_base(
        instance,
        plan.dest_base,
        Some(plan.source_base),
        cpus,
        memory_mib,
        snapshot_path,
        forwards,
        false,
        false,
        no_host_shares,
        deterministic,
        trace_events,
    )
}

fn command_allows_worktree_auto_init(command: Option<&Command>) -> bool {
    match command {
        Some(Command::Init(_))
        | Some(Command::Ingress(_))
        | Some(Command::HiddenIngress(_))
        | Some(Command::HiddenVmInit)
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
    let latest_snapshot = layout.snapshot_dir.join("latest");
    let checkpoints = match fs::read_dir(&layout.checkpoint_dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
            .count(),
        Err(_) => 0,
    };
    let inspect = serde_json::json!({
        "name": layout.instance,
        "state": instance_state(layout),
        "pids": instance_pids(layout),
        "cpus": cpus,
        "memory_mib": memory_mib,
        "created": config.created,
        "image": config.image,
        "settings": config,
        "rootfs": layout.rootfs,
        "rootfs_size_bytes": file_len(&layout.rootfs),
        "rootfs_allocated_bytes": allocated_bytes(&layout.rootfs),
        "snapshot": if latest_snapshot.exists() {
            serde_json::json!({
                "path": latest_snapshot,
                "pages_allocated_bytes": allocated_bytes(&latest_snapshot.join("pages.img")),
            })
        } else {
            serde_json::Value::Null
        },
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

fn list_instances(base: &Path) -> Result<()> {
    let mut names = BTreeSet::new();
    collect_child_dir_names(&base.join("instances"), &mut names)?;

    let mut instances = names
        .into_iter()
        .map(|name| {
            let layout = Layout::resolve_in_base(&name, base.to_path_buf(), None, None);
            let state = instance_state(&layout);
            let pids = instance_pids(&layout).join(",");
            Ok(InstanceRow { name, state, pids })
        })
        .collect::<Result<Vec<_>>>()?;
    instances.sort_by_key(|row| (instance_state_rank(row.state), row.name.clone()));

    println!("{:<36} {:<12} {}", "NAME", "STATE", "PIDS");
    for row in instances {
        println!("{:<36} {:<12} {}", row.name, row.state, row.pids);
    }
    Ok(())
}

struct InstanceRow {
    name: String,
    state: &'static str,
    pids: String,
}

fn instance_state_rank(state: &str) -> u8 {
    match state {
        "running" => 0,
        "starting" => 1,
        "stopped" => 2,
        _ => 3,
    }
}

fn collect_child_dir_names(parent: &Path, names: &mut BTreeSet<String>) -> Result<()> {
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("read {}", parent.display())),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            names.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(())
}

fn instance_state(layout: &Layout) -> &'static str {
    let broker = layout.run_dir.join("broker.sock");
    if broker.exists() && runner::connect_broker(&broker).is_ok() {
        "running"
    } else if alive_owner_pid(&layout.run_dir.join("bootstrap.lock.d")).is_some() {
        "starting"
    } else if layout.rootfs.exists() {
        "stopped"
    } else {
        "partial"
    }
}

fn instance_pids(layout: &Layout) -> Vec<String> {
    let mut pids = BTreeMap::new();
    if let Some(pid) = alive_owner_pid(&layout.run_dir.join("bootstrap.lock.d")) {
        pids.insert(pid, ());
    }
    for pid in host_pids_for_instance(&layout.instance) {
        pids.insert(pid, ());
    }
    pids.keys().map(ToString::to_string).collect()
}

fn alive_owner_pid(lock_dir: &Path) -> Option<i32> {
    let pid = fs::read_to_string(lock_dir.join("owner.pid"))
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()?;
    process_alive(pid).then_some(pid)
}

fn host_pids_for_instance(instance: &str) -> Vec<i32> {
    let output = ProcessCommand::new("pgrep")
        .arg("-f")
        .arg(format!("--instance[= ]{instance}"))
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .filter(|pid| *pid != std::process::id() as i32 && process_alive(*pid))
        .collect()
}

fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

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
    explicit_kernel: bool,
    explicit_rootfs: bool,
) -> Result<()> {
    ensure_image_and_instance(&layout, explicit_kernel, explicit_rootfs)?;
    ensure_vm_initialized(
        &layout,
        cpus,
        memory_mib,
        forwards.clone(),
        snapshot_path.is_some(),
        nested_kvm,
        no_host_shares,
        deterministic.as_ref(),
        trace_events,
    )?;

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
            explicit_rootfs.then_some(layout.rootfs.as_path()),
        )?;
        return Ok(());
    }
    let cwd = std::env::current_dir().context("current directory")?;

    let restore_snapshot =
        restore_snapshot_for_run(&layout, snapshot_path, explicit_kernel, explicit_rootfs);

    let config = runner::RunConfig {
        layout,
        command,
        cwd,
        cpus,
        memory_mib,
        nested_kvm,
        restore_snapshot,
        forwards,
        run_as_root,
        snapshot_output: None,
        no_host_shares,
        deterministic,
        trace_events,
    };

    let status = runner::run(config)?;
    std::process::exit(status);
}

fn ensure_image_and_instance(
    layout: &Layout,
    explicit_kernel: bool,
    explicit_rootfs: bool,
) -> Result<()> {
    if !layout.kernel.exists() {
        if explicit_kernel {
            bail!("missing kernel: {}", layout.kernel.display());
        }
        eprintln!("first run: kernel missing, initializing lnx image files");
        init::ensure_kernel(layout).context("auto-init kernel")?;
    }
    if !layout.rootfs.exists() {
        if explicit_rootfs {
            bail!("missing rootfs: {}", layout.rootfs.display());
        }
        eprintln!("first run: instance rootfs missing, initializing lnx instance files");
        init::run(layout, None, None).context("auto-init")?;
        init::ensure_instance(layout).context("auto-init instance")?;
    } else {
        init::ensure_instance(layout).context("auto-init instance")?;
    }
    Ok(())
}

fn restore_snapshot_for_run(
    layout: &Layout,
    snapshot_path: Option<PathBuf>,
    explicit_kernel: bool,
    explicit_rootfs: bool,
) -> Option<PathBuf> {
    if snapshot_path.is_some() {
        return snapshot_path;
    }
    if explicit_kernel || explicit_rootfs {
        return None;
    }
    let latest = layout.snapshot_dir.join("latest");
    latest.exists().then_some(latest)
}

fn ensure_vm_initialized(
    layout: &Layout,
    cpus: u8,
    memory_mib: u32,
    _forwards: Vec<runner::PortForward>,
    explicit_snapshot: bool,
    nested_kvm: bool,
    no_host_shares: bool,
    deterministic: Option<&runner::DeterministicConfig>,
    trace_events: bool,
) -> Result<()> {
    if layout.vm_initialized.exists() || explicit_snapshot {
        return Ok(());
    }
    if layout.snapshot_dir.join("latest").exists() {
        mark_vm_initialized(layout)?;
        return Ok(());
    }
    eprintln!("first run: initializing VM instance {}", layout.instance);
    let cpus = cpus.to_string();
    let memory_mib = memory_mib.to_string();
    let no_host_shares_arg = no_host_shares.then_some("--no-host-shares");
    let nested_kvm_arg = nested_kvm.then_some("--nested-kvm");
    let mut command = vec!["--cpus", &cpus, "--memory-mib", &memory_mib];
    if let Some(arg) = nested_kvm_arg {
        command.push(arg);
    }
    if let Some(arg) = no_host_shares_arg {
        command.push(arg);
    }
    if let Some(config) = deterministic {
        command.push("--deterministic");
        command.push(&config.seed);
    }
    if trace_events {
        command.push("--trace-events");
    }
    command.push("_vm-init");
    run_lnx_child(
        layout,
        Some(&layout.kernel),
        Some(&layout.rootfs),
        None,
        &command,
        None,
        false,
    )
    .context("initialize VM instance")?;
    Ok(())
}

fn validate_deterministic_args(
    nested_kvm: bool,
    forwards: &[runner::PortForward],
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
    Ok(())
}

fn effective_cpus(configured: u8, deterministic: Option<&runner::DeterministicConfig>) -> u8 {
    if deterministic.is_some() {
        1
    } else {
        configured
    }
}

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
    let linux_gvproxy = find_linux_gvproxy_binary(&layout.base)?;
    let outer_instance = nested_deterministic_outer_instance(&layout.instance);
    let outer_layout = Layout::resolve(&outer_instance, Some(layout.kernel.clone()), None)?;
    ensure_image_and_instance(&outer_layout, explicit_kernel, false)?;
    ensure_vm_initialized(
        &outer_layout,
        2,
        memory_mib.max(DEFAULT_MEMORY_MIB),
        Vec::new(),
        false,
        true,
        false,
        None,
        false,
    )?;

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
        &linux_gvproxy,
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
        snapshot_output: None,
        run_as_root: false,
        no_host_shares: false,
        deterministic: None,
        trace_events: false,
    })
    .with_context(|| format!("run deterministic {command_label} in nested Linux"))?;
    std::process::exit(status);
}

fn nested_deterministic_outer_instance(instance: &str) -> String {
    format!("{instance}-deterministic-outer")
}

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
        "--rootfs".to_string(),
        layout.rootfs.display().to_string(),
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
    linux_gvproxy: &Path,
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
        format!(
            "cp {} \"$nested_tools/gvproxy-linux-arm64\"",
            shell_quote(&linux_gvproxy.display().to_string())
        ),
        "chmod +x \"$nested_tools\"/*".to_string(),
        "export LNX_BIN=\"$nested_tools/lnx\"".to_string(),
        "export GVPROXY_PATH=\"$nested_tools/gvproxy-linux-arm64\"".to_string(),
        "export LNX_ROOTFS_BACKEND=block".to_string(),
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

fn find_linux_gvproxy_binary(base: &Path) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("LNX_LINUX_GVPROXY").map(PathBuf::from) {
        return require_executable_file(path, "Linux gvproxy binary");
    }
    let exe = std::env::current_exe().context("current executable")?;
    if let Some(dir) = exe.parent() {
        let candidate = dir.join("gvproxy-linux-arm64");
        if candidate.exists() {
            return require_executable_file(candidate, "Linux gvproxy binary");
        }
    }
    let mut cursor = exe.parent();
    while let Some(dir) = cursor {
        if dir.file_name().and_then(|name| name.to_str()) == Some("target") {
            let candidate = dir.join("gvproxy-linux-arm64");
            if candidate.exists() {
                return require_executable_file(candidate, "Linux gvproxy binary");
            }
        }
        cursor = dir.parent();
    }
    let cache_path = base.join("cache").join("gvproxy-linux-arm64");
    crate::init::ensure_nested_linux_gvproxy(&cache_path)?;
    require_executable_file(cache_path, "Linux gvproxy binary")
}

fn require_executable_file(path: PathBuf, label: &str) -> Result<PathBuf> {
    if path.is_file() {
        Ok(path)
    } else {
        bail!("{label} not found: {}", path.display())
    }
}

fn initialize_vm_instance(
    layout: Layout,
    cpus: u8,
    memory_mib: u32,
    nested_kvm: bool,
    no_host_shares: bool,
    deterministic: Option<runner::DeterministicConfig>,
    trace_events: bool,
) -> Result<()> {
    if layout.vm_initialized.exists() {
        return Ok(());
    }
    let cwd = std::env::current_dir().context("current directory")?;
    let status = runner::run(runner::RunConfig {
        layout: layout.clone(),
        command: vec!["true".to_string()],
        cwd,
        cpus,
        memory_mib,
        nested_kvm,
        restore_snapshot: None,
        forwards: Vec::new(),
        snapshot_output: Some(layout.snapshot_dir.join("latest")),
        run_as_root: false,
        no_host_shares,
        deterministic,
        trace_events,
    })
    .context("initialize VM instance")?;
    if status != 0 {
        bail!("VM initialization exited with status {status}");
    }
    mark_vm_initialized(&layout)
}

fn mark_vm_initialized(layout: &Layout) -> Result<()> {
    if let Some(parent) = layout.vm_initialized.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(&layout.vm_initialized, b"1\n")
        .with_context(|| format!("write {}", layout.vm_initialized.display()))?;
    Ok(())
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

fn create_checkpoint(
    layout: Layout,
    name: Option<&str>,
    cpus: u8,
    memory_mib: u32,
    snapshot_path: Option<PathBuf>,
    forwards: Vec<runner::PortForward>,
    explicit_kernel: bool,
    explicit_rootfs: bool,
    no_host_shares: bool,
    deterministic: Option<runner::DeterministicConfig>,
    trace_events: bool,
) -> Result<()> {
    ensure_image_and_instance(&layout, explicit_kernel, explicit_rootfs)?;

    std::fs::create_dir_all(&layout.checkpoint_dir)
        .with_context(|| format!("create {}", layout.checkpoint_dir.display()))?;
    let (checkpoint, path) = checkpoints::new_checkpoint_path(&layout, name)?;
    let broker_socket = layout.run_dir.join("broker.sock");
    if broker_socket.exists() {
        runner::validate_runtime_deterministic_compatibility(&layout, deterministic.as_ref())?;
        runner::request_checkpoint(&broker_socket, &path).context("checkpoint running VM")?;
    } else {
        let cwd = std::env::current_dir().context("current directory")?;
        let restore_snapshot =
            restore_snapshot_for_run(&layout, snapshot_path, explicit_kernel, explicit_rootfs);
        runner::seed_checkpoint_from_base(
            &layout,
            &path,
            restore_snapshot.as_deref(),
            &layout.snapshot_dir.join("latest"),
        )?;
        let status = runner::run(runner::RunConfig {
            layout: layout.clone(),
            command: vec!["true".to_string()],
            cwd,
            cpus,
            memory_mib,
            nested_kvm: false,
            restore_snapshot,
            forwards,
            snapshot_output: Some(path.clone()),
            run_as_root: false,
            no_host_shares,
            deterministic,
            trace_events,
        })?;
        if status != 0 {
            bail!("checkpoint command exited with status {status}");
        }
    }
    checkpoints::write_metadata(&layout, &checkpoint)?;
    let label = checkpoint.name.as_deref().unwrap_or(&checkpoint.id);
    println!("{label}");
    Ok(())
}

fn list_checkpoints(layout: &Layout) -> Result<()> {
    for checkpoint in checkpoints::list(layout)? {
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

fn fork_checkpoint(
    source: Layout,
    checkpoint: Option<&str>,
    instance: &str,
    cpus: u8,
    memory_mib: u32,
    snapshot_path: Option<PathBuf>,
    forwards: Vec<runner::PortForward>,
    explicit_kernel: bool,
    explicit_rootfs: bool,
    no_host_shares: bool,
    deterministic: Option<runner::DeterministicConfig>,
    trace_events: bool,
) -> Result<()> {
    let checkpoint = match checkpoint {
        Some(checkpoint) => checkpoints::resolve(&source, checkpoint)?,
        None => create_internal_fork_checkpoint(
            &source,
            cpus,
            memory_mib,
            snapshot_path,
            forwards,
            explicit_kernel,
            explicit_rootfs,
            no_host_shares,
            deterministic,
            trace_events,
        )?,
    };
    let dest = Layout::resolve_in_base(instance, source.base.clone(), None, None);
    init::ensure_base_ignored(&dest.base)?;
    checkpoints::fork(&source, &checkpoint, &dest)?;
    println!("{instance}");
    Ok(())
}

fn create_internal_fork_checkpoint(
    layout: &Layout,
    cpus: u8,
    memory_mib: u32,
    snapshot_path: Option<PathBuf>,
    forwards: Vec<runner::PortForward>,
    explicit_kernel: bool,
    explicit_rootfs: bool,
    no_host_shares: bool,
    deterministic: Option<runner::DeterministicConfig>,
    trace_events: bool,
) -> Result<checkpoints::Checkpoint> {
    ensure_image_and_instance(layout, explicit_kernel, explicit_rootfs)?;

    std::fs::create_dir_all(&layout.checkpoint_dir)
        .with_context(|| format!("create {}", layout.checkpoint_dir.display()))?;
    let (checkpoint, path) = checkpoints::new_checkpoint_path(layout, None)?;
    let broker_socket = layout.run_dir.join("broker.sock");
    if broker_socket.exists() {
        runner::validate_runtime_deterministic_compatibility(layout, deterministic.as_ref())?;
        runner::request_checkpoint(&broker_socket, &path).context("checkpoint running VM")?;
    } else {
        let cwd = std::env::current_dir().context("current directory")?;
        let restore_snapshot =
            restore_snapshot_for_run(layout, snapshot_path, explicit_kernel, explicit_rootfs);
        runner::seed_checkpoint_from_base(
            layout,
            &path,
            restore_snapshot.as_deref(),
            &layout.snapshot_dir.join("latest"),
        )?;
        let status = runner::run(runner::RunConfig {
            layout: layout.clone(),
            command: vec!["true".to_string()],
            cwd,
            cpus,
            memory_mib,
            nested_kvm: false,
            restore_snapshot,
            forwards,
            snapshot_output: Some(path.clone()),
            run_as_root: false,
            no_host_shares,
            deterministic,
            trace_events,
        })?;
        if status != 0 {
            bail!("checkpoint command exited with status {status}");
        }
    }
    checkpoints::write_metadata(layout, &checkpoint)?;
    Ok(checkpoint)
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

#[cfg(test)]
mod tests;
