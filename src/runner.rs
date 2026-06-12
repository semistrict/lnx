use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    net::{Shutdown, TcpListener, TcpStream},
    os::fd::AsRawFd,
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, Once,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;

use anyhow::{Context, Result, bail};
use lnx_protocol::{MAX_MESSAGE_SIZE, Message, PROTOCOL_VERSION};

use crate::{initramfs, krun::Context as KrunContext, paths::Layout};

const AGENT_PORT: u32 = 10240;
const SNAPSHOT_PORT: u32 = 10241;
const CONTROL_PORT: u32 = 10242;
const FRAME_SNAPSHOT: u8 = b'K';
const INTERRUPT_POLL_TIMEOUT: Duration = Duration::from_millis(100);
// Accepted vmstate.bin container version. The macOS and Linux snapshot
// containers version independently; keep in sync with VERSION in
// third_party/libkrun/src/vmm/src/{macos,linux}/snapshot/container.rs.
#[cfg(target_os = "macos")]
const SNAPSHOT_VMSTATE_VERSION: u32 = 1;
#[cfg(not(target_os = "macos"))]
const SNAPSHOT_VMSTATE_VERSION: u32 = 2;

// Owner exit status meaning "the VM failed to start with a restore
// configured"; the client retries the spawn with a cold boot.
const EXIT_RESTORE_FAILED: i32 = 86;

const DEFAULT_BROKER_IDLE_TTL: Duration = Duration::ZERO;
const DEFAULT_OWNER_IDLE_TTL: Duration = Duration::from_secs(5);
// The detached owner counts idle time from broker start, so a TTL shorter than
// the client's connect retry interval would suspend the VM before the client
// that spawned it ever connects.
const MIN_OWNER_IDLE_TTL: Duration = Duration::from_millis(250);
const OWNER_BOOT_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_AGENT_ACCEPT_TIMEOUT: Duration = Duration::from_secs(90);
const ROOTFS_BACKEND_ENV: &str = "LNX_ROOTFS_BACKEND";

static SIGNAL_INIT: Once = Once::new();
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigint(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub layout: Layout,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub cpus: u8,
    pub memory_mib: u32,
    pub nested_kvm: bool,
    pub restore_snapshot: Option<PathBuf>,
    pub forwards: Vec<PortForward>,
    pub snapshot_output: Option<PathBuf>,
    pub run_as_root: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortForward {
    pub listen_host: String,
    pub listen_port: u16,
    pub guest_host: String,
    pub guest_port: u16,
}

pub fn run(config: RunConfig) -> Result<i32> {
    install_signal_handlers();
    INTERRUPTED.store(false, Ordering::SeqCst);
    fs::create_dir_all(&config.layout.run_dir)
        .with_context(|| format!("create {}", config.layout.run_dir.display()))?;
    fs::create_dir_all(&config.layout.snapshot_dir)
        .with_context(|| format!("create {}", config.layout.snapshot_dir.display()))?;
    let run_log = Arc::new(RunLog::open(&config.layout)?);
    run_log.line(format!(
        "run.start pid={} instance={} cmd={:?} cwd={} restore={}",
        std::process::id(),
        config.layout.instance,
        config.command,
        config.cwd.display(),
        config
            .restore_snapshot
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "false".to_string())
    ));
    run_log.line(format!(
        "logs lnx={} timings={} console={} gvproxy={}",
        run_log.path.display(),
        config.layout.run_dir.join("timings.log").display(),
        config.layout.console_log.display(),
        config.layout.run_dir.join("gvproxy.log").display()
    ));
    let broker_socket = config.layout.run_dir.join("broker.sock");
    if let Some(status) =
        run_existing_broker_client(
            &broker_socket,
            &config.command,
            &config.cwd,
            config.run_as_root,
            Some(&run_log),
        )?
    {
        run_log.line(format!("run.done status={status}"));
        return Ok(status);
    }
    if config.snapshot_output.is_some() {
        // Checkpoint and vm-init runs need the snapshot written before they
        // return, so they keep the VM in the foreground.
        return run_foreground(config, run_log, broker_socket);
    }

    let mut owner = spawn_owner_process(&config, &run_log)?;
    let status = match run_broker_client_awaiting_owner(
        &broker_socket,
        &config.command,
        &config.cwd,
        &mut owner,
        &config,
        &config.layout,
        &run_log,
    ) {
        Ok(status) => status,
        Err(e) => {
            run_log.line(format!("client.error {e:#}"));
            return Err(e);
        }
    };
    run_log.line(format!("run.done status={status}"));
    Ok(status)
}

pub fn run_owner(config: RunConfig) -> Result<()> {
    fs::create_dir_all(&config.layout.run_dir)
        .with_context(|| format!("create {}", config.layout.run_dir.display()))?;
    fs::create_dir_all(&config.layout.snapshot_dir)
        .with_context(|| format!("create {}", config.layout.snapshot_dir.display()))?;
    let run_log = Arc::new(RunLog::open(&config.layout)?);
    run_log.line(format!(
        "owner.start pid={} instance={} cwd={} restore={}",
        std::process::id(),
        config.layout.instance,
        config.cwd.display(),
        config
            .restore_snapshot
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "false".to_string())
    ));
    let broker_socket = config.layout.run_dir.join("broker.sock");
    let Some(bootstrap_lock) = acquire_bootstrap_for_owner(
        &config.layout.run_dir.join("bootstrap.lock.d"),
        &broker_socket,
        &run_log,
    )?
    else {
        run_log.line("owner.exit reason=existing_broker");
        return Ok(());
    };
    if broker_socket.exists() {
        run_log.line(format!(
            "broker.stale_socket.remove path={}",
            broker_socket.display()
        ));
        let _ = fs::remove_file(&broker_socket);
    }

    let idle = IdlePolicy {
        ttl: owner_idle_ttl(),
        starts_idle: true,
    };
    let vm = match start_vm(&config, &run_log, &broker_socket, idle) {
        Ok(vm) => vm,
        // A snapshot the devices refuse (topology drift, corrupt sections)
        // must not strand the instance: hand the decision to the client,
        // which respawns the owner for a cold boot.
        Err(e) if config.restore_snapshot.is_some() => {
            run_log.line(format!("owner.start.restore_failed error={e:#}"));
            drop(bootstrap_lock);
            std::process::exit(EXIT_RESTORE_FAILED);
        }
        Err(e) => return Err(e),
    };
    let _ = vm.owner.join();
    run_log.line("owner.done");
    drop(vm.network);
    drop(bootstrap_lock);
    Ok(())
}

fn run_foreground(config: RunConfig, run_log: Arc<RunLog>, broker_socket: PathBuf) -> Result<i32> {
    let bootstrap_lock = match acquire_bootstrap_or_run_client(
        &config.layout.run_dir.join("bootstrap.lock.d"),
        &broker_socket,
        &config.command,
        &config.cwd,
        config.run_as_root,
        &run_log,
    )? {
        BootstrapOutcome::Lock(lock) => lock,
        BootstrapOutcome::Status(status) => return Ok(status),
    };
    if let Some(status) =
        run_existing_broker_client(
            &broker_socket,
            &config.command,
            &config.cwd,
            config.run_as_root,
            Some(&run_log),
        )?
    {
        drop(bootstrap_lock);
        return Ok(status);
    }
    if broker_socket.exists() {
        run_log.line(format!(
            "broker.stale_socket.remove path={}",
            broker_socket.display()
        ));
        let _ = fs::remove_file(&broker_socket);
    }

    let vm = start_vm(
        &config,
        &run_log,
        &broker_socket,
        IdlePolicy {
            ttl: broker_idle_ttl(),
            starts_idle: false,
        },
    )?;
    let status = match run_broker_client_retry(
        &broker_socket,
        &config.command,
        &config.cwd,
        config.run_as_root,
        Duration::from_secs(5),
    )
    .with_context(|| console_hint(&config.layout.console_log))
    {
        Ok(status) => status,
        Err(e) => {
            vm.timings.event(&format!("restore.client.error {e:#}"));
            run_log.line(format!("client.error {e:#}"));
            log_console_tail(&run_log, &config.layout.console_log);
            return Err(e);
        }
    };
    let _ = vm.owner.join();
    vm.timings.event(&format!("run.done status={status}"));
    run_log.line(format!("run.done status={status}"));
    drop(vm.network);
    drop(bootstrap_lock);
    Ok(status)
}

