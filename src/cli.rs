use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::{init, paths::Layout, runner};

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

    #[arg(long, default_value_t = 8192)]
    memory_mib: u32,

    #[arg(
        long,
        help = "Restore from an explicit libkrun memory snapshot directory"
    )]
    snapshot: Option<PathBuf>,

    #[arg(long)]
    no_snapshot_restore: bool,

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
            None => run_guest(
                layout,
                guest_command,
                cpus,
                memory_mib,
                snapshot_path,
                no_snapshot_restore,
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
    };

    let status = runner::run(config)?;
    std::process::exit(status);
}
