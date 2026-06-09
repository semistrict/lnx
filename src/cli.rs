use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::{checkpoints, ingress, init, paths::Layout, runner};

#[derive(Debug, Parser)]
#[command(name = "lnx", about = "Linux VM runner using Rust and libkrun")]
pub struct Cli {
    #[arg(long, env = "LNX_INSTANCE", default_value = "default")]
    instance: String,

    #[arg(long)]
    kernel: Option<PathBuf>,

    #[arg(long)]
    rootfs: Option<PathBuf>,

    #[arg(long, default_value_t = 2)]
    cpus: u8,

    #[arg(long, default_value_t = 4096)]
    memory_mib: u32,

    #[arg(
        long,
        help = "Restore from an explicit libkrun memory snapshot directory"
    )]
    snapshot: Option<PathBuf>,

    #[arg(long)]
    no_snapshot_restore: bool,

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
    Ingress(IngressArgs),
    #[command(hide = true)]
    #[command(name = "_ingress")]
    HiddenIngress(HiddenIngressArgs),
}

#[derive(Debug, Args)]
struct InitArgs {
    #[arg(long)]
    kernel: Option<PathBuf>,

    #[arg(long)]
    rootfs: Option<PathBuf>,
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
struct IngressArgs {
    #[command(subcommand)]
    command: IngressCommand,
}

#[derive(Debug, Subcommand)]
enum IngressCommand {
    Enable,
    Disable,
    Status,
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
            instance,
            kernel,
            rootfs,
            cpus,
            memory_mib,
            snapshot: snapshot_path,
            no_snapshot_restore,
            forwards,
            command,
            guest_command,
        } = self;

        let explicit_kernel = kernel.is_some();
        let explicit_rootfs = rootfs.is_some();
        let layout = Layout::resolve(&instance, kernel, rootfs)?;
        match command {
            Some(Command::Init(args)) => {
                init::run(&layout, args.kernel.as_deref(), args.rootfs.as_deref())
            }
            Some(Command::Run(args)) => run_guest(
                layout,
                args.command,
                cpus,
                memory_mib,
                snapshot_path,
                no_snapshot_restore,
                forwards,
                explicit_kernel,
                explicit_rootfs,
            ),
            Some(Command::Paths) => {
                println!("kernel: {}", layout.kernel.display());
                println!("rootfs: {}", layout.rootfs.display());
                println!("base: {}", layout.base.display());
                println!("name: {}", layout.instance);
                println!("instance: {}", layout.instance_dir.display());
                println!("snapshots: {}", layout.snapshot_dir.display());
                Ok(())
            }
            Some(Command::Checkpoint(args)) => create_checkpoint(
                layout,
                args.message.as_deref(),
                cpus,
                memory_mib,
                snapshot_path,
                no_snapshot_restore,
                forwards,
                explicit_kernel,
                explicit_rootfs,
            ),
            Some(Command::Checkpoints) => list_checkpoints(&layout),
            Some(Command::Fork(args)) => fork_checkpoint(
                layout,
                args.checkpoint.as_deref(),
                &args.instance,
                cpus,
                memory_mib,
                snapshot_path,
                no_snapshot_restore,
                forwards,
                explicit_kernel,
                explicit_rootfs,
            ),
            Some(Command::Ingress(args)) => {
                let config = ingress::load_config()?;
                match args.command {
                    IngressCommand::Enable => ingress::enable(&config),
                    IngressCommand::Disable => ingress::disable(&config),
                    IngressCommand::Status => ingress::print_status(&config),
                }
            }
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
            None => run_guest(
                layout,
                guest_command,
                cpus,
                memory_mib,
                snapshot_path,
                no_snapshot_restore,
                forwards,
                explicit_kernel,
                explicit_rootfs,
            ),
        }
    }
}

fn run_guest(
    layout: Layout,
    command: Vec<String>,
    cpus: u8,
    memory_mib: u32,
    snapshot_path: Option<PathBuf>,
    no_snapshot_restore: bool,
    forwards: Vec<runner::PortForward>,
    explicit_kernel: bool,
    explicit_rootfs: bool,
) -> Result<()> {
    if !layout.kernel.exists() {
        if explicit_kernel {
            bail!("missing kernel: {}", layout.kernel.display());
        }
        eprintln!("first run: kernel missing, initializing lnx image files");
        init::run(&layout, None, None).context("auto-init")?;
    }
    if !layout.rootfs.exists() {
        if explicit_rootfs {
            bail!("missing rootfs: {}", layout.rootfs.display());
        }
        eprintln!("first run: rootfs missing, initializing lnx image files");
        init::run(&layout, None, None).context("auto-init")?;
    }

    let command = if command.is_empty() {
        vec!["bash".to_string(), "-l".to_string()]
    } else {
        command
    };
    let cwd = std::env::current_dir().context("current directory")?;

    let restore_snapshot = if no_snapshot_restore {
        None
    } else {
        snapshot_path.or_else(|| {
            let latest = layout.snapshot_dir.join("latest");
            latest.exists().then_some(latest)
        })
    };

    let config = runner::RunConfig {
        layout,
        command,
        cwd,
        cpus,
        memory_mib,
        restore_snapshot,
        forwards,
        snapshot_output: None,
    };

    let status = runner::run(config)?;
    std::process::exit(status);
}