struct VmHandles {
    owner: thread::JoinHandle<()>,
    network: Gvproxy,
    timings: Arc<TimingLog>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootfsBackend {
    Pmem,
    Block,
}

impl RootfsBackend {
    fn from_env(value: Option<String>) -> Result<Self> {
        match value.as_deref() {
            None | Some("") | Some("pmem") => Ok(Self::Pmem),
            Some("block") => Ok(Self::Block),
            Some(value) => {
                bail!("{ROOTFS_BACKEND_ENV} must be either 'pmem' or 'block', got {value:?}")
            }
        }
    }
}

#[derive(Clone, Copy)]
struct IdlePolicy {
    ttl: Duration,
    starts_idle: bool,
}

fn start_vm(
    config: &RunConfig,
    run_log: &Arc<RunLog>,
    broker_socket: &Path,
    idle: IdlePolicy,
) -> Result<VmHandles> {
    let timings = Arc::new(TimingLog::open(
        &config.layout,
        &config.command,
        config.restore_snapshot.as_deref(),
    )?);
    timings.install_for_libkrun();
    timings.event("dirs.ready");

    let (initrd, rebuilt_initramfs) = initramfs::write_from_agent(
        include_bytes!(env!("LNX_AGENT")),
        config.layout.run_dir.clone(),
    )?;
    timings.event(if rebuilt_initramfs {
        "initramfs.rebuilt"
    } else {
        "initramfs.cached"
    });
    let requested_restore_snapshot = config.restore_snapshot.clone();
    let initramfs_stamp = config.layout.run_dir.join("initramfs.stamp");
    let host_home = host_home_for_cwd(&config.cwd)?;
    let outside_home_cwd = (!config.cwd.starts_with(&host_home)).then(|| config.cwd.clone());
    let shares_stamp = shares_stamp_content(&host_home, outside_home_cwd.as_deref());
    let shares_stamp_path = config.layout.run_dir.join("shares.stamp");
    fs::write(&shares_stamp_path, &shares_stamp)
        .with_context(|| format!("write {}", shares_stamp_path.display()))?;
    let restore_snapshot = if config
        .restore_snapshot
        .as_ref()
        .is_some_and(|snapshot| !snapshot_initramfs_is_compatible(snapshot, &initramfs_stamp))
    {
        timings.event("snapshot.restore.skipped.agent_changed");
        run_log.line("snapshot.restore.skipped reason=agent_changed");
        None
    } else if config
        .restore_snapshot
        .as_ref()
        .is_some_and(|snapshot| !snapshot_shares_are_compatible(snapshot, &shares_stamp))
    {
        timings.event("snapshot.restore.skipped.share_mismatch");
        run_log.line("snapshot.restore.skipped reason=share_mismatch");
        None
    } else if let Some(snapshot) = &config.restore_snapshot {
        match snapshot_vm_config(snapshot) {
            Ok(Some(snapshot_config))
                if !snapshot_config.matches(config.cpus, config.memory_mib) =>
            {
                timings.event("snapshot.restore.skipped.config_mismatch");
                run_log.line(format!(
                    "snapshot.restore.skipped reason=config_mismatch snapshot_cpus={} configured_cpus={} snapshot_memory_mib={} configured_memory_mib={}",
                    snapshot_config.vcpu_count,
                    config.cpus,
                    snapshot_config.memory_mib(),
                    config.memory_mib
                ));
                None
            }
            Ok(_) => config.restore_snapshot.clone(),
            Err(e) => {
                timings.event("snapshot.restore.skipped.unreadable_header");
                run_log.line(format!(
                    "snapshot.restore.skipped reason=unreadable_header error={e:#}"
                ));
                None
            }
        }
    } else {
        config.restore_snapshot.clone()
    };
    if let Some(snapshot) = &requested_restore_snapshot {
        log_snapshot_summary(&run_log, "snapshot.requested", snapshot);
    }

    let socket = config.layout.run_dir.join("lnx-agent.sock");
    let snapshot_socket = config.layout.run_dir.join("lnx-snapshot.sock");
    let control_socket = config.layout.run_dir.join("lnx-control.sock");
    let listener = bind_unix_listener(&socket)?;
    let snapshot_listener = bind_unix_listener(&snapshot_socket)?;
    let control_listener = bind_unix_listener(&control_socket)?;
    let broker_listener = bind_unix_listener(&broker_socket)?;
    timings.event("listeners.ready");
    run_log.line(format!(
        "listeners.ready agent={} snapshot={} control={} broker={}",
        socket.display(),
        snapshot_socket.display(),
        control_socket.display(),
        broker_socket.display()
    ));
    let network = start_gvproxy(&config.layout.run_dir)?;
    timings.event("gvproxy.ready");
    run_log.line(format!(
        "gvproxy.ready socket={}",
        config.layout.run_dir.join("gvproxy.sock").display()
    ));

    KrunContext::set_log_level(2)?;
    let ctx = Arc::new(KrunContext::create()?);
    ctx.set_console_output(&config.layout.console_log)?;
    ctx.set_vm_config(config.cpus, config.memory_mib)?;
    if config.nested_kvm {
        ctx.set_nested_virt(true)?;
    }
    let rootfs = requested_restore_snapshot
        .as_ref()
        .map(|snapshot| snapshot.join("rootfs.ext4"))
        .filter(|path| path.exists())
        .unwrap_or_else(|| config.layout.rootfs.clone());
    log_file_summary(&run_log, "rootfs.selected", &rootfs);
    let rootfs_backend = RootfsBackend::from_env(std::env::var(ROOTFS_BACKEND_ENV).ok())?;
    let root_device = match rootfs_backend {
        RootfsBackend::Pmem => {
            ctx.add_root_pmem(&rootfs)?;
            "/dev/pmem0"
        }
        RootfsBackend::Block => {
            ctx.set_root_disk(&rootfs)?;
            "/dev/vda"
        }
    };
    let guest_home = guest_home(&host_home);
    let guest_cwd = guest_cwd(&config.cwd, &host_home);
    ctx.add_policy_virtiofs("home", &host_home)?;
    set_home_write_allowlist(ctx.as_ref(), &config.cwd, &host_home)?;
    if let Some(cwd) = &outside_home_cwd {
        ctx.add_virtiofs("cwd", cwd, false)?;
    }
    let mut kernel_cmdline =
        format!("console=hvc0 reboot=k panic=1 root={root_device} rw rootfstype=ext4");
    if matches!(rootfs_backend, RootfsBackend::Pmem) {
        kernel_cmdline.push_str(" rootflags=dax");
    }
    if config.nested_kvm {
        kernel_cmdline.push_str(" kvm.allow_unsafe_mappings=1");
    }
    ctx.set_kernel(&config.layout.kernel, Some(&initrd), &kernel_cmdline)?;
    ctx.add_vsock_connector(AGENT_PORT, &socket)?;
    ctx.add_vsock_connector(SNAPSHOT_PORT, &snapshot_socket)?;
    ctx.add_vsock_connector(CONTROL_PORT, &control_socket)?;
    ctx.add_gvproxy_network(&network.socket)?;
    timings.event("krun.devices.configured");

    if let Some(snapshot) = &restore_snapshot {
        ctx.set_snapshot_path(snapshot)?;
        timings.event("snapshot.restore.configured");
        run_log.line(format!(
            "snapshot.restore.configured path={}",
            snapshot.display()
        ));
    }

    ctx.set_workdir("/")?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("host clock is before Unix epoch")?
        .as_secs();
    ctx.set_exec(
        "/init",
        &["--init".to_string()],
        &[
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
            "container=lnx".to_string(),
            format!("LNX_HOST_UNIX_SECS={now}"),
            format!("LNX_ROOT_DEVICE={root_device}"),
            format!("LNX_VIRTIOFS_HOME={guest_home}"),
            outside_home_cwd
                .as_ref()
                .map(|_| format!("LNX_VIRTIOFS_CWD={guest_cwd}"))
                .unwrap_or_else(|| "LNX_VIRTIOFS_CWD=".to_string()),
        ],
    )?;
    timings.event("krun.exec.configured");

    let console_log = config.layout.console_log.clone();
    let vm_ctx = Arc::clone(&ctx);
    let vm_timings = Arc::clone(&timings);
    let vm_run_log = Arc::clone(&run_log);
    let (vm_error_tx, vm_error_rx) = mpsc::channel::<i32>();
    thread::spawn(move || {
        vm_timings.event("krun.start_enter.begin");
        let rc = vm_ctx.start_enter();
        vm_timings.event(&format!("krun.start_enter.return rc={rc}"));
        if rc < 0 {
            vm_run_log.line(format!(
                "krun.start_enter.error rc={rc} error={}",
                krun_return_error(rc)
            ));
            log_console_tail(&vm_run_log, &console_log);
            let _ = vm_error_tx.send(rc);
        } else {
            vm_run_log.line(format!("krun.start_enter.return rc={rc}"));
        }
    });
    timings.event("krun.thread.spawned");

    let snapshot_output = config
        .snapshot_output
        .clone()
        .unwrap_or_else(|| config.layout.snapshot_dir.join("latest"));
    let owner = run_broker_owner(
        listener,
        config.layout.console_log.clone(),
        Arc::clone(&ctx),
        snapshot_output,
        rootfs,
        snapshot_listener,
        control_listener,
        broker_listener,
        broker_socket.to_path_buf(),
        initramfs_stamp,
        restore_snapshot,
        config.forwards.clone(),
        host_home,
        idle,
        Arc::clone(&timings),
        Arc::clone(&run_log),
        vm_error_rx,
    );
    let owner = match owner {
        Ok(owner) => owner,
        Err(e) => {
            timings.event(&format!("restore.owner.error {e:#}"));
            run_log.line(format!("owner.start.error {e:#}"));
            log_console_tail(&run_log, &config.layout.console_log);
            cleanup_runtime_sockets(
                &run_log,
                &[broker_socket, &socket, &snapshot_socket, &control_socket],
            );
            return Err(e);
        }
    };
    Ok(VmHandles {
        owner,
        network,
        timings,
    })
}

pub fn seed_checkpoint_from_base(
    layout: &Layout,
    checkpoint_path: &Path,
    restore_snapshot: Option<&Path>,
    latest_snapshot: &Path,
) -> Result<()> {
    let run_log = RunLog::open(layout)?;
    seed_incremental_snapshot(checkpoint_path, restore_snapshot, latest_snapshot, &run_log)
}

pub fn request_checkpoint(socket: &Path, checkpoint_path: &Path) -> Result<()> {
    let mut stream = connect_broker(socket)?;
    let channel_id = new_request_id()?;
    write_message(
        &mut stream,
        &Message::Checkpoint {
            channel_id,
            path: checkpoint_path.to_string_lossy().into_owned(),
        },
    )?;
    loop {
        match read_message(&mut stream)? {
            Message::CheckpointCreated { channel_id: id } if id == channel_id => return Ok(()),
            Message::Error {
                channel_id: id,
                message,
            } if id == channel_id => bail!("{message}"),
            _ => {}
        }
    }
}

pub fn proxy_stream_to_guest(
    broker_socket: &Path,
    mut local: TcpStream,
    initial_bytes: Vec<u8>,
    guest_host: &str,
    guest_port: u16,
) -> Result<()> {
    let first_response_deadline = Instant::now() + Duration::from_secs(5);
    let (mut broker, channel_id, first_bytes) = 'connect: loop {
        let mut broker = connect_broker(broker_socket)?;
        let channel_id = new_request_id()?;
        write_message(
            &mut broker,
            &Message::OpenTcp {
                channel_id,
                host: guest_host.to_string(),
                port: guest_port,
            },
        )?;
        if !initial_bytes.is_empty() {
            write_message(
                &mut broker,
                &Message::Data {
                    channel_id,
                    bytes: initial_bytes.clone(),
                },
            )?;
        }
        loop {
            match read_message(&mut broker)? {
                Message::Data {
                    channel_id: id,
                    bytes,
                } if id == channel_id => break 'connect (broker, channel_id, bytes),
                Message::Eof { channel_id: id } if id == channel_id => {
                    let _ = local.shutdown(Shutdown::Write);
                }
                Message::Close { channel_id: id } if id == channel_id => return Ok(()),
                Message::Error {
                    channel_id: id,
                    message,
                } if id == channel_id => {
                    if Instant::now() >= first_response_deadline {
                        let _ = local.shutdown(Shutdown::Both);
                        bail!("{message}");
                    }
                    thread::sleep(Duration::from_millis(100));
                    break;
                }
                _ => {}
            }
        }
    };

    local.write_all(&first_bytes)?;

    let mut broker_input = broker.try_clone().context("clone ingress broker stream")?;
    let mut local_reader = local.try_clone().context("clone ingress local stream")?;
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match local_reader.read(&mut buf) {
                Ok(0) => {
                    let _ = write_message(&mut broker_input, &Message::Eof { channel_id });
                    break;
                }
                Ok(n) => {
                    if write_message(
                        &mut broker_input,
                        &Message::Data {
                            channel_id,
                            bytes: buf[..n].to_vec(),
                        },
                    )
                    .is_err()
                    {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(_) => {
                    let _ = write_message(&mut broker_input, &Message::Close { channel_id });
                    break;
                }
            }
        }
    });

    loop {
        match read_message(&mut broker)? {
            Message::Data {
                channel_id: id,
                bytes,
            } if id == channel_id => local.write_all(&bytes)?,
            Message::Eof { channel_id: id } if id == channel_id => {
                let _ = local.shutdown(Shutdown::Write);
            }
            Message::Close { channel_id: id } if id == channel_id => return Ok(()),
            Message::Error {
                channel_id: id,
                message,
            } if id == channel_id => {
                let _ = local.shutdown(Shutdown::Both);
                bail!("{message}");
            }
            _ => {}
        }
    }
}

