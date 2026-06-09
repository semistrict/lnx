use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
};

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
    if command.first().map(String::as_str) == Some("cp")
        && command.iter().any(|arg| is_host_path(arg))
    {
        copy_between_host_and_guest(
            &layout,
            &command[1..],
            explicit_kernel.then_some(layout.kernel.as_path()),
            explicit_rootfs.then_some(layout.rootfs.as_path()),
        )?;
        return Ok(());
    }
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

fn copy_between_host_and_guest(
    layout: &Layout,
    args: &[String],
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
        (true, false, _) => copy_host_to_guest(layout, &operands, explicit_kernel, explicit_rootfs),
        (false, true, true) => {
            copy_guest_to_host(layout, &operands, explicit_kernel, explicit_rootfs)
        }
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
    explicit_kernel: Option<&Path>,
    explicit_rootfs: Option<&Path>,
) -> Result<()> {
    let guest_dest = args.last().context("missing guest destination")?;
    run_lnx_child(
        layout,
        explicit_kernel,
        explicit_rootfs,
        &["mkdir", "-p", guest_dest],
        None,
        false,
    )
    .context("create guest destination")?;

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
        &["tar", "-C", guest_dest, "-xf", "-"],
        Some(&tar_output.stdout),
        false,
    )
    .context("extract archive in guest")?;
    Ok(())
}

fn copy_guest_to_host(
    layout: &Layout,
    args: &[String],
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
    let status = tar.wait().context("wait for host tar extract")?;
    if !status.success() {
        bail!("host tar extract failed with status {status}");
    }
    Ok(())
}

fn run_lnx_child(
    layout: &Layout,
    explicit_kernel: Option<&Path>,
    explicit_rootfs: Option<&Path>,
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
}