fn create_checkpoint(
    layout: Layout,
    name: Option<&str>,
    cpus: u8,
    memory_mib: u32,
    snapshot_path: Option<PathBuf>,
    no_snapshot_restore: bool,
    forwards: Vec<runner::PortForward>,
    explicit_kernel: bool,
    explicit_rootfs: bool,
) -> Result<()> {
    if !layout.kernel.exists() {
        if explicit_kernel {
            bail!("missing kernel: {}", layout.kernel.display());
        }
        eprintln!("first run: kernel missing, initializing lnx image files");
        init::run(&layout, None, None).context("auto-init")?;
    }
    if !layout.rootfs.exists() {
        if explicit_rootfs {
            bail!("missing rootfs: {}", layout.rootfs.display());
        }
        eprintln!("first run: rootfs missing, initializing lnx image files");
        init::run(&layout, None, None).context("auto-init")?;
    }

    std::fs::create_dir_all(&layout.checkpoint_dir)
        .with_context(|| format!("create {}", layout.checkpoint_dir.display()))?;
    let (checkpoint, path) = checkpoints::new_checkpoint_path(&layout, name)?;
    let broker_socket = layout.run_dir.join("broker.sock");
    if broker_socket.exists() {
        runner::request_checkpoint(&broker_socket, &path).context("checkpoint running VM")?;
    } else {
        let cwd = std::env::current_dir().context("current directory")?;
        let restore_snapshot = if no_snapshot_restore {
            None
        } else {
            snapshot_path.or_else(|| {
                let latest = layout.snapshot_dir.join("latest");
                latest.exists().then_some(latest)
            })
        };
        let status = runner::run(runner::RunConfig {
            layout: layout.clone(),
            command: vec!["true".to_string()],
            cwd,
            cpus,
            memory_mib,
            restore_snapshot,
            forwards,
            snapshot_output: Some(path.clone()),
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
    no_snapshot_restore: bool,
    forwards: Vec<runner::PortForward>,
    explicit_kernel: bool,
    explicit_rootfs: bool,
) -> Result<()> {
    let checkpoint = match checkpoint {
        Some(checkpoint) => checkpoints::resolve(&source, checkpoint)?,
        None => create_internal_fork_checkpoint(
            &source,
            cpus,
            memory_mib,
            snapshot_path,
            no_snapshot_restore,
            forwards,
            explicit_kernel,
            explicit_rootfs,
        )?,
    };
    let dest = Layout::resolve(instance, None, None)?;
    checkpoints::fork(&source, &checkpoint, &dest)?;
    println!("{instance}");
    Ok(())
}

fn create_internal_fork_checkpoint(
    layout: &Layout,
    cpus: u8,
    memory_mib: u32,
    snapshot_path: Option<PathBuf>,
    no_snapshot_restore: bool,
    forwards: Vec<runner::PortForward>,
    explicit_kernel: bool,
    explicit_rootfs: bool,
) -> Result<checkpoints::Checkpoint> {
    if !layout.kernel.exists() {
        if explicit_kernel {
            bail!("missing kernel: {}", layout.kernel.display());
        }
        eprintln!("first run: kernel missing, initializing lnx image files");
        init::run(layout, None, None).context("auto-init")?;
    }
    if !layout.rootfs.exists() {
        if explicit_rootfs {
            bail!("missing rootfs: {}", layout.rootfs.display());
        }
        eprintln!("first run: rootfs missing, initializing lnx image files");
        init::run(layout, None, None).context("auto-init")?;
    }

    std::fs::create_dir_all(&layout.checkpoint_dir)
        .with_context(|| format!("create {}", layout.checkpoint_dir.display()))?;
    let (checkpoint, path) = checkpoints::new_checkpoint_path(layout, None)?;
    let broker_socket = layout.run_dir.join("broker.sock");
    if broker_socket.exists() {
        runner::request_checkpoint(&broker_socket, &path).context("checkpoint running VM")?;
    } else {
        let cwd = std::env::current_dir().context("current directory")?;
        let restore_snapshot = if no_snapshot_restore {
            None
        } else {
            snapshot_path.or_else(|| {
                let latest = layout.snapshot_dir.join("latest");
                latest.exists().then_some(latest)
            })
        };
        let status = runner::run(runner::RunConfig {
            layout: layout.clone(),
            command: vec!["true".to_string()],
            cwd,
            cpus,
            memory_mib,
            restore_snapshot,
            forwards,
            snapshot_output: Some(path.clone()),
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
mod tests {
    use super::*;

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
}