struct TimingLog {
    path: PathBuf,
    state_path: PathBuf,
    base_unix_nanos: u128,
    state: Mutex<TimingState>,
}

struct RunLog {
    path: PathBuf,
    file: Mutex<fs::File>,
}

struct SnapshotVmConfig {
    memory_bytes: u64,
    vcpu_count: u32,
}

impl SnapshotVmConfig {
    fn memory_mib(&self) -> u64 {
        self.memory_bytes / 1024 / 1024
    }

    fn matches(&self, cpus: u8, memory_mib: u32) -> bool {
        self.vcpu_count == cpus as u32 && self.memory_mib() == memory_mib as u64
    }
}

struct TimingState {
    file: fs::File,
    state_file: fs::File,
}

struct BootstrapLock {
    path: PathBuf,
}

enum BootstrapOutcome {
    Lock(BootstrapLock),
    Status(i32),
}

struct ActiveReservation {
    active: Arc<AtomicUsize>,
    armed: bool,
}

#[derive(Clone)]
struct BrokerChannel {
    tx: mpsc::Sender<Message>,
    active_owned_by_reader: bool,
}

struct CheckpointRequest {
    path: PathBuf,
    reply: mpsc::Sender<Result<(), String>>,
}

impl ActiveReservation {
    fn new(active: Arc<AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::SeqCst);
        Self {
            active,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ActiveReservation {
    fn drop(&mut self) {
        if self.armed {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl BootstrapLock {
    fn try_acquire(path: &Path) -> Result<Option<Self>> {
        match fs::create_dir(path) {
            Ok(()) => {
                fs::write(path.join("owner.pid"), std::process::id().to_string())
                    .with_context(|| format!("write {}", path.join("owner.pid").display()))?;
                Ok(Some(Self {
                    path: path.to_path_buf(),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if bootstrap_lock_is_stale(path)? {
                    let _ = fs::remove_dir_all(path);
                    match fs::create_dir(path) {
                        Ok(()) => {
                            fs::write(path.join("owner.pid"), std::process::id().to_string())
                                .with_context(|| {
                                    format!("write {}", path.join("owner.pid").display())
                                })?;
                            return Ok(Some(Self {
                                path: path.to_path_buf(),
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(e) => {
                            return Err(e).with_context(|| format!("create {}", path.display()));
                        }
                    }
                }
                Ok(None)
            }
            Err(e) => Err(e).with_context(|| format!("create {}", path.display())),
        }
    }
}

impl Drop for BootstrapLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path.join("owner.pid"));
        let _ = fs::remove_dir(&self.path);
    }
}

fn install_signal_handlers() {
    SIGNAL_INIT.call_once(|| unsafe {
        libc::signal(
            libc::SIGINT,
            handle_sigint as *const () as libc::sighandler_t,
        );
    });
}

fn bootstrap_lock_is_stale(path: &Path) -> Result<bool> {
    let owner_pid = path.join("owner.pid");
    if let Ok(pid) = fs::read_to_string(&owner_pid) {
        if let Ok(pid) = pid.trim().parse::<libc::pid_t>() {
            return Ok(!process_alive(pid));
        }
        return Ok(true);
    }

    let modified = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .modified()
        .with_context(|| format!("stat modified time {}", path.display()))?;
    Ok(modified.elapsed().unwrap_or_default() > Duration::from_secs(10))
}

fn process_alive(pid: libc::pid_t) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

impl RunLog {
    fn open(layout: &Layout) -> Result<Self> {
        let path = layout.run_dir.join("lnx.log");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    fn line(&self, message: impl AsRef<str>) {
        let mut file = match self.file.lock() {
            Ok(file) => file,
            Err(_) => return,
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let message = message.as_ref().replace('\r', "").replace('\n', " | ");
        let _ = writeln!(
            file,
            "{}.{:09} {}",
            now.as_secs(),
            now.subsec_nanos(),
            message
        );
    }
}

impl TimingLog {
    fn open(layout: &Layout, command: &[String], restore_snapshot: Option<&Path>) -> Result<Self> {
        let path = layout.run_dir.join("timings.log");
        let state_path = layout.run_dir.join("timings.state");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let mut state_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&state_path)
            .with_context(|| format!("open {}", state_path.display()))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let base_unix_nanos = now.as_nanos();
        write!(state_file, "{base_unix_nanos}")?;
        writeln!(
            file,
            "\nrun pid={} unix={} instance={} restore={} cmd={:?}",
            std::process::id(),
            now.as_secs(),
            layout.instance,
            restore_snapshot
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "false".to_string()),
            command
        )?;
        Ok(Self {
            path,
            state_path,
            base_unix_nanos,
            state: Mutex::new(TimingState { file, state_file }),
        })
    }

    fn install_for_libkrun(&self) {
        // This happens before the libkrun thread is spawned; libkrun reads these
        // process-local values only to append profiling milestones.
        unsafe {
            std::env::set_var("KRUN_TIMINGS_LOG", &self.path);
            std::env::set_var("KRUN_TIMINGS_STATE", &self.state_path);
            std::env::set_var(
                "KRUN_TIMINGS_BASE_UNIX_NANOS",
                self.base_unix_nanos.to_string(),
            );
        }
    }

    fn event(&self, label: &str) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        let now = unix_nanos();
        if lock_file(&state.state_file).is_err() {
            return;
        }
        let delta_nanos = replace_timing_state(&mut state.state_file, self.base_unix_nanos, now)
            .unwrap_or_default();
        let elapsed_nanos = now.saturating_sub(self.base_unix_nanos);

        let line = format!(
            "{:>10.3}ms +{:>9.3}ms {}",
            elapsed_nanos as f64 / 1_000_000.0,
            delta_nanos as f64 / 1_000_000.0,
            label
        );
        let _ = state.file.write_all(line.as_bytes());
        let _ = state.file.write_all(b"\n");
        let _ = unlock_file(&state.state_file);
    }
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn krun_return_error(rc: i32) -> String {
    if rc < 0 {
        std::io::Error::from_raw_os_error(-rc).to_string()
    } else {
        format!("unexpected return code {rc}")
    }
}

fn snapshot_vm_config(snapshot: &Path) -> Result<Option<SnapshotVmConfig>> {
    let path = snapshot.join("vmstate.bin");
    if !path.exists() {
        return Ok(None);
    }
    let mut file = fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
    let mut header = [0u8; 40];
    file.read_exact(&mut header)
        .with_context(|| format!("read {}", path.display()))?;
    if &header[0..8] != b"LKRNSS01" {
        bail!("bad snapshot magic in {}", path.display());
    }
    let version = u32::from_le_bytes(header[8..12].try_into().unwrap());
    if version != SNAPSHOT_VMSTATE_VERSION {
        bail!(
            "unsupported snapshot version {version} in {}",
            path.display()
        );
    }
    Ok(Some(SnapshotVmConfig {
        memory_bytes: u64::from_le_bytes(header[16..24].try_into().unwrap()),
        vcpu_count: u32::from_le_bytes(header[32..36].try_into().unwrap()),
    }))
}

fn snapshot_initramfs_is_compatible(snapshot_path: &Path, current_stamp: &Path) -> bool {
    let Some(snapshot_sha) = stamp_sha256(&snapshot_path.join("initramfs.stamp")) else {
        return false;
    };
    let Some(current_sha) = stamp_sha256(current_stamp) else {
        return false;
    };
    snapshot_sha == current_sha
}

// A restored guest keeps its snapshot-time share mounts, so a snapshot is only
// valid for the same host share roots: a drifted root would silently back the
// old guest mount points with a different host directory.
fn shares_stamp_content(host_home: &Path, outside_home_cwd: Option<&Path>) -> String {
    let mut content = format!("home={}\n", host_home.display());
    if let Some(cwd) = outside_home_cwd {
        content.push_str(&format!("cwd={}\n", cwd.display()));
    }
    content
}

fn snapshot_shares_are_compatible(snapshot_path: &Path, current: &str) -> bool {
    fs::read_to_string(snapshot_path.join("shares.stamp")).is_ok_and(|stamp| stamp == current)
}

fn stamp_sha256(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()?.lines().find_map(|line| {
        let value = line.strip_prefix("sha256=")?;
        Some(value.to_string())
    })
}

fn replace_timing_state(file: &mut fs::File, base: u128, now: u128) -> std::io::Result<u128> {
    file.seek(SeekFrom::Start(0))?;
    let mut raw = String::new();
    file.read_to_string(&mut raw)?;
    let previous = raw.trim().parse::<u128>().unwrap_or(base);
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    write!(file, "{now}")?;
    Ok(now.saturating_sub(previous))
}

fn lock_file(file: &fs::File) -> std::io::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn unlock_file(file: &fs::File) -> std::io::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

struct Gvproxy {
    socket: PathBuf,
    child: Child,
}

impl Drop for Gvproxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.socket);
        if let (Some(parent), Some(name)) = (self.socket.parent(), self.socket.file_name()) {
            let krun_socket = parent.join(format!("{}-krun.sock", name.to_string_lossy()));
            let _ = fs::remove_file(krun_socket);
        }
    }
}

fn start_gvproxy(run_dir: &Path) -> Result<Gvproxy> {
    let gvproxy = std::env::var_os("GVPROXY_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/homebrew/opt/podman/libexec/podman/gvproxy"));
    if !gvproxy.exists() {
        bail!(
            "gvproxy not found at {}. Install podman with Homebrew or set GVPROXY_PATH.",
            gvproxy.display()
        );
    }

    let socket = run_dir.join("gvproxy.sock");
    let log = run_dir.join("gvproxy.log");
    let _ = fs::remove_file(&socket);
    let log_file = fs::File::create(&log).with_context(|| format!("create {}", log.display()))?;
    let ssh_port = unused_local_port().context("find unused localhost port for gvproxy ssh")?;
    let child = Command::new(&gvproxy)
        .arg("--listen-vfkit")
        .arg(format!("unixgram:{}", socket.display()))
        .arg("--ssh-port")
        .arg(ssh_port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log_file)
        .spawn()
        .with_context(|| format!("start {}", gvproxy.display()))?;

    // Generous: in a freshly restored nested guest, process startup and
    // socket creation on virtiofs can take well over the usual instant.
    wait_for_path(&socket, Duration::from_secs(30))
        .with_context(|| format!("gvproxy did not create {}", socket.display()))?;
    Ok(Gvproxy { socket, child })
}

fn unused_local_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn wait_for_path(path: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(5));
    }
    bail!("timed out waiting for {}", path.display())
}

fn bind_unix_listener(path: &Path) -> Result<UnixListener> {
    let mut last_error = None;
    for _ in 0..20 {
        let _ = fs::remove_file(path);
        match UnixListener::bind(path) {
            Ok(listener) => return Ok(listener),
            Err(e) if e.kind() == ErrorKind::AddrInUse => {
                last_error = Some(e);
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e).with_context(|| format!("listen on {}", path.display())),
        }
    }
    Err(last_error.unwrap_or_else(|| ErrorKind::AddrInUse.into()))
        .with_context(|| format!("listen on {}", path.display()))
}

pub(crate) fn write_message(stream: &mut UnixStream, message: &Message) -> Result<()> {
    let bytes = postcard::to_allocvec(message).context("encode protocol message")?;
    if bytes.len() > MAX_MESSAGE_SIZE as usize {
        bail!("protocol message too large: {}", bytes.len());
    }
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}

pub(crate) fn read_message(stream: &mut UnixStream) -> Result<Message> {
    let len = read_u32(stream).context("read protocol length")?;
    if len > MAX_MESSAGE_SIZE {
        bail!("protocol message too large: {len}");
    }
    let mut bytes = vec![0u8; len as usize];
    stream
        .read_exact(&mut bytes)
        .with_context(|| format!("read protocol body ({len} bytes)"))?;
    postcard::from_bytes(&bytes).context("decode protocol message")
}

fn read_message_interruptible(stream: &mut UnixStream) -> Result<Option<Message>> {
    stream
        .set_read_timeout(Some(INTERRUPT_POLL_TIMEOUT))
        .context("set interruptible read timeout")?;
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            let _ = stream.set_read_timeout(None);
            return Ok(None);
        }
        match read_message(stream) {
            Ok(message) => {
                let _ = stream.set_read_timeout(None);
                return Ok(Some(message));
            }
            Err(e) if is_timeout_error(&e) => {}
            Err(e) => {
                let _ = stream.set_read_timeout(None);
                return Err(e);
            }
        }
    }
}

pub(crate) fn is_timeout_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .map(|io| {
                matches!(
                    io.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                )
            })
            .unwrap_or(false)
    })
}

fn acquire_bootstrap_or_run_client(
    lock_path: &Path,
    socket: &Path,
    command: &[String],
    cwd: &Path,
    run_as_root: bool,
    run_log: &RunLog,
) -> Result<BootstrapOutcome> {
    let start = Instant::now();
    let mut logged_wait = false;
    loop {
        if let Some(lock) = BootstrapLock::try_acquire(lock_path)? {
            run_log.line(format!(
                "bootstrap.lock.acquired path={}",
                lock_path.display()
            ));
            return Ok(BootstrapOutcome::Lock(lock));
        }
        if !logged_wait {
            run_log.line(format!("bootstrap.lock.busy path={}", lock_path.display()));
            logged_wait = true;
        }
        if let Some(status) =
            run_existing_broker_client(socket, command, cwd, run_as_root, Some(run_log))?
        {
            return Ok(BootstrapOutcome::Status(status));
        }
        if start.elapsed() > Duration::from_secs(120) {
            run_log.line(format!(
                "bootstrap.lock.timeout path={}",
                lock_path.display()
            ));
            bail!("timed out waiting for {}", lock_path.display());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn run_existing_broker_client(
    socket: &Path,
    command: &[String],
    cwd: &Path,
    run_as_root: bool,
    run_log: Option<&RunLog>,
) -> Result<Option<i32>> {
    match connect_broker(socket) {
        Ok(stream) => {
            if let Some(log) = run_log {
                log.line(format!(
                    "broker.client.connected socket={}",
                    socket.display()
                ));
            }
            run_broker_session(stream, command, cwd, run_as_root).map(Some)
        }
        Err(e) => {
            if socket.exists() {
                if let Some(log) = run_log {
                    log.line(format!(
                        "broker.client.connect_failed socket={} error={e:#}",
                        socket.display()
                    ));
                }
            }
            Ok(None)
        }
    }
}

pub(crate) fn connect_broker(socket: &Path) -> Result<UnixStream> {
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("connect {}", socket.display()))?;
    stream
        .set_nonblocking(false)
        .context("set broker stream blocking")?;
    write_message(
        &mut stream,
        &Message::Hello {
            version: PROTOCOL_VERSION,
        },
    )?;
    match read_message(&mut stream).context("read broker hello")? {
        Message::Hello { version } if version == PROTOCOL_VERSION => {}
        other => bail!("bad broker hello: {other:?}"),
    }
    Ok(stream)
}

fn run_broker_session(
    mut stream: UnixStream,
    command: &[String],
    cwd: &Path,
    run_as_root: bool,
) -> Result<i32> {
    INTERRUPTED.store(false, Ordering::SeqCst);
    let channel_id = new_request_id()?;
    let host_home = host_home_for_cwd(cwd)?;
    let use_pty = should_request_pty();
    let raw_mode = if use_pty { RawTerminal::enter() } else { None };
    let (term, colorterm, rows, cols) = if use_pty {
        (
            std::env::var("TERM")
                .ok()
                .filter(|value| !value.is_empty() && value != "dumb")
                .unwrap_or_else(|| "xterm-256color".to_string()),
            std::env::var("COLORTERM").unwrap_or_default(),
            terminal_size().0,
            terminal_size().1,
        )
    } else {
        (String::new(), String::new(), 1, 1)
    };
    write_message(
        &mut stream,
        &Message::OpenExec {
            channel_id,
            argv: command.to_vec(),
            cwd: guest_cwd(cwd, &host_home),
            pty: use_pty,
            term,
            colorterm,
            rows,
            cols,
            uid: if run_as_root { 0 } else { unsafe { libc::getuid() } },
            gid: if run_as_root { 0 } else { unsafe { libc::getgid() } },
            group: if run_as_root { String::new() } else { host_group_name() },
            env: forwarded_exec_env(),
        },
    )?;

    if !is_tty(std::io::stdin().as_raw_fd()) {
        let mut bytes = Vec::new();
        let mut input = [0u8; 8192];
        loop {
            match std::io::stdin().read(&mut input) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&input[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).context("read stdin"),
            }
        }
        if !bytes.is_empty() {
            write_message(&mut stream, &Message::Data { channel_id, bytes })?;
        }
        write_message(&mut stream, &Message::Eof { channel_id })?;
        loop {
            let Some(message) = read_message_interruptible(&mut stream)? else {
                let _ = write_message(&mut stream, &Message::Eof { channel_id });
                let _ = write_message(&mut stream, &Message::Close { channel_id });
                return Ok(130);
            };
            match message {
                Message::Data {
                    channel_id: id,
                    bytes,
                } if id == channel_id => {
                    std::io::stdout().write_all(&bytes)?;
                    std::io::stdout().flush()?;
                }
                Message::Stderr {
                    channel_id: id,
                    bytes,
                } if id == channel_id => {
                    std::io::stderr().write_all(&bytes)?;
                    std::io::stderr().flush()?;
                }
                Message::ExitStatus {
                    channel_id: id,
                    status,
                } if id == channel_id => {
                    return Ok(status);
                }
                Message::Error {
                    channel_id: id,
                    message,
                } if id == channel_id => {
                    bail!("{message}");
                }
                _ => {}
            }
        }
    }

    let mut input_stream = stream
        .try_clone()
        .context("clone broker stream for stdin")?;
    thread::spawn(move || {
        let stdin_handle = std::io::stdin();
        let mut stdin = stdin_handle.lock();
        let mut input = [0u8; 8192];
        loop {
            match stdin.read(&mut input) {
                Ok(0) => {
                    let _ = write_message(&mut input_stream, &Message::Eof { channel_id });
                    break;
                }
                Ok(n) => {
                    let result = write_message(
                        &mut input_stream,
                        &Message::Data {
                            channel_id,
                            bytes: input[..n].to_vec(),
                        },
                    );
                    if result.is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });

    loop {
        let Some(message) = read_message_interruptible(&mut stream)? else {
            let _ = write_message(&mut stream, &Message::Eof { channel_id });
            let _ = write_message(&mut stream, &Message::Close { channel_id });
            drop(raw_mode);
            return Ok(130);
        };
        match message {
            Message::Data {
                channel_id: id,
                bytes,
            } if id == channel_id => {
                std::io::stdout().write_all(&bytes)?;
                std::io::stdout().flush()?;
            }
            Message::Stderr {
                channel_id: id,
                bytes,
            } if id == channel_id => {
                std::io::stderr().write_all(&bytes)?;
                std::io::stderr().flush()?;
            }
            Message::ExitStatus {
                channel_id: id,
                status,
            } if id == channel_id => {
                drop(raw_mode);
                return Ok(status);
            }
            Message::Error {
                channel_id: id,
                message,
            } if id == channel_id => {
                bail!("{message}");
            }
            _ => {}
        }
    }
}

/// Name of the host's primary group, so the guest can label the matching gid
/// the way the host does (e.g. gid 20 is `staff` on macOS, `dialout` on
/// Ubuntu). Empty when the lookup fails; the guest then keeps its own name.
fn host_group_name() -> String {
    let gid = unsafe { libc::getgid() };
    let mut buf = [0u8; 1024];
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::group = std::ptr::null_mut();
    let rc = unsafe {
        libc::getgrgid_r(
            gid,
            &mut grp,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(grp.gr_name) }
        .to_string_lossy()
        .into_owned()
}

fn forwarded_exec_env() -> Vec<(String, String)> {
    const EXACT: &[&str] = &[
        "TERM",
        "COLORTERM",
        "LANG",
        "LANGUAGE",
        "TZ",
        "NO_COLOR",
        "CLICOLOR",
        "CLICOLOR_FORCE",
    ];
    let mut env = Vec::new();
    for key in EXACT {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                env.push(((*key).to_string(), value));
            }
        }
    }
    for (key, value) in std::env::vars() {
        if key.starts_with("LC_") && !value.is_empty() && !env.iter().any(|(k, _)| k == &key) {
            env.push((key, value));
        }
    }
    env
}

fn run_broker_client_retry(
    socket: &Path,
    command: &[String],
    cwd: &Path,
    run_as_root: bool,
    timeout: Duration,
) -> Result<i32> {
    let start = Instant::now();
    let mut last = None;
    while start.elapsed() < timeout {
        match connect_broker(socket) {
            Ok(stream) => return run_broker_session(stream, command, cwd, run_as_root),
            Err(e) => {
                last = Some(e);
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
    match last {
        Some(e) => Err(e),
        None => bail!("timed out connecting to broker"),
    }
}

fn run_broker_owner(
    listener: UnixListener,
    console_log: PathBuf,
    ctx: Arc<KrunContext>,
    snapshot_path: PathBuf,
    rootfs: PathBuf,
    snapshot_listener: UnixListener,
    _control_listener: UnixListener,
    broker_listener: UnixListener,
    broker_socket: PathBuf,
    initramfs_stamp: PathBuf,
    restore_snapshot: Option<PathBuf>,
    forwards: Vec<PortForward>,
    host_home: PathBuf,
    idle: IdlePolicy,
    timings: Arc<TimingLog>,
    run_log: Arc<RunLog>,
    vm_error_rx: mpsc::Receiver<i32>,
) -> Result<thread::JoinHandle<()>> {
    listener
        .set_nonblocking(true)
        .context("set lnx-agent listener nonblocking")?;
    let agent_timeout = agent_accept_timeout_from_env(std::env::var("LNX_AGENT_TIMEOUT_MS").ok());
    timings.event("agent.accept.begin");
    run_log.line(format!(
        "agent.accept.begin timeout_ms={} restore={}",
        agent_timeout.as_millis(),
        restore_snapshot
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "false".to_string())
    ));
    let mut agent_stream =
        match accept_agent_hello(&listener, agent_timeout, &timings, &run_log, &vm_error_rx) {
            Ok(stream) => stream,
            Err(e) => {
                run_log.line(format!("agent.accept.error {e:#}"));
                log_console_tail(&run_log, &console_log);
                return Err(e).with_context(|| console_hint(&console_log));
            }
        };
    write_message(
        &mut agent_stream,
        &Message::Hello {
            version: PROTOCOL_VERSION,
        },
    )?;

    let (agent_tx, agent_rx) = mpsc::channel::<Message>();
    let (checkpoint_tx, checkpoint_rx) = mpsc::channel::<CheckpointRequest>();
    let (snapshot_exit_tx, snapshot_exit_rx) = mpsc::channel::<u64>();
    let client_senders = Arc::new(Mutex::new(HashMap::<u64, BrokerChannel>::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let seen_active = Arc::new(AtomicBool::new(idle.starts_idle));

    let mut agent_writer = agent_stream
        .try_clone()
        .context("clone lnx-agent stream for writer")?;
    thread::spawn(move || {
        while let Ok(message) = agent_rx.recv() {
            if write_message(&mut agent_writer, &message).is_err() {
                break;
            }
        }
    });

    let mut agent_reader = agent_stream;
    let reader_clients = Arc::clone(&client_senders);
    let reader_active = Arc::clone(&active);
    let reader_snapshot_exit_tx = snapshot_exit_tx.clone();
    let reader_log = Arc::clone(&run_log);
    thread::spawn(move || {
        let reader_err = loop {
            let message = match read_message(&mut agent_reader) {
                Ok(message) => message,
                Err(e) => break e,
            };
            let channel_id = match &message {
                Message::Data { channel_id, .. }
                | Message::Stderr { channel_id, .. }
                | Message::Eof { channel_id }
                | Message::ExitStatus { channel_id, .. }
                | Message::Close { channel_id }
                | Message::Error { channel_id, .. }
                | Message::SnapshotExit { channel_id } => Some(*channel_id),
                _ => None,
            };
            if let Message::SnapshotExit { channel_id } = message {
                let _ = reader_snapshot_exit_tx.send(channel_id);
                continue;
            }
            if let Some(channel_id) = channel_id {
                let channel = reader_clients
                    .lock()
                    .ok()
                    .and_then(|clients| clients.get(&channel_id).cloned());
                if let Some(channel) = channel {
                    let _ = channel.tx.send(message.clone());
                }
                if matches!(message, Message::Close { .. }) {
                    let decrement = reader_clients
                        .lock()
                        .ok()
                        .and_then(|mut clients| clients.remove(&channel_id))
                        .map(|channel| channel.active_owned_by_reader)
                        .unwrap_or(false);
                    if decrement {
                        reader_active.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            }
        };
        if let Ok(mut clients) = reader_clients.lock() {
            let dropped = clients
                .values()
                .filter(|channel| channel.active_owned_by_reader)
                .count();
            clients.clear();
            if dropped > 0 {
                reader_active.fetch_sub(dropped, Ordering::SeqCst);
            }
            reader_log.line(format!(
                "broker.agent.reader_eof dropped_channels={dropped} error={reader_err:#}"
            ));
        }
    });

    broker_listener
        .set_nonblocking(true)
        .context("set broker listener nonblocking")?;
    let owner_timings = Arc::clone(&timings);
    let owner_log = Arc::clone(&run_log);
    let force_full_snapshot = restore_snapshot.is_none();
    let broker_idle_ttl = idle.ttl;
    for forward in forwards {
        start_forward_listener(
            forward,
            agent_tx.clone(),
            Arc::clone(&client_senders),
            Arc::clone(&active),
            Arc::clone(&seen_active),
            Arc::clone(&run_log),
        )?;
    }
    Ok(thread::spawn(move || {
        owner_timings.event("broker.ready");
        owner_log.line(format!(
            "broker.ready socket={} idle_ttl_ms={}",
            broker_socket.display(),
            broker_idle_ttl.as_millis()
        ));
        let mut idle_deadline: Option<Instant> = None;
        loop {
            match broker_listener.accept() {
                Ok((client, _)) => {
                    owner_log.line("broker.client.accepted");
                    let tx = agent_tx.clone();
                    let clients = Arc::clone(&client_senders);
                    // Reserve at accept time so the idle grace period cannot
                    // expire between a client connecting and its first message.
                    let reservation = ActiveReservation::new(Arc::clone(&active));
                    let seen = Arc::clone(&seen_active);
                    let checkpoint_tx = checkpoint_tx.clone();
                    let client_log = Arc::clone(&owner_log);
                    let client_ctx = Arc::clone(&ctx);
                    let client_host_home = host_home.clone();
                    thread::spawn(move || {
                        if let Err(e) = handle_broker_client(
                            client,
                            tx,
                            checkpoint_tx,
                            clients,
                            reservation,
                            seen,
                            client_ctx,
                            client_host_home,
                            Arc::clone(&client_log),
                        ) {
                            client_log.line(format!("broker.client.error {e:#}"));
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    while let Ok(request) = checkpoint_rx.try_recv() {
                        owner_timings.event("checkpoint.request.begin");
                        owner_log.line(format!(
                            "checkpoint.request path={}",
                            request.path.display()
                        ));
                        let result = seed_incremental_snapshot(
                            &request.path,
                            restore_snapshot.as_deref(),
                            &snapshot_path,
                            &owner_log,
                        )
                        .and_then(|()| {
                            ctx.snapshot_with_file_copy(&request.path, &rootfs, "rootfs.ext4")?;
                            copy_snapshot_stamp(&request.path, &initramfs_stamp)
                        })
                        .map_err(|e| format!("{e:#}"));
                        if result.is_ok() {
                            owner_log
                                .line(format!("checkpoint.done path={}", request.path.display()));
                            log_snapshot_summary(&owner_log, "checkpoint", &request.path);
                        }
                        let _ = request.reply.send(result);
                    }
                    while let Ok(channel_id) = snapshot_exit_rx.try_recv() {
                        owner_timings.event("snapshot_exit.request.begin");
                        owner_log.line(format!(
                            "snapshot_exit.request channel_id={channel_id} path={}",
                            snapshot_path.display()
                        ));
                        let result = snapshot_with_file_copy_full(
                            &ctx,
                            &snapshot_path,
                            &rootfs,
                            &initramfs_stamp,
                        );
                        match result {
                            Ok(()) => {
                                owner_log.line(format!(
                                    "snapshot_exit.done channel_id={channel_id} path={}",
                                    snapshot_path.display()
                                ));
                                log_snapshot_summary(&owner_log, "snapshot.latest", &snapshot_path);
                                let _ = agent_tx.send(Message::CheckpointCreated { channel_id });
                            }
                            Err(e) => {
                                owner_log.line(format!(
                                    "snapshot_exit.error channel_id={channel_id} error={e:#}"
                                ));
                                let _ = agent_tx.send(Message::Error {
                                    channel_id,
                                    message: format!("snapshot-exit failed: {e:#}"),
                                });
                            }
                        }
                    }
                    if active.load(Ordering::SeqCst) > 0 {
                        idle_deadline = None;
                    } else if seen_active.load(Ordering::SeqCst) {
                        if broker_idle_ttl.is_zero() {
                            break;
                        }
                        let deadline =
                            idle_deadline.get_or_insert_with(|| Instant::now() + broker_idle_ttl);
                        if Instant::now() >= *deadline {
                            break;
                        }
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        let _ = fs::remove_file(&broker_socket);
        drop(broker_listener);
        owner_timings.event("snapshot.request.guest");
        owner_log.line(format!(
            "snapshot.request.guest path={} full={force_full_snapshot}",
            snapshot_path.display()
        ));
        let _ = agent_tx.send(Message::SnapshotReady);
        match serve_snapshot(
            snapshot_listener,
            &ctx,
            &snapshot_path,
            &rootfs,
            &initramfs_stamp,
            force_full_snapshot,
            &owner_timings,
        ) {
            Ok(()) => {
                owner_log.line("snapshot.done");
                log_snapshot_summary(&owner_log, "snapshot.latest", &snapshot_path);
            }
            Err(e) => owner_log.line(format!("snapshot.error {e:#}")),
        }
    }))
}

fn broker_idle_ttl() -> Duration {
    broker_idle_ttl_from_env(std::env::var("LNX_BROKER_IDLE_TTL_MS").ok().as_deref())
}

fn broker_idle_ttl_from_env(value: Option<&str>) -> Duration {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_BROKER_IDLE_TTL)
}

fn owner_idle_ttl() -> Duration {
    owner_idle_ttl_from_env(std::env::var("LNX_BROKER_IDLE_TTL_MS").ok().as_deref())
}

fn owner_idle_ttl_from_env(value: Option<&str>) -> Duration {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_OWNER_IDLE_TTL)
        .max(MIN_OWNER_IDLE_TTL)
}

fn forward_spec(forward: &PortForward) -> String {
    format!(
        "{}:{}:{}:{}",
        forward.listen_host, forward.listen_port, forward.guest_host, forward.guest_port
    )
}

fn spawn_owner_process(config: &RunConfig, run_log: &RunLog) -> Result<Child> {
    let exe = std::env::current_exe().context("current executable")?;
    let log_path = config.layout.run_dir.join("owner.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let mut command = Command::new(exe);
    command
        .arg("--instance")
        .arg(&config.layout.instance)
        .arg("--kernel")
        .arg(&config.layout.kernel)
        .arg("--rootfs")
        .arg(&config.layout.rootfs)
        .arg("--cpus")
        .arg(config.cpus.to_string())
        .arg("--memory-mib")
        .arg(config.memory_mib.to_string());
    if config.nested_kvm {
        command.arg("--nested-kvm");
    }
    for forward in &config.forwards {
        command.arg("--forward").arg(forward_spec(forward));
    }
    command.arg("_vm-owner").arg("--cwd").arg(&config.cwd);
    if let Some(snapshot) = &config.restore_snapshot {
        command.arg("--restore").arg(snapshot);
    }
    command
        .stdin(Stdio::null())
        .stdout(log.try_clone().context("clone owner log handle")?)
        .stderr(log)
        .process_group(0);
    let child = command.spawn().context("spawn lnx _vm-owner")?;
    run_log.line(format!("owner.spawned pid={}", child.id()));
    Ok(child)
}

fn run_broker_client_awaiting_owner(
    socket: &Path,
    command: &[String],
    cwd: &Path,
    owner: &mut Child,
    config: &RunConfig,
    layout: &Layout,
    run_log: &RunLog,
) -> Result<i32> {
    let deadline = Instant::now() + OWNER_BOOT_TIMEOUT;
    let mut last = None;
    let mut owner_config = config.clone();
    while Instant::now() < deadline {
        if INTERRUPTED.load(Ordering::SeqCst) {
            return Ok(130);
        }
        match connect_broker(socket) {
            Ok(stream) => return run_broker_session(stream, command, cwd, config.run_as_root),
            Err(e) => last = Some(e),
        }
        if let Some(status) = owner.try_wait().context("check lnx _vm-owner")? {
            // An owner that exits zero lost the bootstrap race to another
            // owner, so keep retrying until that one's broker comes up.
            if status.success() {
                run_log.line("owner.exited.early status=0 retry=spawn");
                *owner = spawn_owner_process(&owner_config, run_log)?;
            } else if status.code() == Some(EXIT_RESTORE_FAILED)
                && owner_config.restore_snapshot.is_some()
            {
                run_log.line("snapshot.restore.skipped reason=start_failed retry=cold_boot");
                owner_config.restore_snapshot = None;
                *owner = spawn_owner_process(&owner_config, run_log)?;
            } else {
                run_log.line(format!("owner.exited.early status={status}"));
                bail!(
                    "lnx VM owner exited with {status} before the broker came up{}{}",
                    owner_log_hint(layout),
                    console_hint(&layout.console_log)
                );
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    match last {
        Some(e) => Err(e).with_context(|| {
            format!(
                "timed out waiting for the lnx VM owner broker{}",
                console_hint(&layout.console_log)
            )
        }),
        None => bail!("timed out waiting for the lnx VM owner broker"),
    }
}

fn acquire_bootstrap_for_owner(
    lock_path: &Path,
    broker_socket: &Path,
    run_log: &RunLog,
) -> Result<Option<BootstrapLock>> {
    let start = Instant::now();
    let mut logged_wait = false;
    loop {
        if let Some(lock) = BootstrapLock::try_acquire(lock_path)? {
            run_log.line(format!(
                "owner.bootstrap.lock.acquired path={}",
                lock_path.display()
            ));
            return Ok(Some(lock));
        }
        if !logged_wait {
            run_log.line(format!(
                "owner.bootstrap.lock.busy path={}",
                lock_path.display()
            ));
            logged_wait = true;
        }
        if connect_broker(broker_socket).is_ok() {
            return Ok(None);
        }
        if start.elapsed() > Duration::from_secs(120) {
            run_log.line(format!(
                "owner.bootstrap.lock.timeout path={}",
                lock_path.display()
            ));
            bail!("timed out waiting for {}", lock_path.display());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn owner_log_hint(layout: &Layout) -> String {
    let path = layout.run_dir.join("owner.log");
    let Ok(bytes) = fs::read(&path) else {
        return String::new();
    };
    if bytes.is_empty() {
        return String::new();
    }
    let start = bytes.len().saturating_sub(2048);
    format!(
        "\n\nVM owner log ({}):\n{}",
        path.display(),
        String::from_utf8_lossy(&bytes[start..]).trim_end()
    )
}

fn agent_accept_timeout_from_env(value: Option<String>) -> Duration {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_AGENT_ACCEPT_TIMEOUT)
}

fn handle_broker_client(
    mut client: UnixStream,
    agent_tx: mpsc::Sender<Message>,
    checkpoint_tx: mpsc::Sender<CheckpointRequest>,
    clients: Arc<Mutex<HashMap<u64, BrokerChannel>>>,
    mut active_reservation: ActiveReservation,
    seen_active: Arc<AtomicBool>,
    ctx: Arc<KrunContext>,
    host_home: PathBuf,
    run_log: Arc<RunLog>,
) -> Result<()> {
    client
        .set_nonblocking(false)
        .context("set broker client blocking")?;
    match read_message(&mut client)? {
        Message::Hello { version } if version == PROTOCOL_VERSION => {}
        other => bail!("bad client hello: {other:?}"),
    }
    write_message(
        &mut client,
        &Message::Hello {
            version: PROTOCOL_VERSION,
        },
    )?;
    let first = read_message(&mut client)?;
    if let Message::Checkpoint { channel_id, path } = first {
        let (reply_tx, reply_rx) = mpsc::channel();
        checkpoint_tx
            .send(CheckpointRequest {
                path: PathBuf::from(path),
                reply: reply_tx,
            })
            .context("send checkpoint request to owner")?;
        match reply_rx.recv().context("receive checkpoint result")? {
            Ok(()) => write_message(&mut client, &Message::CheckpointCreated { channel_id })?,
            Err(message) => write_message(
                &mut client,
                &Message::Error {
                    channel_id,
                    message,
                },
            )?,
        }
        return Ok(());
    };
    let channel_id = match &first {
        Message::OpenExec { channel_id, .. } | Message::OpenTcp { channel_id, .. } => *channel_id,
        _ => bail!("client did not open a channel"),
    };
    run_log.line(format!("broker.client.open channel={channel_id:016x}"));
    if let Message::OpenExec { cwd, .. } = &first {
        set_home_write_allowlist(ctx.as_ref(), Path::new(cwd), &host_home)?;
    }
    seen_active.store(true, Ordering::SeqCst);
    let (to_client_tx, to_client_rx) = mpsc::channel::<Message>();
    clients
        .lock()
        .map_err(|_| anyhow::anyhow!("lock broker clients"))?
        .insert(
            channel_id,
            BrokerChannel {
                tx: to_client_tx,
                active_owned_by_reader: true,
            },
        );
    if let Err(e) = agent_tx.send(first) {
        if let Ok(mut clients) = clients.lock() {
            clients.remove(&channel_id);
        }
        return Err(e).context("send open exec to agent");
    }
    active_reservation.disarm();
    let mut writer = client.try_clone().context("clone broker client")?;
    thread::spawn(move || {
        while let Ok(message) = to_client_rx.recv() {
            if write_message(&mut writer, &message).is_err() {
                break;
            }
        }
    });
    loop {
        match read_message(&mut client) {
            Ok(message) => {
                match &message {
                    Message::Data { channel_id, bytes } => run_log.line(format!(
                        "broker.client.data channel={channel_id:016x} bytes={}",
                        bytes.len()
                    )),
                    Message::Eof { channel_id } => {
                        run_log.line(format!("broker.client.eof channel={channel_id:016x}"))
                    }
                    Message::Close { channel_id } => {
                        run_log.line(format!("broker.client.close channel={channel_id:016x}"))
                    }
                    _ => {}
                }
                if let Err(e) = agent_tx.send(message) {
                    // The agent writer is gone; this channel can never
                    // complete, so release its idle-accounting slot.
                    let owned = clients
                        .lock()
                        .ok()
                        .and_then(|mut clients| clients.remove(&channel_id))
                        .map(|channel| channel.active_owned_by_reader)
                        .unwrap_or(false);
                    if owned {
                        active_reservation.active.fetch_sub(1, Ordering::SeqCst);
                    }
                    return Err(e).context("send client message to agent");
                }
            }
            Err(_) => {
                run_log.line(format!("broker.client.read_eof channel={channel_id:016x}"));
                let _ = agent_tx.send(Message::Eof { channel_id });
                return Ok(());
            }
        }
    }
}

fn start_forward_listener(
    forward: PortForward,
    agent_tx: mpsc::Sender<Message>,
    clients: Arc<Mutex<HashMap<u64, BrokerChannel>>>,
    active: Arc<AtomicUsize>,
    seen_active: Arc<AtomicBool>,
    run_log: Arc<RunLog>,
) -> Result<()> {
    let listener = TcpListener::bind((forward.listen_host.as_str(), forward.listen_port))
        .with_context(|| format!("listen on {}:{}", forward.listen_host, forward.listen_port))?;
    listener.set_nonblocking(true).with_context(|| {
        format!(
            "set forward listener nonblocking {}:{}",
            forward.listen_host, forward.listen_port
        )
    })?;
    run_log.line(format!(
        "forward.listen host={} port={} guest_host={} guest_port={}",
        forward.listen_host, forward.listen_port, forward.guest_host, forward.guest_port
    ));
    thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((stream, peer)) => {
                    let _ = stream.set_nonblocking(false);
                    run_log.line(format!(
                        "forward.accept listen_port={} peer={peer}",
                        forward.listen_port
                    ));
                    let connection_forward = forward.clone();
                    let connection_tx = agent_tx.clone();
                    let connection_clients = Arc::clone(&clients);
                    let connection_active = Arc::clone(&active);
                    let connection_seen = Arc::clone(&seen_active);
                    let connection_log = Arc::clone(&run_log);
                    thread::spawn(move || {
                        if let Err(e) = handle_forward_connection(
                            stream,
                            connection_forward,
                            connection_tx,
                            connection_clients,
                            connection_active,
                            connection_seen,
                            Arc::clone(&connection_log),
                        ) {
                            connection_log.line(format!("forward.connection.error {e:#}"));
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    run_log.line(format!("forward.accept.error {e:#}"));
                    break;
                }
            }
        }
    });
    Ok(())
}

fn handle_forward_connection(
    mut local: TcpStream,
    forward: PortForward,
    agent_tx: mpsc::Sender<Message>,
    clients: Arc<Mutex<HashMap<u64, BrokerChannel>>>,
    active: Arc<AtomicUsize>,
    seen_active: Arc<AtomicBool>,
    run_log: Arc<RunLog>,
) -> Result<()> {
    let reservation = ActiveReservation::new(active);
    seen_active.store(true, Ordering::SeqCst);
    let channel_id = new_request_id()?;
    let (to_forward_tx, to_forward_rx) = mpsc::channel::<Message>();
    clients
        .lock()
        .map_err(|_| anyhow::anyhow!("lock broker clients"))?
        .insert(
            channel_id,
            BrokerChannel {
                tx: to_forward_tx,
                active_owned_by_reader: false,
            },
        );
    if let Err(e) = agent_tx.send(Message::OpenTcp {
        channel_id,
        host: forward.guest_host,
        port: forward.guest_port,
    }) {
        if let Ok(mut clients) = clients.lock() {
            clients.remove(&channel_id);
        }
        return Err(e).context("send open tcp to agent");
    }

    let mut local_reader = local.try_clone().context("clone local forward stream")?;
    let input_tx = agent_tx.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match local_reader.read(&mut buf) {
                Ok(0) => {
                    let _ = input_tx.send(Message::Eof { channel_id });
                    break;
                }
                Ok(n) => {
                    if input_tx
                        .send(Message::Data {
                            channel_id,
                            bytes: buf[..n].to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(_) => {
                    let _ = input_tx.send(Message::Close { channel_id });
                    break;
                }
            }
        }
    });

    while let Ok(message) = to_forward_rx.recv() {
        match message {
            Message::Data {
                channel_id: id,
                bytes,
            } if id == channel_id => {
                if local.write_all(&bytes).is_err() {
                    run_log.line(format!("forward.local_write.error channel={channel_id}"));
                    break;
                }
            }
            Message::Eof { channel_id: id } if id == channel_id => {
                let _ = local.shutdown(Shutdown::Write);
            }
            Message::Close { channel_id: id } if id == channel_id => {
                break;
            }
            Message::Error {
                channel_id: id,
                message,
            } if id == channel_id => bail!("{message}"),
            _ => {}
        }
    }
    if let Ok(mut clients) = clients.lock() {
        clients.remove(&channel_id);
    }
    let _ = agent_tx.send(Message::Close { channel_id });
    thread::sleep(Duration::from_secs(60));
    drop(reservation);
    Ok(())
}

fn accept_unix(listener: &UnixListener, timeout: Duration) -> Result<UnixStream> {
    accept_unix_with_progress(listener, timeout, None, None)
}

fn accept_agent_hello(
    listener: &UnixListener,
    timeout: Duration,
    timings: &TimingLog,
    run_log: &RunLog,
    vm_error_rx: &mpsc::Receiver<i32>,
) -> Result<UnixStream> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let remaining = timeout.saturating_sub(start.elapsed());
        let mut stream = accept_unix_with_progress(
            listener,
            remaining,
            Some((timings, "agent.accept.waiting")),
            Some(vm_error_rx),
        )?;
        stream
            .set_nonblocking(false)
            .context("set lnx-agent stream blocking")?;
        match read_message(&mut stream) {
            Ok(Message::Hello { version }) if version == PROTOCOL_VERSION => {
                timings.event("agent.accepted");
                run_log.line("agent.accepted");
                return Ok(stream);
            }
            Ok(other) => {
                run_log.line(format!("agent.accept.bad_hello {other:?}"));
            }
            Err(e) => {
                run_log.line(format!("agent.accept.bad_hello_error {e:#}"));
            }
        }
    }
    bail!("timed out waiting for lnx-agent");
}

fn accept_unix_with_progress(
    listener: &UnixListener,
    timeout: Duration,
    progress: Option<(&TimingLog, &str)>,
    vm_error_rx: Option<&mpsc::Receiver<i32>>,
) -> Result<UnixStream> {
    let start = Instant::now();
    let mut last = None;
    while start.elapsed() < timeout {
        if let Some(rx) = vm_error_rx {
            if let Ok(rc) = rx.try_recv() {
                bail!("krun_start_enter failed: {}", krun_return_error(rc));
            }
        }
        let remaining = timeout.saturating_sub(start.elapsed());
        let poll_timeout = remaining.min(Duration::from_millis(250));
        let timeout_ms = poll_timeout.as_millis().min(i32::MAX as u128) as i32;
        let mut fds = [libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout_ms) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e).context("poll unix listener");
        }
        if rc == 0 {
            if let Some((timings, label)) = progress {
                timings.event(&format!(
                    "{label} elapsed_ms={:.0}",
                    start.elapsed().as_secs_f64() * 1000.0
                ));
            }
            continue;
        }
        if let Some(rx) = vm_error_rx {
            if let Ok(rc) = rx.try_recv() {
                bail!("krun_start_enter failed: {}", krun_return_error(rc));
            }
        }
        match listener.accept() {
            Ok((stream, _)) => return Ok(stream),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => last = Some(e),
        }
    }
    match last {
        Some(e) => Err(e).context("timed out waiting for lnx-agent"),
        None => bail!("timed out waiting for lnx-agent"),
    }
}

pub(crate) fn new_request_id() -> Result<u64> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("host clock is before Unix epoch")?
        .as_nanos() as u64;
    Ok(nanos ^ ((std::process::id() as u64) << 32))
}

fn should_request_pty() -> bool {
    is_tty(std::io::stdin().as_raw_fd()) && is_tty(std::io::stdout().as_raw_fd())
}

fn is_tty(fd: i32) -> bool {
    (unsafe { libc::isatty(fd) }) == 1
}

fn terminal_size() -> (u16, u16) {
    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    let mut size = Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::ioctl(std::io::stdin().as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
    let rows = if rc == 0 && size.ws_row > 0 {
        size.ws_row
    } else {
        24
    };
    let cols = if rc == 0 && size.ws_col > 0 {
        size.ws_col
    } else {
        80
    };
    (rows, cols)
}

struct RawTerminal {
    fd: i32,
    saved: libc::termios,
}

impl RawTerminal {
    fn enter() -> Option<Self> {
        let fd = std::io::stdin().as_raw_fd();
        if unsafe { libc::isatty(fd) } != 1 {
            return None;
        }

        let mut saved = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return None;
        }
        let mut raw = saved;
        raw.c_iflag |= libc::IGNPAR;
        raw.c_iflag &= !(libc::ISTRIP
            | libc::INLCR
            | libc::IGNCR
            | libc::ICRNL
            | libc::IXON
            | libc::IXANY
            | libc::IXOFF);
        raw.c_lflag &= !(libc::ISIG
            | libc::ICANON
            | libc::ECHO
            | libc::ECHOE
            | libc::ECHOK
            | libc::ECHONL
            | libc::IEXTEN);
        raw.c_oflag &= !libc::OPOST;
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSADRAIN, &raw) } != 0 {
            return None;
        }
        Some(Self { fd, saved })
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSADRAIN, &self.saved) };
    }
}

fn guest_home(host_home: &Path) -> String {
    host_home.to_string_lossy().into_owned()
}

fn host_home_for_cwd(cwd: &Path) -> Result<PathBuf> {
    let mut components = cwd.components();
    if matches!(components.next(), Some(std::path::Component::RootDir))
        && matches!(
            components.next().and_then(|c| c.as_os_str().to_str()),
            Some("Users")
        )
    {
        if let Some(user) = components.next() {
            return Ok(PathBuf::from("/Users").join(user.as_os_str()));
        }
    }
    dirs::home_dir().context("host home directory")
}

fn guest_cwd(cwd: &Path, host_home: &Path) -> String {
    if cwd.starts_with(host_home) {
        cwd.to_string_lossy().into_owned()
    } else {
        cwd.to_string_lossy().into_owned()
    }
}

fn home_write_allowlist(cwd: &Path, host_home: &Path) -> Vec<String> {
    let Ok(relative) = cwd.strip_prefix(host_home) else {
        return Vec::new();
    };
    if relative.as_os_str().is_empty() {
        vec![".".to_string()]
    } else {
        vec![relative.to_string_lossy().into_owned()]
    }
}

#[cfg(target_os = "macos")]
fn set_home_write_allowlist(ctx: &KrunContext, cwd: &Path, host_home: &Path) -> Result<()> {
    ctx.set_virtiofs_write_allowlist("home", &home_write_allowlist(cwd, host_home))
}

#[cfg(not(target_os = "macos"))]
fn set_home_write_allowlist(_ctx: &KrunContext, _cwd: &Path, _host_home: &Path) -> Result<()> {
    Ok(())
}

fn serve_snapshot(
    listener: UnixListener,
    ctx: &KrunContext,
    snapshot_path: &Path,
    rootfs: &Path,
    initramfs_stamp: &Path,
    force_full: bool,
    timings: &TimingLog,
) -> Result<()> {
    listener
        .set_nonblocking(true)
        .context("set snapshot listener nonblocking")?;
    timings.event("snapshot.accept.begin");
    let mut stream = accept_unix(&listener, Duration::from_secs(30))?;
    timings.event("snapshot.accepted");
    stream
        .set_nonblocking(false)
        .context("set snapshot stream blocking")?;
    let mut frame_type = [0u8; 1];
    stream.read_exact(&mut frame_type)?;
    let len = read_u32(&mut stream)?;
    if frame_type[0] != FRAME_SNAPSHOT || len != 0 {
        bail!("bad snapshot request");
    }
    timings.event(&format!("snapshot.request.read full={force_full}"));
    let mut ready = [0u8; 1];
    stream
        .read_exact(&mut ready)
        .context("read snapshot ready")?;
    if ready[0] != b'R' {
        bail!("bad snapshot ready");
    }
    timings.event("snapshot.ready.read");
    timings.event("snapshot.capture.begin");
    if force_full {
        snapshot_with_file_copy_full(ctx, snapshot_path, rootfs, initramfs_stamp)?;
    } else {
        ctx.snapshot_with_file_copy(snapshot_path, rootfs, "rootfs.ext4")?;
        copy_snapshot_stamp(snapshot_path, initramfs_stamp)?;
    }
    timings.event("snapshot.done");
    Ok(())
}

fn snapshot_with_file_copy_full(
    ctx: &KrunContext,
    snapshot_path: &Path,
    rootfs: &Path,
    initramfs_stamp: &Path,
) -> Result<()> {
    let parent = snapshot_path
        .parent()
        .context("snapshot path has no parent")?;
    let name = snapshot_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("snapshot path has no file name")?;
    let temp = parent.join(format!(".{name}.full"));
    remove_path_if_exists(&temp)?;
    ctx.snapshot_with_file_copy(&temp, rootfs, "rootfs.ext4")?;
    remove_path_if_exists(snapshot_path)?;
    fs::rename(&temp, snapshot_path)
        .with_context(|| format!("rename {} to {}", temp.display(), snapshot_path.display()))?;
    copy_snapshot_stamp(snapshot_path, initramfs_stamp)?;
    Ok(())
}

fn seed_incremental_snapshot(
    snapshot_path: &Path,
    restore_snapshot: Option<&Path>,
    latest_snapshot: &Path,
    run_log: &RunLog,
) -> Result<()> {
    if snapshot_path.join("pages.img").exists() {
        return Ok(());
    }
    let base = restore_snapshot
        .filter(|path| path.join("pages.img").exists())
        .or_else(|| {
            latest_snapshot
                .join("pages.img")
                .exists()
                .then_some(latest_snapshot)
        });
    let Some(base) = base else {
        run_log.line(format!(
            "snapshot.seed.skip path={} reason=no_base",
            snapshot_path.display()
        ));
        return Ok(());
    };
    remove_path_if_exists(snapshot_path)?;
    fs::create_dir_all(snapshot_path)
        .with_context(|| format!("create {}", snapshot_path.display()))?;
    for name in [
        "pages.img",
        "vmstate.bin",
        "rootfs.ext4",
        "initramfs.stamp",
        "shares.stamp",
    ] {
        let src = base.join(name);
        if src.exists() {
            clone_or_copy_file(&src, &snapshot_path.join(name))?;
        }
    }
    run_log.line(format!(
        "snapshot.seed.incremental path={} base={}",
        snapshot_path.display(),
        base.display()
    ));
    Ok(())
}

#[cfg(target_os = "macos")]
fn clone_or_copy_file(src: &Path, dst: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let c_src = CString::new(src.as_os_str().as_bytes())?;
    let c_dst = CString::new(dst.as_os_str().as_bytes())?;
    if unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) } == 0 {
        return Ok(());
    }
    fs::copy(src, dst).with_context(|| format!("copy {} to {}", src.display(), dst.display()))?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn clone_or_copy_file(src: &Path, dst: &Path) -> Result<()> {
    crate::sparse_copy::clone_or_copy_file(src, dst)
}

fn copy_snapshot_stamp(snapshot_path: &Path, initramfs_stamp: &Path) -> Result<()> {
    // Both stamps live in the run dir; they travel with the snapshot so a
    // later restore can check agent and share-root compatibility.
    let shares_stamp = initramfs_stamp.with_file_name("shares.stamp");
    for stamp in [initramfs_stamp, shares_stamp.as_path()] {
        let name = stamp.file_name().context("stamp file name")?;
        let target = snapshot_path.join(name);
        fs::copy(stamp, &target)
            .with_context(|| format!("copy {} to {}", stamp.display(), target.display()))?;
    }
    Ok(())
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(dir_err) => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(file_err) if file_err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(dir_err).with_context(|| format!("remove {}", path.display())),
        },
    }
}

fn cleanup_runtime_sockets(run_log: &RunLog, paths: &[&Path]) {
    for path in paths {
        match fs::remove_file(path) {
            Ok(()) => run_log.line(format!("runtime_socket.removed path={}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => run_log.line(format!(
                "runtime_socket.remove_failed path={} error={e}",
                path.display()
            )),
        }
    }
}

fn log_snapshot_summary(run_log: &RunLog, label: &str, path: &Path) {
    log_file_summary(run_log, label, path);
    for name in ["vmstate.bin", "pages.img", "rootfs.ext4"] {
        let file_label = format!("{label}.{name}");
        log_file_summary(run_log, &file_label, &path.join(name));
    }
}

fn log_file_summary(run_log: &RunLog, label: &str, path: &Path) {
    match fs::metadata(path) {
        Ok(meta) => {
            let kind = if meta.is_dir() {
                "dir"
            } else if meta.is_file() {
                "file"
            } else {
                "other"
            };
            let modified = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|time| time.as_secs().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            #[cfg(unix)]
            let allocation = format!(
                " blocks={} allocated_bytes={}",
                meta.blocks(),
                meta.blocks().saturating_mul(512)
            );
            #[cfg(not(unix))]
            let allocation = String::new();
            run_log.line(format!(
                "{label} path={} kind={kind} size={}{} modified_unix={modified}",
                path.display(),
                meta.len(),
                allocation
            ));
        }
        Err(e) => run_log.line(format!("{label} path={} stat_error={e}", path.display())),
    }
}

fn log_console_tail(run_log: &RunLog, path: &Path) {
    match fs::read(path) {
        Ok(bytes) if !bytes.is_empty() => {
            let start = bytes.len().saturating_sub(4096);
            let tail = String::from_utf8_lossy(&bytes[start..]);
            let mut lines = tail.lines().rev().take(12).collect::<Vec<_>>();
            lines.reverse();
            for line in lines {
                run_log.line(format!("console.tail {}", line.trim_end()));
            }
        }
        Ok(_) => run_log.line(format!("console.tail path={} empty=true", path.display())),
        Err(e) => run_log.line(format!(
            "console.tail path={} read_error={e}",
            path.display()
        )),
    }
}

fn read_u32(stream: &mut UnixStream) -> Result<u32> {
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).context("read u32")?;
    Ok(u32::from_be_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("lnx-{name}-{}-{unique}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn write_vmstate_header(snapshot: &Path, memory_bytes: u64, vcpu_count: u32) {
        fs::create_dir_all(snapshot).expect("create snapshot dir");
        let mut header = [0u8; 40];
        header[0..8].copy_from_slice(b"LKRNSS01");
        header[8..12].copy_from_slice(&SNAPSHOT_VMSTATE_VERSION.to_le_bytes());
        header[16..24].copy_from_slice(&memory_bytes.to_le_bytes());
        header[32..36].copy_from_slice(&vcpu_count.to_le_bytes());
        fs::write(snapshot.join("vmstate.bin"), header).expect("write vmstate");
    }

    #[test]
    fn snapshot_vm_config_returns_none_when_vmstate_is_absent() {
        let temp = TempDir::new("snapshot-missing");

        assert!(
            snapshot_vm_config(temp.path())
                .expect("read config")
                .is_none()
        );
    }

    #[test]
    fn snapshot_vm_config_parses_header_and_matches_config() {
        let temp = TempDir::new("snapshot-header");
        write_vmstate_header(temp.path(), 4 * 1024 * 1024 * 1024, 2);

        let config = snapshot_vm_config(temp.path())
            .expect("read config")
            .expect("config present");

        assert_eq!(config.vcpu_count, 2);
        assert_eq!(config.memory_mib(), 4096);
        assert!(config.matches(2, 4096));
        assert!(!config.matches(1, 4096));
        assert!(!config.matches(2, 8192));
    }

    #[test]
    fn shares_stamp_content_lists_home_and_outside_home_cwd() {
        assert_eq!(
            shares_stamp_content(Path::new("/Users/ramon"), None),
            "home=/Users/ramon\n"
        );
        assert_eq!(
            shares_stamp_content(Path::new("/Users/ramon"), Some(Path::new("/tmp/build"))),
            "home=/Users/ramon\ncwd=/tmp/build\n"
        );
    }

    #[test]
    fn snapshot_shares_compatibility_requires_identical_stamp() {
        let temp = TempDir::new("snapshot-shares");
        fs::create_dir_all(temp.path()).expect("create snapshot dir");
        let current = shares_stamp_content(Path::new("/Users/ramon"), None);

        // A snapshot from before share stamping must not restore.
        assert!(!snapshot_shares_are_compatible(temp.path(), &current));

        fs::write(temp.path().join("shares.stamp"), &current).expect("write stamp");
        assert!(snapshot_shares_are_compatible(temp.path(), &current));

        let drifted = shares_stamp_content(Path::new("/home/ramon"), None);
        assert!(!snapshot_shares_are_compatible(temp.path(), &drifted));
    }

    #[test]
    fn snapshot_vm_config_rejects_bad_magic_and_version() {
        let temp = TempDir::new("snapshot-bad");
        fs::create_dir_all(temp.path()).expect("create snapshot dir");
        fs::write(temp.path().join("vmstate.bin"), [0u8; 40]).expect("write bad vmstate");
        assert!(snapshot_vm_config(temp.path()).is_err());

        let mut header = [0u8; 40];
        header[0..8].copy_from_slice(b"LKRNSS01");
        header[8..12].copy_from_slice(&(SNAPSHOT_VMSTATE_VERSION + 1).to_le_bytes());
        fs::write(temp.path().join("vmstate.bin"), header).expect("write bad version");
        assert!(snapshot_vm_config(temp.path()).is_err());
    }

    #[test]
    fn framed_message_round_trips_over_unix_stream() {
        let (mut left, mut right) = UnixStream::pair().expect("unix pair");
        let message = Message::Data {
            channel_id: 7,
            bytes: b"hello".to_vec(),
        };

        write_message(&mut left, &message).expect("write message");
        let decoded = read_message(&mut right).expect("read message");

        assert_eq!(decoded, message);
    }

    #[test]
    fn framed_message_rejects_oversized_writes_and_reads() {
        let (mut left, mut right) = UnixStream::pair().expect("unix pair");
        let too_large = Message::Data {
            channel_id: 1,
            bytes: vec![0; MAX_MESSAGE_SIZE as usize + 1],
        };
        assert!(write_message(&mut left, &too_large).is_err());

        left.write_all(&(MAX_MESSAGE_SIZE + 1).to_be_bytes())
            .expect("write oversized length");
        assert!(read_message(&mut right).is_err());
    }

    #[test]
    fn broker_idle_ttl_defaults_to_immediate_shutdown() {
        assert_eq!(broker_idle_ttl_from_env(None), Duration::ZERO);
        assert_eq!(broker_idle_ttl_from_env(Some("nope")), Duration::ZERO);
    }

    #[test]
    fn broker_idle_ttl_reads_milliseconds_from_env() {
        assert_eq!(
            broker_idle_ttl_from_env(Some("30000")),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn owner_idle_ttl_defaults_to_five_second_grace() {
        assert_eq!(owner_idle_ttl_from_env(None), Duration::from_secs(5));
        assert_eq!(
            owner_idle_ttl_from_env(Some("nope")),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn owner_idle_ttl_reads_env_but_clamps_to_minimum() {
        assert_eq!(
            owner_idle_ttl_from_env(Some("30000")),
            Duration::from_secs(30)
        );
        assert_eq!(
            owner_idle_ttl_from_env(Some("0")),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn rootfs_backend_defaults_to_pmem() {
        assert_eq!(RootfsBackend::from_env(None).unwrap(), RootfsBackend::Pmem);
        assert_eq!(
            RootfsBackend::from_env(Some(String::new())).unwrap(),
            RootfsBackend::Pmem
        );
    }

    #[test]
    fn rootfs_backend_accepts_block() {
        assert_eq!(
            RootfsBackend::from_env(Some("block".to_string())).unwrap(),
            RootfsBackend::Block
        );
    }

    #[test]
    fn rootfs_backend_rejects_unknown_values() {
        assert!(RootfsBackend::from_env(Some("virtiofs".to_string())).is_err());
    }

    #[test]
    fn forward_spec_round_trips_the_cli_format() {
        let forward = PortForward {
            listen_host: "127.0.0.1".to_string(),
            listen_port: 16081,
            guest_host: "localhost".to_string(),
            guest_port: 6080,
        };
        assert_eq!(forward_spec(&forward), "127.0.0.1:16081:localhost:6080");
    }

    #[test]
    fn guest_cwd_uses_host_path_under_home() {
        assert_eq!(
            guest_cwd(Path::new("/Users/ramon/src/lnx"), Path::new("/Users/ramon")),
            "/Users/ramon/src/lnx"
        );
    }

    #[test]
    fn guest_cwd_uses_host_path_outside_home() {
        assert_eq!(
            guest_cwd(Path::new("/tmp/build"), Path::new("/Users/ramon")),
            "/tmp/build"
        );
    }

    #[test]
    fn host_home_for_cwd_uses_mounted_macos_home() {
        assert_eq!(
            host_home_for_cwd(Path::new("/Users/ramon/src/lnx")).unwrap(),
            PathBuf::from("/Users/ramon")
        );
    }

    #[test]
    fn home_write_allowlist_is_relative_under_home() {
        assert_eq!(
            home_write_allowlist(Path::new("/Users/ramon/src/lnx"), Path::new("/Users/ramon")),
            vec!["src/lnx".to_string()]
        );
    }

    #[test]
    fn home_write_allowlist_uses_dot_for_home_root() {
        assert_eq!(
            home_write_allowlist(Path::new("/Users/ramon"), Path::new("/Users/ramon")),
            vec![".".to_string()]
        );
    }

    #[test]
    fn home_write_allowlist_is_empty_outside_home() {
        assert!(
            home_write_allowlist(Path::new("/tmp/build"), Path::new("/Users/ramon")).is_empty()
        );
    }
}

fn console_hint(path: &Path) -> String {
    let Ok(bytes) = fs::read(path) else {
        return String::new();
    };
    if bytes.is_empty() {
        return String::new();
    }
    let start = bytes.len().saturating_sub(4096);
    format!(
        "\n\nVM console:\n{}",
        String::from_utf8_lossy(&bytes[start..]).trim_end()
    )
}
