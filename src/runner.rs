use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    fs::{self, OpenOptions},
    io::{ErrorKind, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    os::fd::AsRawFd,
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, Once,
        atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;

use crate::fsutil::remove_path_if_exists;
use anyhow::{Context, Result, anyhow, bail};
use libkrun::{Error as KrunError, Kernel, Network, VmBuilder, VmHandle};
use lnx_protocol::{Message, PROTOCOL_VERSION};

use crate::store::{self, GenerationId, Origin, Store};
use crate::{
    host_share, initramfs, krun,
    paths::{
        GVPROXY_KRUN_SOCKET_SUFFIX, Layout, RuntimeSocket, UNIX_SOCKET_PATH_CAPACITY,
        unix_socket_path_fits,
    },
};

const AGENT_PORT: u32 = 10240;
const SNAPSHOT_PORT: u32 = 10241;
const CONTROL_PORT: u32 = 10242;
const FRAME_SNAPSHOT: u8 = b'K';

// Owner exit status meaning "the VM failed to start with a restore
// configured"; the client reports a hard restore failure.
const EXIT_RESTORE_FAILED: i32 = 86;
/// What `--timeout` exits with, as timeout(1) does.
const EXIT_TIMED_OUT: i32 = 124;

const DEFAULT_OWNER_IDLE_TTL: Duration = Duration::from_secs(5);
// The detached owner counts idle time from broker start, so a TTL shorter than
// the client's connect retry interval would suspend the VM before the client
// that spawned it ever connects.
const MIN_OWNER_IDLE_TTL: Duration = Duration::from_millis(250);
const OWNER_BOOT_TIMEOUT: Duration = Duration::from_secs(120);
const FRESH_OWNER_SLOT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_AGENT_ACCEPT_TIMEOUT: Duration = Duration::from_secs(90);
const RESTORE_AGENT_ACCEPT_TIMEOUT: Duration = Duration::from_secs(20);
const BROKER_HELLO_TIMEOUT: Duration = Duration::from_secs(1);
const OWNER_REPLACE_GRACE: Duration = Duration::from_secs(120);
const ROOTFS_BACKEND_ENV: &str = "LNX_ROOTFS_BACKEND";
const DETERMINISTIC_EXEC_UID: u32 = 1000;
const DETERMINISTIC_EXEC_GID: u32 = 1000;
const DETERMINISTIC_EXEC_GROUP: &str = "lnxuser";
const DETERMINISTIC_TERM: &str = "xterm-256color";
const DETERMINISTIC_COLORTERM: &str = "";
const DETERMINISTIC_ROWS: u16 = 24;
const DETERMINISTIC_COLS: u16 = 80;
const DETERMINISTIC_CLOCK_STATE: &str = "deterministic-clock.state";
const DETERMINISTIC_TIMER_JUMPS: &str = "deterministic-timer-jumps.log";
const DETERMINISTIC_TIMER_JUMPS_CURSOR: &str = "deterministic-timer-jumps.cursor";
const RUN_ID_ENV: &str = "LNX_RUN_ID";
const LAUNCH_METADATA: &str = "launch.json";
/// The stamp of the agent the running VM is executing, which a capture
/// records: the snapshot's agent when the VM was resumed, the current one
/// when it booted.
const RUNNING_AGENT_STAMP: &str = "running-agent.stamp";
static SIGNAL_INIT: Once = Once::new();
static OWNER_SIGNAL_INIT: Once = Once::new();
/// The signal (SIGINT, SIGTERM or SIGHUP) that asked the client to stop, or 0.
static INTERRUPT_SIGNAL: AtomicI32 = AtomicI32::new(0);
static OWNER_SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static LIFECYCLE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

extern "C" fn handle_client_interrupt(signal: libc::c_int) {
    INTERRUPT_SIGNAL.store(signal, Ordering::SeqCst);
}

pub(crate) fn client_interrupted() -> bool {
    INTERRUPT_SIGNAL.load(Ordering::SeqCst) != 0
}

/// The exit status of a client stopped by a signal, as a shell reports it.
fn interrupted_status() -> i32 {
    128 + INTERRUPT_SIGNAL.load(Ordering::SeqCst)
}

extern "C" fn handle_owner_shutdown(_: libc::c_int) {
    OWNER_SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
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
    pub exec: ExecOptions,
    pub no_host_shares: bool,
    pub vhost_user_fs: Vec<VhostUserFsMount>,
    pub reuse_owner: bool,
    pub deterministic: Option<DeterministicConfig>,
    pub trace_events: bool,
}

/// Options for one guest command, as opposed to the VM's shape.
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    pub run_as_root: bool,
    /// Environment set after what lnx forwards, so it wins.
    pub env: Vec<(String, String)>,
    /// The guest working directory, instead of the host's; a relative path
    /// is relative to the host's.
    pub workdir: Option<String>,
    /// Ends the command, and everything it started, after this long.
    pub timeout: Option<Duration>,
    /// Starts the command in its own session in the background, prints its
    /// pid and returns; its output goes to /tmp/lnx-detached-PID.log.
    pub detach: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VhostUserFsMount {
    pub tag: String,
    pub mountpoint: String,
    pub socket: PathBuf,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeterministicConfig {
    pub seed: String,
}

/// Marks an owner start failure that happened while restoring a snapshot's
/// memory, as opposed to an unrelated boot failure.
#[derive(Debug)]
struct RestoreRefused;

impl std::fmt::Display for RestoreRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "snapshot memory restore refused by the devices")
    }
}

impl std::error::Error for RestoreRefused {}

#[derive(Debug)]
struct BrokerProtocolMismatch {
    expected: u16,
    actual: u16,
}

impl std::fmt::Display for BrokerProtocolMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "running VM owner protocol version {} is incompatible with this client protocol version {}; stop the instance and retry",
            self.actual, self.expected
        )
    }
}

impl std::error::Error for BrokerProtocolMismatch {}

#[derive(Debug)]
struct BrokerHelloFailed;

impl std::fmt::Display for BrokerHelloFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "running VM owner did not complete the broker protocol hello; stop the instance and retry"
        )
    }
}

impl std::error::Error for BrokerHelloFailed {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortForward {
    pub listen_host: String,
    pub listen_port: u16,
    pub guest_host: String,
    pub guest_port: u16,
}

/// The owner was already stopping when the command reached it, so the
/// command never started and running it again is safe.
#[derive(Debug)]
struct CommandNotStarted;

impl std::fmt::Display for CommandNotStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(OWNER_STOPPING_NOT_STARTED)
    }
}

impl std::error::Error for CommandNotStarted {}

/// How many times a command is offered to a VM owner that turns out to be
/// stopping before lnx gives up.
const NOT_STARTED_ATTEMPTS: usize = 3;

/// Runs a guest command, starting a VM owner if none is running. A command
/// that reached an owner just as it stopped is run again on the next one.
pub fn run(config: RunConfig) -> Result<i32> {
    let mut attempt = 1;
    loop {
        match run_once(&config) {
            Err(error) if error.is::<CommandNotStarted>() && attempt < NOT_STARTED_ATTEMPTS => {
                attempt += 1;
            }
            result => return result,
        }
    }
}

fn run_once(config: &RunConfig) -> Result<i32> {
    if config.trace_events && config.deterministic.is_none() {
        bail!("trace events require deterministic mode");
    }
    if config.vhost_user_fs.iter().any(|mount| !mount.read_only) {
        bail!("vhost-user fs mounts are read-only only");
    }
    install_signal_handlers();
    INTERRUPT_SIGNAL.store(0, Ordering::SeqCst);
    config.layout.create_runtime_dirs()?;
    refuse_crashed_run(&config.layout)?;
    show_last_run_notice(&config.layout);
    let run_log = Arc::new(RunLog::open(&config.layout)?);
    let run_id = current_run_id();
    run_log.line(format!(
        "run.start run_id={} pid={} instance={} cmd={:?} cwd={} restore={}",
        run_id,
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
    preflight_host_share_cwd(&config.layout, &config.cwd, config.no_host_shares)?;
    let broker_socket = config.layout.socket(RuntimeSocket::Broker);
    let no_daemon_reuse = !config.reuse_owner || debug_flag_enabled("nodaemonreuse");
    if no_daemon_reuse {
        run_log.line("debug.nodaemonreuse enabled");
        eprintln!(
            "debug[nodaemonreuse]: replacing any existing VM owner for this instance before starting a fresh owner."
        );
    }
    if config.forwards.is_empty() && !no_daemon_reuse {
        if broker_socket.exists() {
            validate_runtime_deterministic_compatibility(
                &config.layout,
                config.deterministic.as_ref(),
            )?;
            validate_runtime_share_compatibility(config)?;
        }
        if let Some(status) = run_existing_broker_client(&broker_socket, config, Some(&run_log))? {
            run_log.line(format!("run.done run_id={run_id} status={status}"));
            return Ok(status);
        }
    } else {
        preflight_fresh_owner_network(config, &run_log)?;
        prepare_fresh_owner_slot(&config.layout, no_daemon_reuse, &run_log)?;
    }
    preflight_fresh_owner_network(config, &run_log)?;
    let start_lock = match acquire_owner_start_or_run_client(
        &broker_socket,
        config,
        config.forwards.is_empty() && !no_daemon_reuse,
        &run_log,
    )? {
        OwnerStartOutcome::Lock(lock) => lock,
        OwnerStartOutcome::Status(status) => {
            run_log.line(format!("run.done run_id={run_id} status={status}"));
            return Ok(status);
        }
    };
    let mut owner = spawn_owner_process(config, &run_log, &run_id)?;
    let status = match run_broker_client_awaiting_owner(&broker_socket, &mut owner, config, &run_log)
    {
        Ok(status) => status,
        Err(e) => {
            run_log.line(format!("client.error {e:#}"));
            return Err(e);
        }
    };
    drop(start_lock);
    run_log.line(format!("run.done run_id={run_id} status={status}"));
    Ok(status)
}

/// Whether a VM owner or a maintenance command holds the instance now.
pub(crate) fn instance_is_held(layout: &Layout) -> Result<bool> {
    Ok(instance_lock_state(layout)?.is_held())
}

/// Fails early, with the recovery commands, when the instance's last VM run
/// crashed after serving commands and nobody has chosen what to keep.
pub(crate) fn refuse_crashed_run(layout: &Layout) -> Result<()> {
    with_instance_guard(layout, |state| {
        if state.is_held() {
            return Ok(());
        }
        refuse_crashed_run_unguarded(layout)
    })
}

/// [`refuse_crashed_run`] for callers that already hold the instance guard
/// (or the lock) and know nobody is running the instance.
pub(crate) fn refuse_crashed_run_unguarded(layout: &Layout) -> Result<()> {
    match Store::new(&layout.instance_dir).crashed()? {
        Some(crashed) => Err(crashed.into()),
        None => Ok(()),
    }
}

/// Runs `action` while holding the instance lock as a maintenance command,
/// unless a VM owner or another command holds it. `action` receives the lease
/// of a holder that died holding the lock, if any.
pub(crate) fn with_exclusive_instance_state<T>(
    layout: &Layout,
    action: impl FnOnce(&InstanceLock, Option<&Lease>) -> Result<T>,
) -> Result<Option<T>> {
    let mut stale = None;
    let Some(lock) = InstanceLock::try_acquire(layout, LeaseRole::Maintenance, |lease| {
        stale = lease.cloned();
        Ok(())
    })?
    else {
        return Ok(None);
    };
    action(&lock, stale.as_ref()).map(Some)
}

fn validate_runtime_share_compatibility(config: &RunConfig) -> Result<()> {
    let current = launch_metadata_for_config(config)?;
    let path = config.layout.run_dir.join(LAUNCH_METADATA);
    match read_launch_metadata(&config.layout.run_dir) {
        Ok(metadata) if launch_metadata_matches_ignoring_cwd(&metadata, &current) => Ok(()),
        Ok(metadata) => bail!(
            "running VM launch metadata is incompatible ({}): {}",
            describe_launch_mismatch(&metadata, &current),
            path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("running VM has no launch metadata: {}", path.display())
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn prepare_fresh_owner_slot(
    layout: &Layout,
    replace_existing: bool,
    run_log: &RunLog,
) -> Result<()> {
    if replace_existing {
        replace_existing_owner(layout, run_log)?;
    }
    wait_for_fresh_owner_slot(layout, run_log)
}

fn wait_for_fresh_owner_slot(layout: &Layout, run_log: &RunLog) -> Result<()> {
    let start = Instant::now();
    let mut logged_wait = false;
    while instance_is_held(layout)? {
        if !logged_wait {
            run_log.line(format!(
                "fresh_owner.slot.wait lock={} timeout_ms={}",
                instance_lock_path(layout).display(),
                FRESH_OWNER_SLOT_TIMEOUT.as_millis()
            ));
            logged_wait = true;
        }
        if start.elapsed() > FRESH_OWNER_SLOT_TIMEOUT {
            bail!(
                "starting a fresh VM owner requires exclusive ownership, but an existing owner is still running for instance {}; wait for it to checkpoint and exit before retrying",
                layout.instance
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn replace_existing_owner(layout: &Layout, run_log: &RunLog) -> Result<()> {
    let Some(owner) = live_owner(layout) else {
        return refuse_crashed_run(layout);
    };
    let owner = owner.process;
    run_log.line(format!(
        "owner.replace.term pid={} instance={}",
        owner.pid, layout.instance
    ));
    owner.signal(libc::SIGTERM)?;
    let deadline = Instant::now() + OWNER_REPLACE_GRACE;
    while Instant::now() < deadline {
        match live_owner(layout).map(|lease| lease.process) {
            Some(current) if current == owner => {}
            Some(current) => {
                run_log.line(format!(
                    "owner.replace.changed previous_pid={} current_pid={}",
                    owner.pid, current.pid
                ));
                return Ok(());
            }
            None => {
                run_log.line(format!("owner.replace.exited pid={}", owner.pid));
                return refuse_crashed_run(layout);
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    if live_owner(layout).is_some_and(|lease| lease.process == owner) {
        bail!(
            "owner process {} for instance {} did not finish its shutdown snapshot within 120 seconds; it was left running so recoverable state is not discarded",
            owner.pid,
            layout.instance
        );
    }
    run_log.line(format!("owner.replace.exited pid={}", owner.pid));
    refuse_crashed_run(layout)
}

/// Runs the detached VM owner process. A refused memory restore exits with a
/// distinct status, once the run has released the instance, so the client
/// waiting for the broker can report it as such.
pub fn run_owner(config: RunConfig) -> Result<()> {
    let result = own_vm(config);
    if let Err(error) = &result
        && error.downcast_ref::<RestoreRefused>().is_some()
    {
        eprintln!("Error: {error:#}");
        std::process::exit(EXIT_RESTORE_FAILED);
    }
    result
}

fn own_vm(config: RunConfig) -> Result<()> {
    if config.trace_events && config.deterministic.is_none() {
        bail!("trace events require deterministic mode");
    }
    OWNER_SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    install_owner_signal_handlers();
    config.layout.create_runtime_dirs()?;
    let run_log = Arc::new(RunLog::open(&config.layout)?);
    let owner_run_id = current_run_id();
    run_log.line(format!(
        "owner.start owner_run_id={} pid={} instance={} cwd={} snapshot={}",
        owner_run_id,
        std::process::id(),
        config.layout.instance,
        config.cwd.display(),
        config
            .restore_snapshot
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "latest".to_string())
    ));
    let broker_socket = config.layout.socket(RuntimeSocket::Broker);
    let Some(instance_lock) = acquire_instance_for_owner(&config.layout, &broker_socket, &run_log)?
    else {
        run_log.line("owner.exit reason=existing_broker");
        return Ok(());
    };
    reset_owner_attempt_logs(&config.layout, &run_log);
    if broker_socket.exists() {
        run_log.line(format!(
            "broker.stale_socket.remove path={}",
            broker_socket.display()
        ));
        let _ = fs::remove_file(&broker_socket);
    }
    let session = Arc::new(RunSession::begin(
        &config.layout,
        instance_lock,
        config.restore_snapshot.as_deref(),
        Arc::clone(&run_log),
    )?);

    let idle = IdlePolicy {
        ttl: owner_idle_ttl(),
        starts_idle: !debug_flag_enabled("nodaemonreuse"),
    };
    let vm = match start_vm(
        &config,
        &session,
        &run_log,
        &broker_socket,
        idle,
        &owner_run_id,
    ) {
        Ok(vm) => vm,
        Err(error) => {
            if let Err(abandon_error) = session.abandon(&error) {
                run_log.line(format!("store.run.abandon_error error={abandon_error:#}"));
            }
            if error.downcast_ref::<RestoreRefused>().is_some() {
                run_log.line(format!("owner.start.restore_failed error={error:#}"));
            }
            return Err(error);
        }
    };
    let owner_result = vm
        .owner
        .join()
        .map_err(|_| anyhow!("VM owner thread panicked"))
        .and_then(|result| result);
    if let Err(error) = &owner_result {
        run_log.line(format!("owner.error error={error:#}"));
        if let Err(abandon_error) = session.abandon(error) {
            run_log.line(format!("store.run.abandon_error error={abandon_error:#}"));
        }
    }
    owner_result?;
    flush_deterministic_trace_events(&config.layout, vm.trace_log.as_deref())?;
    run_log.line(format!("owner.done owner_run_id={owner_run_id}"));
    drop(vm.network);
    Ok(())
}

struct VmHandles {
    owner: thread::JoinHandle<Result<()>>,
    network: NetworkBacking,
    trace_log: Option<Arc<TraceLog>>,
}

enum NetworkBacking {
    Gvproxy(Gvproxy),
}

impl NetworkBacking {
    /// LNX_NET_IP / LNX_NET_GATEWAY values for the guest agent; empty means
    /// the agent uses the gvproxy static configuration.
    fn guest_env(&self) -> (String, String) {
        match self {
            NetworkBacking::Gvproxy(_) => (String::new(), String::new()),
        }
    }
}

fn start_network(
    config: &RunConfig,
    run_log: &RunLog,
    timings: &TimingLog,
) -> Result<NetworkBacking> {
    let _ = config;
    let gvproxy = start_gvproxy(
        &config.layout.socket(RuntimeSocket::Gvproxy),
        &config.layout.run_dir.join("gvproxy.log"),
    )?;
    timings.event("gvproxy.ready");
    run_log.line(format!("gvproxy.ready socket={}", gvproxy.socket.display()));
    Ok(NetworkBacking::Gvproxy(gvproxy))
}

fn preflight_fresh_owner_network(config: &RunConfig, run_log: &RunLog) -> Result<()> {
    let _ = config;
    let _ = run_log;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootfsBackend {
    Pmem,
}

impl RootfsBackend {
    fn from_env(value: Option<String>) -> Result<Self> {
        match value.as_deref() {
            None | Some("") | Some("pmem") => Ok(Self::Pmem),
            Some(value) => {
                bail!("{ROOTFS_BACKEND_ENV} must be 'pmem' or unset, got {value:?}")
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
    session: &Arc<RunSession>,
    run_log: &Arc<RunLog>,
    broker_socket: &Path,
    idle: IdlePolicy,
    owner_run_id: &str,
) -> Result<VmHandles> {
    let run = session.run();
    // The memory snapshot this run resumes: the run's private clones of its
    // base generation.
    let restore_dir = run.restores_memory().then(|| run.dir.clone());
    let timings = Arc::new(TimingLog::open(
        &config.layout,
        &config.command,
        restore_dir.as_deref(),
    )?);
    timings.install_for_libkrun();
    timings.event("dirs.ready");
    let trace_log = if config.trace_events {
        let trace_log = Arc::new(TraceLog::open(&config.layout)?);
        run_log.line(format!("trace.events path={}", trace_log.path.display()));
        Some(trace_log)
    } else {
        None
    };

    let (initrd, rebuilt_initramfs) = initramfs::write_from_agent(
        include_bytes!(env!("LNX_AGENT")),
        env!("LNX_AGENT_SOURCE_STAMP"),
        config.layout.run_dir.clone(),
    )?;
    timings.event(if rebuilt_initramfs {
        "initramfs.rebuilt"
    } else {
        "initramfs.cached"
    });
    let initramfs_stamp = config.layout.run_dir.join("initramfs.stamp");
    let mut network = start_network(config, run_log, &timings)?;
    let current_host_home = host_home_for_cwd(&config.cwd)?;
    let current_outside_home_cwd =
        (!config.cwd.starts_with(&current_host_home)).then(|| config.cwd.clone());
    let current_launch_metadata = launch_metadata_for_config(config)?;
    let mut share_layout = ShareLayout {
        host_home: current_host_home,
        outside_home_cwd: current_outside_home_cwd,
        no_host_shares: config.no_host_shares,
    };
    let mut launch_metadata = current_launch_metadata.clone();
    if let Some(snapshot) = &restore_dir {
        if let Some(snapshot_share_layout) = snapshot_share_layout(snapshot)? {
            if launch_metadata_matches_ignoring_cwd(
                &snapshot_share_layout.metadata,
                &current_launch_metadata,
            ) {
                run_log.line(format!(
                    "snapshot.shares.restore_layout path={}",
                    snapshot.join(LAUNCH_METADATA).display()
                ));
                launch_metadata = snapshot_share_layout.metadata;
                share_layout = snapshot_share_layout.layout;
            }
        }
    }
    let launch_metadata_path = config.layout.run_dir.join(LAUNCH_METADATA);
    write_launch_metadata(&launch_metadata_path, &launch_metadata)?;
    let deterministic_stamp = deterministic_stamp_content(config.deterministic.as_ref());
    let deterministic_stamp_path = config.layout.run_dir.join("deterministic.stamp");
    fs::write(&deterministic_stamp_path, &deterministic_stamp)
        .with_context(|| format!("write {}", deterministic_stamp_path.display()))?;
    configure_libkrun_deterministic_time(config.deterministic.is_some());
    let deterministic_clock_state =
        deterministic_clock_state_for_start(config.deterministic.as_ref(), restore_dir.as_deref())?;
    if let Some(clock_state) = &deterministic_clock_state {
        let clock_state_path = config.layout.run_dir.join(DETERMINISTIC_CLOCK_STATE);
        write_deterministic_clock_state(&clock_state_path, clock_state)?;
        configure_libkrun_deterministic_clock_state(Some(&clock_state_path));
        configure_libkrun_deterministic_timer_jumps(Some(
            &config.layout.run_dir.join(DETERMINISTIC_TIMER_JUMPS),
        ));
    } else {
        let clock_state_path = config.layout.run_dir.join(DETERMINISTIC_CLOCK_STATE);
        remove_path_if_exists(&clock_state_path)?;
        configure_libkrun_deterministic_clock_state(None);
        configure_libkrun_deterministic_timer_jumps(None);
    }
    if let (Some(trace), Some(clock_state)) = (&trace_log, &deterministic_clock_state) {
        trace.set_next_sequence(clock_state.event_sequence);
    }
    if let Some(trace) = &trace_log {
        let mut fields = vec![
            trace_text("instance", config.layout.instance.clone()),
            trace_integer("cpus", config.cpus as i64),
            trace_integer("memory_mib", config.memory_mib as i64),
            trace_bool("nested_kvm", config.nested_kvm),
            trace_bool("no_host_shares", config.no_host_shares),
            trace_bool("restore_snapshot", restore_dir.is_some()),
            trace_text("network", "embedded-gvproxy"),
        ];
        if let Some(deterministic) = &config.deterministic {
            fields.push(trace_text("seed", deterministic.seed.clone()));
            fields.push(trace_integer("initial_realtime_unix_secs", 0));
        }
        trace.event("vm_start_config", fields);
        if let Some(clock_state) = &deterministic_clock_state {
            trace.event(
                "deterministic_clock_state",
                vec![
                    trace_integer(
                        "realtime_unix_nanos",
                        clock_state.realtime_unix_nanos as i64,
                    ),
                    trace_integer("monotonic_nanos", clock_state.monotonic_nanos as i64),
                    trace_integer(
                        "counter_frequency_hz",
                        clock_state.counter_frequency_hz as i64,
                    ),
                    trace_integer("event_sequence", clock_state.event_sequence as i64),
                    trace_integer("timer_jump_count", clock_state.timer_jump_count as i64),
                    trace_integer(
                        "last_timer_deadline_ticks",
                        clock_state.last_timer_deadline_ticks as i64,
                    ),
                ],
            );
        }
    }
    if let Some(snapshot) = &restore_dir {
        validate_restore_compatibility(
            snapshot,
            &initramfs_stamp,
            &launch_metadata,
            &deterministic_stamp,
            config,
            run_log,
        )?;
    }
    let running_agent = restore_dir
        .as_ref()
        .map(|snapshot| snapshot.join("initramfs.stamp"))
        .unwrap_or_else(|| initramfs_stamp.clone());
    let running_agent_stamp = initramfs_stamp.with_file_name(RUNNING_AGENT_STAMP);
    fs::copy(&running_agent, &running_agent_stamp).with_context(|| {
        format!(
            "record the running agent from {} in {}",
            running_agent.display(),
            running_agent_stamp.display()
        )
    })?;
    let vm_restore_snapshot = restore_dir.clone();
    configure_snapshot_restore_compat(vm_restore_snapshot.as_deref(), run_log);

    let socket = config.layout.socket(RuntimeSocket::Agent);
    let snapshot_socket = config.layout.socket(RuntimeSocket::Snapshot);
    let control_socket = config.layout.socket(RuntimeSocket::Control);
    let listener = bind_unix_listener(&socket)?;
    let snapshot_listener = bind_unix_listener(&snapshot_socket)?;
    let control_listener = bind_unix_listener(&control_socket)?;
    let broker_listener = bind_unix_listener(broker_socket)?;
    timings.event("listeners.ready");
    run_log.line(format!(
        "listeners.ready agent={} snapshot={} control={} broker={}",
        socket.display(),
        snapshot_socket.display(),
        control_socket.display(),
        broker_socket.display()
    ));
    let krun_log_level = std::env::var("LNX_KRUN_LOG_LEVEL")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(2);
    krun::init_logging_once(krun::log_level_from_verbosity(krun_log_level))?;
    let mut vm_builder = VmBuilder::new();
    vm_builder.console_output(&config.layout.console_log);
    vm_builder.resources(config.cpus, config.memory_mib)?;
    if config.nested_kvm {
        vm_builder.nested_virt(true);
    }
    let rootfs = run.rootfs();
    run_log.line(format!(
        "rootfs.live owner_run_id={owner_run_id} run={} path={}",
        run.id,
        rootfs.display()
    ));
    crate::init::ensure_ext4_has_no_errors(&rootfs, "rootfs").map_err(|e| {
        run_log.line(format!(
            "rootfs.health.error path={} error={e:#}",
            rootfs.display()
        ));
        e
    })?;
    log_file_summary(run_log, "rootfs.selected", &rootfs);
    let rootfs_backend = RootfsBackend::from_env(std::env::var(ROOTFS_BACKEND_ENV).ok())?;
    let root_device = match rootfs_backend {
        RootfsBackend::Pmem => {
            vm_builder.root_pmem(&rootfs);
            "/dev/pmem0"
        }
    };
    let (guest_home, guest_cwd) = if share_layout.no_host_shares {
        (String::new(), String::new())
    } else {
        (
            guest_home(&share_layout.host_home),
            share_layout
                .outside_home_cwd
                .as_deref()
                .map(guest_cwd)
                .unwrap_or_default(),
        )
    };
    if share_layout.no_host_shares {
        run_log.line("host_shares.disabled");
    } else {
        vm_builder.virtiofs(krun::host_share_virtiofs(
            "home",
            &share_layout.host_home,
            &home_write_allowlist(&config.cwd, &share_layout.host_home),
            &run.host_share_state().join("home"),
        ))?;
        if let Some(cwd) = &share_layout.outside_home_cwd {
            vm_builder.virtiofs(krun::host_share_virtiofs(
                "cwd",
                cwd,
                &cwd_write_allowlist(),
                &run.host_share_state().join("cwd"),
            ))?;
        }
    }
    for mount in &config.vhost_user_fs {
        vm_builder.vhost_user_virtiofs(&mount.tag, &mount.socket)?;
        run_log.line(format!(
            "vhost_user_fs.added tag={} mount={} socket={} read_only={}",
            mount.tag,
            mount.mountpoint,
            mount.socket.display(),
            mount.read_only
        ));
    }
    let mut kernel_cmdline =
        format!("console=hvc0 reboot=k panic=1 root={root_device} rw rootfstype=ext4");
    #[cfg(target_arch = "aarch64")]
    kernel_cmdline.push_str(" arm64.nopauth");
    kernel_cmdline.push_str(" rootflags=dax");
    if config.nested_kvm {
        kernel_cmdline.push_str(" kvm.allow_unsafe_mappings=1");
    }
    vm_builder.kernel(
        Kernel::raw(&config.layout.kernel)
            .initramfs(&initrd)
            .cmdline(kernel_cmdline),
    )?;
    vm_builder.vsock_connector(AGENT_PORT, &socket)?;
    vm_builder.vsock_connector(SNAPSHOT_PORT, &snapshot_socket)?;
    vm_builder.vsock_connector(CONTROL_PORT, &control_socket)?;
    match &mut network {
        NetworkBacking::Gvproxy(gvproxy) => {
            vm_builder.network(Network::gvproxy_vfkit(&gvproxy.socket))?;
        }
    }
    timings.event("krun.devices.configured");

    if let Some(snapshot) = &vm_restore_snapshot {
        vm_builder.restore_from_snapshot(snapshot)?;
        timings.event("snapshot.restore.configured");
        run_log.line(format!(
            "snapshot.restore.configured owner_run_id={owner_run_id} generation={} path={}",
            session
                .base()
                .map(|base| base.id().to_string())
                .unwrap_or_else(|| "none".to_string()),
            snapshot.display()
        ));
    }

    vm_builder.workdir("/");
    let init_unix_secs = match &config.deterministic {
        Some(_) => 0,
        None => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("host clock is before Unix epoch")?
            .as_secs(),
    };
    let (net_ip, net_gateway) = network.guest_env();
    let init_env = vec![
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        "container=lnx".to_string(),
        format!("LNX_HOST_UNIX_SECS={init_unix_secs}"),
        format!("LNX_ROOT_DEVICE={root_device}"),
        format!("LNX_NET_IP={net_ip}"),
        format!("LNX_NET_GATEWAY={net_gateway}"),
        format!("LNX_VIRTIOFS_HOME={guest_home}"),
        share_layout
            .outside_home_cwd
            .as_ref()
            .filter(|_| !share_layout.no_host_shares)
            .map(|_| format!("LNX_VIRTIOFS_CWD={guest_cwd}"))
            .unwrap_or_else(|| "LNX_VIRTIOFS_CWD=".to_string()),
        format!(
            "LNX_VHOST_USER_FS={}",
            vhost_user_fs_guest_env(&config.vhost_user_fs)
        ),
    ];
    vm_builder.exec("/init", &["--init".to_string()], &init_env);
    timings.event("krun.exec.configured");

    let vm = vm_builder.build();
    let ctx = Arc::new(vm.handle());
    let console_log = config.layout.console_log.clone();
    let vm_timings = Arc::clone(&timings);
    let vm_run_log = Arc::clone(run_log);
    let (vm_error_tx, vm_error_rx) = mpsc::channel::<KrunError>();
    thread::spawn(move || {
        vm_timings.event("krun.start_enter.begin");
        match vm.start() {
            Ok(()) => {
                vm_timings.event("krun.start_enter.return ok");
                vm_run_log.line("krun.start_enter.return ok");
            }
            Err(error) => {
                vm_timings.event("krun.start_enter.error");
                vm_run_log.line(format!("krun.start_enter.error error={error}"));
                log_console_tail(&vm_run_log, &console_log);
                let _ = vm_error_tx.send(error);
            }
        }
    });
    timings.event("krun.thread.spawned");

    let owner = run_broker_owner(OwnerParts {
        layout: config.layout.clone(),
        vm: Arc::clone(&ctx),
        session: Arc::clone(session),
        agent_listener: listener,
        snapshot_listener,
        control_listener,
        broker_listener,
        broker_socket: broker_socket.to_path_buf(),
        initramfs_stamp,
        forwards: config.forwards.clone(),
        host_home: share_layout.host_home.clone(),
        no_host_shares: share_layout.no_host_shares,
        deterministic: config.deterministic.clone(),
        deterministic_clock_state: deterministic_clock_state.clone(),
        idle,
        timings: Arc::clone(&timings),
        run_log: Arc::clone(run_log),
        trace_log: trace_log.clone(),
        vm_errors: vm_error_rx,
        owner_run_id: owner_run_id.to_string(),
    });
    let owner = match owner {
        Ok(owner) => owner,
        Err(e) => {
            timings.event(&format!("restore.owner.error {e:#}"));
            run_log.line(format!("owner.start.error {e:#}"));
            log_console_tail(run_log, &config.layout.console_log);
            cleanup_runtime_sockets(
                run_log,
                &[broker_socket, &socket, &snapshot_socket, &control_socket],
            );
            return Err(e);
        }
    };
    Ok(VmHandles {
        owner,
        network,
        trace_log,
    })
}

fn request_checkpoint_with_timeout(
    socket: &Path,
    spec: &CheckpointSpec,
    timeout: Option<Duration>,
) -> Result<()> {
    let mut stream = connect_broker(socket)?;
    if let Some(timeout) = timeout {
        // Once the owner accepts a checkpoint request it may legitimately
        // spend longer than the broker-readiness deadline writing a large
        // snapshot. Do not time out and let the caller delete its output while
        // that write is still in flight.
        stream
            .set_write_timeout(Some(timeout))
            .context("set checkpoint request timeout")?;
    }
    let channel_id = new_request_id()?;
    write_message(
        &mut stream,
        &Message::Checkpoint {
            channel_id,
            request: serde_json::to_string(spec).context("encode checkpoint request")?,
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

/// Asks the instance's running VM owner to capture it as `spec` describes.
/// With `deterministic`, the request is refused unless the running VM uses
/// the same deterministic configuration.
pub(crate) fn request_live_checkpoint(
    layout: &Layout,
    spec: &CheckpointSpec,
    deterministic: Option<Option<&DeterministicConfig>>,
    timeout: Duration,
) -> Result<()> {
    request_checkpoint_from_current_owner(
        layout,
        spec,
        deterministic.flatten(),
        deterministic.is_some(),
        timeout,
    )
}

fn request_checkpoint_from_current_owner(
    layout: &Layout,
    spec: &CheckpointSpec,
    deterministic: Option<&DeterministicConfig>,
    validate_deterministic: bool,
    timeout: Duration,
) -> Result<()> {
    let broker_socket = layout.socket(RuntimeSocket::Broker);
    let current_owner = || live_owner(layout).map(|lease| lease.process);
    let expected =
        current_owner().context("checkpoint requested for an instance without a live VM owner")?;
    let expected_pid = expected.pid;
    let deadline = Instant::now() + timeout;
    loop {
        match current_owner() {
            Some(owner) if owner == expected => {}
            Some(owner) => bail!(
                "instance {} changed VM owner from pid {expected_pid} to pid {} while waiting to checkpoint",
                layout.instance,
                owner.pid
            ),
            None => {
                refuse_crashed_run(layout)?;
                bail!(
                    "instance {} VM owner exited before its broker became ready for checkpointing",
                    layout.instance
                );
            }
        }
        if broker_socket.exists() && connect_broker(&broker_socket).is_ok() {
            if current_owner() != Some(expected) {
                continue;
            }
            if validate_deterministic {
                validate_runtime_deterministic_compatibility(layout, deterministic)?;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "timed out waiting for instance {} VM owner broker before checkpointing",
                    layout.instance
                );
            }
            request_checkpoint_with_timeout(&broker_socket, spec, Some(remaining))?;
            if let Some(owner) = current_owner()
                && owner != expected
            {
                bail!(
                    "instance {} changed VM owner from pid {expected_pid} to pid {} while checkpointing",
                    layout.instance,
                    owner.pid
                );
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for instance {} VM owner broker before checkpointing",
                layout.instance
            );
        }
        thread::sleep(Duration::from_millis(10));
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

/// The client stops on SIGINT, SIGTERM and SIGHUP by closing its channel,
/// which ends the guest command's process group, instead of dying and
/// leaving the command running.
/// Tells the user, once, what an owner had to do when its run ended badly.
fn show_last_run_notice(layout: &Layout) {
    let path = layout.instance_dir.join(LAST_RUN_NOTICE);
    if let Ok(notice) = fs::read_to_string(&path) {
        eprint!("{notice}");
        let _ = fs::remove_file(&path);
    }
}

fn install_signal_handlers() {
    SIGNAL_INIT.call_once(|| {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            unsafe {
                libc::signal(
                    signal,
                    handle_client_interrupt as *const () as libc::sighandler_t,
                );
            }
        }
    });
}

fn install_owner_signal_handlers() {
    OWNER_SIGNAL_INIT.call_once(|| unsafe {
        libc::signal(
            libc::SIGTERM,
            handle_owner_shutdown as *const () as libc::sighandler_t,
        );
    });
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn configure_snapshot_restore_compat(_restore_snapshot: Option<&Path>, _run_log: &RunLog) {}

#[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
fn configure_snapshot_restore_compat(_restore_snapshot: Option<&Path>, _run_log: &RunLog) {}

fn owner_restart_args(config: &RunConfig) -> Vec<String> {
    let mut args = vec![
        "--instance".to_string(),
        config.layout.instance.clone(),
        "--kernel".to_string(),
        config.layout.kernel.display().to_string(),
        "--cpus".to_string(),
        config.cpus.to_string(),
        "--memory-mib".to_string(),
        config.memory_mib.to_string(),
    ];
    if config.nested_kvm {
        args.push("--nested-kvm".to_string());
    }
    if config.no_host_shares {
        args.push("--no-host-shares".to_string());
    }
    if let Some(deterministic) = &config.deterministic {
        args.push("--deterministic".to_string());
        args.push(deterministic.seed.clone());
    }
    if config.trace_events {
        args.push("--trace-events".to_string());
    }
    for forward in &config.forwards {
        args.push("--forward".to_string());
        args.push(forward_spec(forward));
    }
    for mount in &config.vhost_user_fs {
        args.push("--vhost-user-fs".to_string());
        args.push(vhost_user_fs_arg(mount));
    }
    args.push("_vm-owner".to_string());
    args.push("--cwd".to_string());
    args.push(config.cwd.display().to_string());
    if let Some(snapshot) = &config.restore_snapshot {
        args.push("--restore".to_string());
        args.push(snapshot.display().to_string());
    }
    args
}

fn vhost_user_fs_guest_env(mounts: &[VhostUserFsMount]) -> String {
    mounts
        .iter()
        .map(|mount| {
            format!(
                "{}:{}:{}",
                mount.tag,
                mount.mountpoint,
                if mount.read_only { "ro" } else { "rw" }
            )
        })
        .collect::<Vec<_>>()
        .join(";")
}

struct Gvproxy {
    socket: PathBuf,
    embedded: Option<crate::gvproxy_embedded::EmbeddedGvproxy>,
}

impl Drop for Gvproxy {
    fn drop(&mut self) {
        drop(self.embedded.take());
        let _ = fs::remove_file(&self.socket);
        let mut krun_socket = self.socket.clone().into_os_string();
        krun_socket.push(GVPROXY_KRUN_SOCKET_SUFFIX);
        let _ = fs::remove_file(krun_socket);
    }
}

fn start_gvproxy(socket: &Path, log: &Path) -> Result<Gvproxy> {
    ensure_unix_socket_path_fits(socket)?;
    let _ = fs::remove_file(socket);
    let ssh_port = unused_local_port().context("find unused localhost port for gvproxy ssh")?;

    let embedded = crate::gvproxy_embedded::EmbeddedGvproxy::start(socket, log, ssh_port)?;
    wait_for_path(socket, Duration::from_secs(30))
        .with_context(|| format!("embedded gvproxy did not create {}", socket.display()))?;
    Ok(Gvproxy {
        socket: socket.to_path_buf(),
        embedded: Some(embedded),
    })
}

fn unused_local_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn wait_for_path(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut attempts = 0usize;
    loop {
        if path_is_visible(path) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            break;
        }
        attempts = attempts.wrapping_add(1);
        // In nested Linux, the musl build has been observed to wedge in a
        // short nanosleep here even after gvproxy has created the socket.
        // Yielding keeps this startup wait responsive without relying on
        // guest timer delivery.
        if attempts & 0x7f == 0 {
            thread::yield_now();
        } else {
            std::hint::spin_loop();
        }
    }
    bail!("timed out waiting for {}", path.display())
}

fn path_is_visible(path: &Path) -> bool {
    if path.exists() {
        return true;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    let Some(name) = path.file_name() else {
        return false;
    };
    parent
        .read_dir()
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .any(|entry| entry.file_name() == name)
        })
        .unwrap_or(false)
}

/// Fails with an actionable message instead of the platform's opaque
/// `EINVAL`/`ENAMETOOLONG` when a socket path does not fit in `sun_path`.
fn ensure_unix_socket_path_fits(path: &Path) -> Result<()> {
    if unix_socket_path_fits(path) {
        return Ok(());
    }
    bail!(
        "unix socket path is {} bytes, longer than the {} bytes the OS allows: {}",
        path.as_os_str().len(),
        UNIX_SOCKET_PATH_CAPACITY - 1,
        path.display()
    )
}

fn bind_unix_listener(path: &Path) -> Result<UnixListener> {
    ensure_unix_socket_path_fits(path)?;
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

/// Either the lock a caller asked for, or the exit status of its command
/// after it attached to a VM owner that another process started meanwhile.
enum OwnerStartOutcome {
    Lock(OwnerStartLock),
    Status(i32),
}

fn acquire_owner_start_or_run_client(
    socket: &Path,
    config: &RunConfig,
    allow_existing_broker: bool,
    run_log: &RunLog,
) -> Result<OwnerStartOutcome> {
    let layout = &config.layout;
    let lock_path = owner_start_lock_path(layout);
    let start = Instant::now();
    let mut logged_wait = false;
    loop {
        if let Some(lock) = OwnerStartLock::try_acquire(layout)? {
            run_log.line(format!(
                "owner_start.lock.acquired path={}",
                lock_path.display()
            ));
            return Ok(OwnerStartOutcome::Lock(lock));
        }
        if !logged_wait {
            run_log.line(format!(
                "owner_start.lock.busy path={}",
                lock_path.display()
            ));
            logged_wait = true;
        }
        if allow_existing_broker {
            if let Some(status) = run_existing_broker_client(socket, config, Some(run_log))? {
                return Ok(OwnerStartOutcome::Status(status));
            }
        }
        if start.elapsed() > Duration::from_secs(120) {
            run_log.line(format!(
                "owner_start.lock.timeout path={}",
                lock_path.display()
            ));
            bail!("timed out waiting for {}", lock_path.display());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn run_existing_broker_client(
    socket: &Path,
    config: &RunConfig,
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
            run_broker_session(stream, config).map(Some)
        }
        Err(e) => {
            if e.downcast_ref::<BrokerProtocolMismatch>().is_some() {
                return Err(e);
            }
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
    stream
        .set_read_timeout(Some(BROKER_HELLO_TIMEOUT))
        .context("set broker hello read timeout")?;
    stream
        .set_write_timeout(Some(BROKER_HELLO_TIMEOUT))
        .context("set broker hello write timeout")?;
    write_message(
        &mut stream,
        &Message::Hello {
            version: PROTOCOL_VERSION,
        },
    )?;
    let hello = match read_message(&mut stream) {
        Ok(message) => message,
        Err(_) => return Err(BrokerHelloFailed.into()),
    };
    match hello {
        Message::Hello { version } if version == PROTOCOL_VERSION => {}
        Message::Hello { version } => {
            return Err(BrokerProtocolMismatch {
                expected: PROTOCOL_VERSION,
                actual: version,
            }
            .into());
        }
        other => bail!("bad broker hello: {other:?}"),
    }
    stream
        .set_read_timeout(None)
        .context("clear broker hello read timeout")?;
    stream
        .set_write_timeout(None)
        .context("clear broker hello write timeout")?;
    Ok(stream)
}

fn run_broker_session(mut stream: UnixStream, config: &RunConfig) -> Result<i32> {
    let RunConfig {
        command,
        cwd,
        exec,
        no_host_shares,
        ..
    } = config;
    let deterministic = config.deterministic.as_ref();
    INTERRUPT_SIGNAL.store(0, Ordering::SeqCst);
    // Validate the cwd resolves to a host home directory even when
    // no_host_shares is set, matching the eager-validation pattern used
    // elsewhere (e.g. snapshot_shares_incompatibility_for_import).
    host_home_for_cwd(cwd)?;
    let default_cwd = if *no_host_shares {
        "/".to_string()
    } else {
        guest_cwd(cwd)
    };
    let guest_cwd = exec_workdir(&default_cwd, exec.workdir.as_deref());
    let detached;
    let command = if exec.detach {
        detached = detached_argv(command);
        &detached
    } else {
        command
    };
    let use_pty = if deterministic.is_some() || exec.detach {
        false
    } else {
        should_request_pty()
    };
    let raw_mode = if use_pty { RawTerminal::enter() } else { None };
    let (term, colorterm, rows, cols) = if deterministic.is_some() {
        (
            DETERMINISTIC_TERM.to_string(),
            DETERMINISTIC_COLORTERM.to_string(),
            DETERMINISTIC_ROWS,
            DETERMINISTIC_COLS,
        )
    } else if use_pty {
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
    let (uid, gid, group) = exec_identity(exec.run_as_root, deterministic);
    let mut env = exec_env(deterministic);
    env.push(("LNX_INSTANCE".to_string(), config.layout.instance.clone()));
    env.push(("LNX_INGRESS_DOMAIN".to_string(), ingress_domain()));
    env.extend(exec.env.iter().cloned());
    let channel_id = match deterministic {
        Some(config) => deterministic_exec_request_id(
            &config.seed,
            command,
            &guest_cwd,
            exec.run_as_root,
            use_pty,
            rows,
            cols,
        ),
        None => new_request_id()?,
    };
    write_message(
        &mut stream,
        &Message::OpenExec {
            channel_id,
            argv: command.to_vec(),
            cwd: guest_cwd,
            pty: use_pty,
            term,
            colorterm,
            rows,
            cols,
            uid,
            gid,
            group,
            env,
        },
    )?;

    if deterministic.is_some() && !is_tty(std::io::stdin().as_raw_fd()) {
        // A deterministic run must see the same input chunks every time.
        send_all_stdin(&mut stream, channel_id)?;
    } else {
        spawn_stdin_pump(&stream, channel_id)?;
    }
    let status = relay_channel_output(&mut stream, channel_id, exec.timeout);
    drop(raw_mode);
    status
}

fn send_all_stdin(stream: &mut UnixStream, channel_id: u64) -> Result<()> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .read_to_end(&mut bytes)
        .context("read stdin")?;
    if !bytes.is_empty() {
        write_message(stream, &Message::Data { channel_id, bytes })?;
    }
    write_message(stream, &Message::Eof { channel_id })
}

/// Forwards stdin to the guest as it arrives, then its end.
fn spawn_stdin_pump(stream: &UnixStream, channel_id: u64) -> Result<()> {
    let mut input_stream = stream
        .try_clone()
        .context("clone broker stream for stdin")?;
    thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut input = [0u8; 8192];
        loop {
            match stdin.read(&mut input) {
                Ok(0) => {
                    let _ = write_message(&mut input_stream, &Message::Eof { channel_id });
                    break;
                }
                Ok(n) => {
                    let data = Message::Data {
                        channel_id,
                        bytes: input[..n].to_vec(),
                    };
                    if write_message(&mut input_stream, &data).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
    Ok(())
}

/// Copies the guest command's output to stdout and stderr until it exits.
/// A client stopped by a signal, or a command that runs past `timeout`,
/// closes the channel, which ends the command.
fn relay_channel_output(
    stream: &mut UnixStream,
    channel_id: u64,
    timeout: Option<Duration>,
) -> Result<i32> {
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    loop {
        let message = match read_message_interruptible(stream, deadline)? {
            Interruptible::Message(message) => message,
            stopped => {
                let _ = write_message(stream, &Message::Eof { channel_id });
                let _ = write_message(stream, &Message::Close { channel_id });
                if matches!(stopped, Interruptible::DeadlinePassed) {
                    eprintln!(
                        "lnx: the command ran longer than {} and was stopped",
                        humantime_seconds(timeout.unwrap_or_default())
                    );
                    return Ok(EXIT_TIMED_OUT);
                }
                return Ok(interrupted_status());
            }
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
            } if id == channel_id => return Ok(status),
            Message::Error {
                channel_id: id,
                message,
            } if id == channel_id => {
                if message == OWNER_STOPPING_NOT_STARTED {
                    return Err(CommandNotStarted.into());
                }
                bail!("{message}")
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

fn exec_identity(
    run_as_root: bool,
    deterministic: Option<&DeterministicConfig>,
) -> (u32, u32, String) {
    if run_as_root {
        return (0, 0, String::new());
    }
    if deterministic.is_some() {
        return (
            DETERMINISTIC_EXEC_UID,
            DETERMINISTIC_EXEC_GID,
            DETERMINISTIC_EXEC_GROUP.to_string(),
        );
    }
    (
        unsafe { libc::getuid() },
        unsafe { libc::getgid() },
        host_group_name(),
    )
}

fn exec_env(deterministic: Option<&DeterministicConfig>) -> Vec<(String, String)> {
    if deterministic.is_some() {
        return vec![
            ("TERM".to_string(), DETERMINISTIC_TERM.to_string()),
            ("LANG".to_string(), "C.UTF-8".to_string()),
            ("LC_ALL".to_string(), "C.UTF-8".to_string()),
            ("TZ".to_string(), "UTC".to_string()),
        ];
    }
    forwarded_exec_env()
}

fn ingress_domain() -> String {
    crate::ingress::load_config()
        .map(|config| config.domain)
        .unwrap_or_else(|_| "lnx".to_string())
}

fn open_url_on_host(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(url);
        command
    };
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        bail!("opening URLs on the host is not supported on this platform");
    }

    let status = command.status().context("launch host browser")?;
    if status.success() {
        Ok(())
    } else {
        bail!("host browser launcher exited with {status}")
    }
}

fn localhost_url_forward(url: &str) -> Option<(&'static str, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.contains('@') {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        if host != "::1" {
            return None;
        }
        return Some(("::1", tail.strip_prefix(':')?.parse().ok()?));
    }
    let (host, port) = authority.rsplit_once(':')?;
    if host.contains(':') || !matches!(host, "localhost" | "127.0.0.1") {
        return None;
    }
    Some(("127.0.0.1", port.parse().ok()?))
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
    if !env.iter().any(|(key, _)| key == "TZ") {
        if let Some(zone) = host_timezone() {
            env.push(("TZ".to_string(), zone));
        }
    }
    env
}

/// IANA zone name of the host when `$TZ` is unset: `/etc/localtime` is a
/// symlink into a zoneinfo tree on macOS and most Linux distributions, with
/// `/etc/timezone` as the Debian fallback.
fn host_timezone() -> Option<String> {
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        if let Some(zone) = zone_from_localtime_target(&target.to_string_lossy()) {
            return Some(zone);
        }
    }
    let zone = std::fs::read_to_string("/etc/timezone").ok()?;
    let zone = zone.trim();
    (!zone.is_empty()).then(|| zone.to_string())
}

fn zone_from_localtime_target(target: &str) -> Option<String> {
    let (_, zone) = target.rsplit_once("/zoneinfo/")?;
    (!zone.is_empty()).then(|| zone.to_string())
}

#[allow(clippy::too_many_arguments)]
/// Everything the owner thread needs once the VM is starting.
struct OwnerParts {
    layout: Layout,
    vm: Arc<VmHandle>,
    session: Arc<RunSession>,
    agent_listener: UnixListener,
    snapshot_listener: UnixListener,
    control_listener: UnixListener,
    broker_listener: UnixListener,
    broker_socket: PathBuf,
    initramfs_stamp: PathBuf,
    forwards: Vec<PortForward>,
    host_home: PathBuf,
    no_host_shares: bool,
    deterministic: Option<DeterministicConfig>,
    deterministic_clock_state: Option<DeterministicClockState>,
    idle: IdlePolicy,
    timings: Arc<TimingLog>,
    run_log: Arc<RunLog>,
    trace_log: Option<Arc<TraceLog>>,
    vm_errors: mpsc::Receiver<KrunError>,
    owner_run_id: String,
}

fn run_broker_owner(parts: OwnerParts) -> Result<thread::JoinHandle<Result<()>>> {
    let OwnerParts {
        layout,
        vm: ctx,
        session,
        agent_listener: listener,
        snapshot_listener,
        control_listener: _control_listener,
        broker_listener,
        broker_socket,
        initramfs_stamp,
        forwards,
        host_home,
        no_host_shares,
        deterministic,
        deterministic_clock_state,
        idle,
        timings,
        run_log,
        trace_log,
        vm_errors: vm_error_rx,
        owner_run_id,
    } = parts;
    let console_log = layout.console_log.clone();
    let restores = session.run().restores_memory();
    listener
        .set_nonblocking(true)
        .context("set lnx-agent listener nonblocking")?;
    let agent_timeout = agent_accept_timeout_from_env(
        std::env::var("LNX_AGENT_TIMEOUT_MS").ok(),
        restores,
    );
    timings.event("agent.accept.begin");
    run_log.line(format!(
        "agent.accept.begin owner_run_id={} timeout_ms={} restore={restores}",
        owner_run_id,
        agent_timeout.as_millis(),
    ));
    let restore_snapshot_unblocker = if restores {
        Some(spawn_restore_snapshot_unblocker(
            &snapshot_listener,
            agent_timeout,
            Arc::clone(&timings),
            Arc::clone(&run_log),
        )?)
    } else {
        None
    };
    if restores {
        maybe_spawn_restore_proof_snapshotter(Arc::clone(&ctx), Arc::clone(&run_log));
    }
    let accept_result =
        accept_agent_hello(&listener, agent_timeout, &timings, &run_log, &vm_error_rx);
    if let Some(unblocker) = restore_snapshot_unblocker {
        unblocker.stop(&run_log);
    }
    let mut agent_stream = match accept_result {
        Ok(stream) => stream,
        Err(e) => {
            run_log.line(format!("agent.accept.error {e:#}"));
            log_console_tail(&run_log, &console_log);
            let e = e.context(console_hint(&console_log));
            // A restored guest that never reconnects means the devices
            // refused the memory image; tag it as a hard restore failure.
            if restores {
                return Err(e.context(RestoreRefused));
            }
            return Err(e);
        }
    };
    write_message(
        &mut agent_stream,
        &Message::Hello {
            version: PROTOCOL_VERSION,
        },
    )?;

    if restores {
        let channel_id = match deterministic.as_ref() {
            Some(config) => deterministic_restore_sync_request_id(&config.seed),
            None => new_request_id()?,
        };
        timings.event("snapshot.restore.sync.begin");
        run_log.line(format!(
            "snapshot.restore.sync.begin channel_id={channel_id:016x}"
        ));
        agent_stream
            .set_read_timeout(Some(agent_timeout))
            .context("set restore-sync read timeout")?;
        let sync_result = (|| -> Result<()> {
            let entropy = restore_entropy(deterministic.as_ref())?;
            if let Some(trace) = &trace_log {
                trace.event(
                    "restore_sync_begin",
                    vec![
                        trace_text("channel_id", format!("{channel_id:016x}")),
                        trace_blob("entropy", &entropy),
                    ],
                );
            }
            write_message(
                &mut agent_stream,
                &Message::RestoreSync {
                    channel_id,
                    entropy,
                },
            )?;
            loop {
                match read_message(&mut agent_stream)? {
                    Message::RestoreSynced { channel_id: id } if id == channel_id => return Ok(()),
                    Message::Error {
                        channel_id: id,
                        message,
                    } if id == channel_id => bail!("{message}"),
                    Message::Hello { .. } => {}
                    _ => {}
                }
            }
        })();
        let _ = agent_stream.set_read_timeout(None);
        match sync_result {
            Ok(()) => {
                timings.event("snapshot.restore.sync.done");
                run_log.line(format!(
                    "snapshot.restore.sync.done channel_id={channel_id:016x}"
                ));
                if let Some(trace) = &trace_log {
                    trace.event(
                        "restore_sync_done",
                        vec![trace_text("channel_id", format!("{channel_id:016x}"))],
                    );
                }
            }
            Err(e) => {
                timings.event("snapshot.restore.sync.error");
                return Err(e.context("restore sync failed").context(RestoreRefused));
            }
        }
    }

    let (agent_tx, agent_rx) = mpsc::channel::<Message>();
    let dispatch_session = Arc::clone(&session);
    let state = BrokerState::new(
        agent_tx.clone(),
        idle.starts_idle,
        move || dispatch_session.mark_dirty(),
        Arc::clone(&run_log),
    );
    let agent_failed_before_snapshot = Arc::new(AtomicBool::new(false));
    let snapshot_started = Arc::new(AtomicBool::new(false));

    let mut agent_writer = agent_stream
        .try_clone()
        .context("clone lnx-agent stream for writer")?;
    thread::spawn(move || {
        while let Ok(message) = agent_rx.recv() {
            let _activity = krun::deterministic_host_activity();
            if write_message(&mut agent_writer, &message).is_err() {
                break;
            }
        }
    });

    let capture_worker = CaptureWorker::spawn(Capturer {
        vm: Arc::clone(&ctx),
        session: Arc::clone(&session),
        initramfs_stamp: initramfs_stamp.clone(),
        deterministic_clock_state: deterministic_clock_state.clone(),
        agent_tx: agent_tx.clone(),
        timings: Arc::clone(&timings),
        run_log: Arc::clone(&run_log),
        trace_log: trace_log.clone(),
        owner_run_id: owner_run_id.clone(),
    });

    {
        let state = Arc::clone(&state);
        let captures = capture_worker.jobs();
        let snapshot_started = Arc::clone(&snapshot_started);
        let agent_failed_before_snapshot = Arc::clone(&agent_failed_before_snapshot);
        let trace_log = trace_log.clone();
        thread::spawn(move || {
            run_agent_reader(
                agent_stream,
                state,
                captures,
                snapshot_started,
                agent_failed_before_snapshot,
                trace_log,
            )
        });
    }

    broker_listener
        .set_nonblocking(true)
        .context("set broker listener nonblocking")?;
    for forward in forwards {
        reserve_forward_port(&state, &forward);
        start_forward_listener(forward, &state)?;
    }
    let client_context = Arc::new(ClientContext {
        state: Arc::clone(&state),
        captures: capture_worker.jobs(),
        vm: Arc::clone(&ctx),
        host_home,
        no_host_shares,
        trace_log: trace_log.clone(),
    });
    let broker_idle_ttl = idle.ttl;
    Ok(thread::spawn(move || {
        timings.event("broker.ready");
        run_log.line(format!(
            "broker.ready socket={} idle_ttl_ms={}",
            broker_socket.display(),
            broker_idle_ttl.as_millis()
        ));
        let mut idle_deadline: Option<Instant> = None;
        loop {
            if OWNER_SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
                timings.event("owner.shutdown.requested");
                let dropped = state.begin_shutdown(OWNER_STOPPING);
                run_log.line(format!(
                    "owner.shutdown.requested owner_run_id={owner_run_id} active_clients={} notified_clients={dropped}",
                    state.active_channels(),
                ));
                break;
            }
            match broker_listener.accept() {
                Ok((client, _)) => {
                    run_log.line("broker.client.accepted");
                    // Counted from accept so the owner cannot stop between a
                    // client connecting and its request arriving.
                    let pending = state.pending_connection();
                    let context = Arc::clone(&client_context);
                    thread::spawn(move || {
                        if let Err(error) = handle_broker_client(client, pending, &context) {
                            context
                                .state
                                .run_log()
                                .line(format!("broker.client.error {error:#}"));
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if agent_failed_before_snapshot.load(Ordering::SeqCst) {
                        break;
                    }
                    let status = state.idle_status();
                    if status.busy {
                        idle_deadline = None;
                    } else if status.seen_active {
                        let deadline =
                            idle_deadline.get_or_insert_with(|| Instant::now() + broker_idle_ttl);
                        if Instant::now() >= *deadline && !status.pending {
                            break;
                        }
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        // Establish the same registration barrier for every loop exit, not
        // only signal-driven shutdown. This closes the accept/idle race before
        // the final snapshot begins.
        let dropped = state.begin_shutdown(OWNER_STOPPING);
        run_log.line(format!(
            "broker.shutdown.barrier owner_run_id={owner_run_id} active_clients={} notified_clients={dropped}",
            state.active_channels(),
        ));
        let _ = fs::remove_file(&broker_socket);
        drop(broker_listener);
        // Checkpoints and snapshot-exits accepted before the barrier finish
        // before the final snapshot captures the VM.
        capture_worker.finish()?;
        if agent_failed_before_snapshot.load(Ordering::SeqCst) {
            timings.event("snapshot.skipped.agent_failed");
            run_log.line("snapshot.skipped reason=guest_agent_disconnected_before_snapshot");
            return Err(anyhow!(
                "guest agent disconnected before the final snapshot"
            ));
        }
        snapshot_started.store(true, Ordering::SeqCst);
        timings.event("snapshot.request.guest");
        run_log.line(format!(
            "snapshot.request.guest owner_run_id={owner_run_id} run={}",
            session.run().id
        ));
        if let Some(trace) = &trace_log {
            trace.event("snapshot_request_guest", Vec::new());
        }
        let _ = agent_tx.send(Message::SnapshotReady);
        let capture = CaptureContext {
            vm: &ctx,
            session: &session,
            initramfs_stamp: &initramfs_stamp,
            trace_log: trace_log.as_deref(),
            deterministic_clock_state: deterministic_clock_state.as_ref(),
        };
        let result = serve_snapshot(snapshot_listener, &capture, &timings).and_then(|id| {
            session.commit_final(&id)?;
            Ok(id)
        });
        match result {
            Ok(id) => {
                run_log.line(format!(
                    "snapshot.done owner_run_id={owner_run_id} generation={id}"
                ));
                if let Some(trace) = &trace_log {
                    trace.event("snapshot_done", Vec::new());
                }
                Ok(())
            }
            Err(error) => {
                run_log.line(format!(
                    "snapshot.error owner_run_id={owner_run_id} error={error:#}"
                ));
                Err(error)
            }
        }
    }))
}

fn maybe_spawn_restore_proof_snapshotter(ctx: Arc<VmHandle>, run_log: Arc<RunLog>) {
    let Some(path) = std::env::var_os("LNX_RESTORE_PROOF_SNAPSHOT_DIR").map(PathBuf::from) else {
        return;
    };
    let delay = std::env::var("LNX_RESTORE_PROOF_SNAPSHOT_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_millis(250));
    run_log.line(format!(
        "restore.proof_snapshot.scheduled path={} delay_ms={}",
        path.display(),
        delay.as_millis()
    ));
    thread::spawn(move || {
        thread::sleep(delay);
        run_log.line(format!(
            "restore.proof_snapshot.begin path={}",
            path.display()
        ));
        match ctx.snapshot(&path) {
            Ok(()) => {
                run_log.line(format!(
                    "restore.proof_snapshot.done path={}",
                    path.display()
                ));
            }
            Err(e) => {
                run_log.line(format!(
                    "restore.proof_snapshot.error path={} error={e:#}",
                    path.display()
                ));
            }
        }
    });
}

fn owner_idle_ttl() -> Duration {
    if debug_flag_enabled("nodaemonreuse") {
        Duration::ZERO
    } else {
        owner_idle_ttl_from_env(std::env::var("LNX_BROKER_IDLE_TTL_MS").ok().as_deref())
    }
}

fn owner_idle_ttl_from_env(value: Option<&str>) -> Duration {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_OWNER_IDLE_TTL)
        .max(MIN_OWNER_IDLE_TTL)
}

fn debug_flag_enabled(flag: &str) -> bool {
    debug_flag_enabled_in(std::env::var("LNX_DEBUG").ok().as_deref(), flag)
}

fn debug_flag_enabled_in(value: Option<&str>, flag: &str) -> bool {
    value.is_some_and(|value| {
        value
            .split([',', ':', ';', ' ', '\t', '\n'])
            .any(|part| part == flag)
    })
}

fn forward_spec(forward: &PortForward) -> String {
    format!(
        "{}:{}:{}:{}",
        forward.listen_host, forward.listen_port, forward.guest_host, forward.guest_port
    )
}

fn spawn_owner_process(config: &RunConfig, run_log: &RunLog, run_id: &str) -> Result<Child> {
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
        .arg("--cpus")
        .arg(config.cpus.to_string())
        .arg("--memory-mib")
        .arg(config.memory_mib.to_string());
    if config.nested_kvm {
        command.arg("--nested-kvm");
    }
    if config.no_host_shares {
        command.arg("--no-host-shares");
    }
    if let Some(deterministic) = &config.deterministic {
        command.arg("--deterministic").arg(&deterministic.seed);
    }
    if config.trace_events {
        command.arg("--trace-events");
    }
    for forward in &config.forwards {
        command.arg("--forward").arg(forward_spec(forward));
    }
    for mount in &config.vhost_user_fs {
        command.arg("--vhost-user-fs").arg(vhost_user_fs_arg(mount));
    }
    command.arg("_vm-owner").arg("--cwd").arg(&config.cwd);
    if let Some(snapshot) = &config.restore_snapshot {
        command.arg("--restore").arg(snapshot);
    }
    command
        .stdin(Stdio::null())
        .stdout(log.try_clone().context("clone owner log handle")?)
        .stderr(log)
        .env(RUN_ID_ENV, run_id)
        .process_group(0);
    let child = command.spawn().context("spawn lnx _vm-owner")?;
    run_log.line(format!("owner.spawned run_id={run_id} pid={}", child.id()));
    Ok(child)
}

pub(crate) fn vhost_user_fs_arg(mount: &VhostUserFsMount) -> String {
    format!(
        "tag={},mount={},socket={}{}",
        mount.tag,
        mount.mountpoint,
        mount.socket.display(),
        if mount.read_only { ",ro" } else { "" }
    )
}

fn run_broker_client_awaiting_owner(
    socket: &Path,
    owner: &mut Child,
    config: &RunConfig,
    run_log: &RunLog,
) -> Result<i32> {
    let layout = &config.layout;
    let deadline = Instant::now() + OWNER_BOOT_TIMEOUT;
    let mut last = None;
    while Instant::now() < deadline {
        if client_interrupted() {
            return Ok(interrupted_status());
        }
        match connect_broker(socket) {
            Ok(stream) => {
                return run_broker_session(stream, config);
            }
            Err(e) => {
                if e.downcast_ref::<BrokerProtocolMismatch>().is_some() {
                    return Err(e);
                }
                last = Some(e);
            }
        }
        if let Some(status) = owner.try_wait().context("check lnx _vm-owner")? {
            if status.code() == Some(EXIT_RESTORE_FAILED) {
                run_log.line(format!(
                    "owner.exited.early status={status} restore_failed=true"
                ));
                bail!(
                    "the saved memory snapshot could not be resumed: the guest did not come back\n{}{}{}",
                    drop_memory_guidance(&layout.instance),
                    owner_log_hint(layout),
                    console_hint(&layout.console_log)
                );
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

fn acquire_instance_for_owner(
    layout: &Layout,
    broker_socket: &Path,
    run_log: &RunLog,
) -> Result<Option<InstanceLock>> {
    let lock_path = instance_lock_path(layout);
    let start = Instant::now();
    let mut logged_wait = false;
    loop {
        if let Some(lock) = InstanceLock::try_acquire(layout, LeaseRole::Owner, |_| Ok(()))? {
            run_log.line(format!(
                "owner.instance.lock.acquired path={}",
                lock_path.display()
            ));
            return Ok(Some(lock));
        }
        if !logged_wait {
            run_log.line(format!(
                "owner.instance.lock.busy path={}",
                lock_path.display()
            ));
            logged_wait = true;
        }
        if connect_broker(broker_socket).is_ok() {
            return Ok(None);
        }
        if start.elapsed() > Duration::from_secs(120) {
            run_log.line(format!(
                "owner.instance.lock.timeout path={}",
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

fn reset_owner_attempt_logs(layout: &Layout, run_log: &RunLog) {
    for (label, path) in [
        ("owner", layout.run_dir.join("owner.log")),
        ("console", layout.console_log.clone()),
    ] {
        match OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
        {
            Ok(_) => run_log.line(format!("{label}.log.reset path={}", path.display())),
            Err(e) => run_log.line(format!(
                "{label}.log.reset_error path={} error={e}",
                path.display()
            )),
        }
    }
}

/// How long to wait for the guest agent. A booting guest may take a while;
/// a resumed one reconnects within milliseconds, so a long wait there only
/// delays reporting a snapshot that cannot be resumed.
fn agent_accept_timeout_from_env(value: Option<String>, restores: bool) -> Duration {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(if restores {
            RESTORE_AGENT_ACCEPT_TIMEOUT
        } else {
            DEFAULT_AGENT_ACCEPT_TIMEOUT
        })
}

fn trace_client_open(trace: &TraceLog, message: &Message) {
    match message {
        Message::OpenExec {
            channel_id,
            argv,
            cwd,
            pty,
            term,
            colorterm,
            rows,
            cols,
            uid,
            gid,
            group,
            env,
        } => trace.event(
            "client_open_exec",
            trace_open_exec_fields(
                *channel_id,
                argv,
                cwd,
                *pty,
                term,
                colorterm,
                *rows,
                *cols,
                *uid,
                *gid,
                group,
                env,
            ),
        ),
        Message::OpenTcp {
            channel_id,
            host,
            port,
        } => trace.event(
            "client_open_tcp",
            vec![
                trace_text("channel_id", format!("{channel_id:016x}")),
                trace_text("host", host),
                trace_integer("port", *port as i64),
            ],
        ),
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn trace_open_exec_fields(
    channel_id: u64,
    argv: &[String],
    cwd: &str,
    pty: bool,
    term: &str,
    colorterm: &str,
    rows: u16,
    cols: u16,
    uid: u32,
    gid: u32,
    group: &str,
    env: &[(String, String)],
) -> Vec<TraceField> {
    let mut fields = vec![
        trace_text("channel_id", format!("{channel_id:016x}")),
        trace_text("cwd", cwd),
        trace_bool("pty", pty),
        trace_text("term", term),
        trace_text("colorterm", colorterm),
        trace_integer("rows", rows as i64),
        trace_integer("cols", cols as i64),
        trace_integer("uid", uid as i64),
        trace_integer("gid", gid as i64),
        trace_text("group", group),
    ];
    for (index, arg) in argv.iter().enumerate() {
        fields.push(trace_text_ordinal("argv", index, arg));
    }
    for (index, (key, value)) in env.iter().enumerate() {
        fields.push(trace_text_ordinal("env_key", index, key));
        fields.push(trace_text_ordinal("env_value", index, value));
    }
    fields
}

fn trace_client_message(trace: &TraceLog, message: &Message) {
    match message {
        Message::Data { channel_id, bytes } => trace.event(
            "client_stdin",
            vec![
                trace_text("channel_id", format!("{channel_id:016x}")),
                trace_integer("len", bytes.len() as i64),
                trace_blob("bytes", bytes),
            ],
        ),
        Message::Eof { channel_id } => trace.event(
            "client_eof",
            vec![trace_text("channel_id", format!("{channel_id:016x}"))],
        ),
        Message::Close { channel_id } => trace.event(
            "client_close",
            vec![trace_text("channel_id", format!("{channel_id:016x}"))],
        ),
        Message::WindowResize {
            channel_id,
            rows,
            cols,
        } => trace.event(
            "client_window_resize",
            vec![
                trace_text("channel_id", format!("{channel_id:016x}")),
                trace_integer("rows", *rows as i64),
                trace_integer("cols", *cols as i64),
            ],
        ),
        _ => {}
    }
}

fn trace_agent_message(trace: &TraceLog, message: &Message) {
    match message {
        Message::Data { channel_id, bytes } => trace.event(
            "guest_stdout",
            vec![
                trace_text("channel_id", format!("{channel_id:016x}")),
                trace_integer("len", bytes.len() as i64),
                trace_blob("bytes", bytes),
            ],
        ),
        Message::Stderr { channel_id, bytes } => trace.event(
            "guest_stderr",
            vec![
                trace_text("channel_id", format!("{channel_id:016x}")),
                trace_integer("len", bytes.len() as i64),
                trace_blob("bytes", bytes),
            ],
        ),
        Message::ExecStarted { channel_id } => trace.event(
            "guest_exec_started",
            vec![trace_text("channel_id", format!("{channel_id:016x}"))],
        ),
        Message::ExitStatus { channel_id, status } => trace.event(
            "guest_exit_status",
            vec![
                trace_text("channel_id", format!("{channel_id:016x}")),
                trace_integer("status", *status as i64),
            ],
        ),
        Message::Eof { channel_id } => trace.event(
            "guest_eof",
            vec![trace_text("channel_id", format!("{channel_id:016x}"))],
        ),
        Message::Close { channel_id } => trace.event(
            "guest_close",
            vec![trace_text("channel_id", format!("{channel_id:016x}"))],
        ),
        Message::Error {
            channel_id,
            message,
        } => trace.event(
            "guest_error",
            vec![
                trace_text("channel_id", format!("{channel_id:016x}")),
                trace_text("message", message),
            ],
        ),
        Message::CheckpointCreated { channel_id } => trace.event(
            "guest_checkpoint_created",
            vec![trace_text("channel_id", format!("{channel_id:016x}"))],
        ),
        _ => {}
    }
}

struct RestoreSnapshotUnblocker {
    cancel: Arc<AtomicBool>,
    handle: thread::JoinHandle<()>,
}

impl RestoreSnapshotUnblocker {
    fn stop(self, run_log: &RunLog) {
        self.cancel.store(true, Ordering::SeqCst);
        if self.handle.join().is_err() {
            run_log.line("snapshot.restore.unblock.thread_panicked");
        }
    }
}

fn spawn_restore_snapshot_unblocker(
    listener: &UnixListener,
    timeout: Duration,
    timings: Arc<TimingLog>,
    run_log: Arc<RunLog>,
) -> Result<RestoreSnapshotUnblocker> {
    let listener = listener
        .try_clone()
        .context("clone snapshot listener for restore unblocker")?;
    let cancel = Arc::new(AtomicBool::new(false));
    let thread_cancel = Arc::clone(&cancel);
    let handle = thread::spawn(move || {
        if let Err(e) =
            unblock_restore_snapshot_wait(listener, timeout, &thread_cancel, &timings, &run_log)
        {
            run_log.line(format!("snapshot.restore.unblock.error {e:#}"));
        }
    });
    Ok(RestoreSnapshotUnblocker { cancel, handle })
}

fn unblock_restore_snapshot_wait(
    listener: UnixListener,
    timeout: Duration,
    cancel: &AtomicBool,
    timings: &TimingLog,
    run_log: &RunLog,
) -> Result<()> {
    listener
        .set_nonblocking(true)
        .context("set restore snapshot listener nonblocking")?;
    timings.event("snapshot.restore.unblock.begin");
    run_log.line(format!(
        "snapshot.restore.unblock.begin timeout_ms={}",
        timeout.as_millis()
    ));
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cancel.load(Ordering::SeqCst) {
            timings.event("snapshot.restore.unblock.cancelled");
            run_log.line("snapshot.restore.unblock.cancelled");
            return Ok(());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                timings.event("snapshot.restore.unblock.accepted");
                run_log.line("snapshot.restore.unblock.accepted");
                stream
                    .set_read_timeout(Some(Duration::from_millis(250)))
                    .context("set restore snapshot stream read timeout")?;
                let mut frame_type = [0u8; 1];
                match stream.read_exact(&mut frame_type) {
                    Ok(()) => match read_u32(&mut stream) {
                        Ok(len) => run_log.line(format!(
                            "snapshot.restore.unblock.frame type={} len={len}",
                            frame_type[0]
                        )),
                        Err(e) => {
                            run_log.line(format!("snapshot.restore.unblock.frame_len_error {e:#}"))
                        }
                    },
                    Err(e)
                        if matches!(
                            e.kind(),
                            ErrorKind::WouldBlock
                                | ErrorKind::TimedOut
                                | ErrorKind::UnexpectedEof
                                | ErrorKind::ConnectionReset
                        ) =>
                    {
                        run_log.line(format!(
                            "snapshot.restore.unblock.frame_unavailable kind={:?}",
                            e.kind()
                        ));
                    }
                    Err(e) => run_log.line(format!("snapshot.restore.unblock.frame_error {e:#}")),
                }
                let mut ready = [0u8; 1];
                match stream.read_exact(&mut ready) {
                    Ok(()) => {
                        run_log.line(format!("snapshot.restore.unblock.ready byte={}", ready[0]))
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            ErrorKind::WouldBlock
                                | ErrorKind::TimedOut
                                | ErrorKind::UnexpectedEof
                                | ErrorKind::ConnectionReset
                        ) =>
                    {
                        run_log.line(format!(
                            "snapshot.restore.unblock.ready_unavailable kind={:?}",
                            e.kind()
                        ));
                    }
                    Err(e) => run_log.line(format!("snapshot.restore.unblock.ready_error {e:#}")),
                }
                let _ = stream.shutdown(Shutdown::Both);
                timings.event("snapshot.restore.unblock.closed");
                run_log.line("snapshot.restore.unblock.closed");
                return Ok(());
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e).context("accept restore snapshot wait connection"),
        }
    }
    timings.event("snapshot.restore.unblock.timeout");
    run_log.line("snapshot.restore.unblock.timeout");
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
    vm_error_rx: &mpsc::Receiver<KrunError>,
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
        stream
            .set_read_timeout(Some(remaining.min(Duration::from_secs(2))))
            .context("set lnx-agent hello read timeout")?;
        match read_message(&mut stream) {
            Ok(Message::Hello { version }) if version == PROTOCOL_VERSION => {
                let _ = stream.set_read_timeout(None);
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
    vm_error_rx: Option<&mpsc::Receiver<KrunError>>,
) -> Result<UnixStream> {
    let start = Instant::now();
    let mut last = None;
    while start.elapsed() < timeout {
        if let Some(rx) = vm_error_rx {
            if let Ok(error) = rx.try_recv() {
                bail!("libkrun start failed: {error}");
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
            if let Ok(error) = rx.try_recv() {
                bail!("libkrun start failed: {error}");
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

/// Runs `command` detached: in its own session, with no input, writing its
/// output to /tmp/lnx-detached-PID.log, and prints its pid. (`setsid` runs
/// in place here, since a background job of a non-interactive shell is not a
/// process group leader, so `$!` is the command's own pid.)
fn detached_argv(command: &[String]) -> Vec<String> {
    let mut argv = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        r#"setsid /bin/sh -c 'exec "$@" >"/tmp/lnx-detached-$$.log" 2>&1' lnx-detached "$@" </dev/null & echo $!"#
            .to_string(),
        "lnx-detach".to_string(),
    ];
    argv.extend(command.iter().cloned());
    argv
}

/// The guest working directory for `--workdir`, relative to `default_cwd`.
fn exec_workdir(default_cwd: &str, workdir: Option<&str>) -> String {
    match workdir {
        Some(workdir) => Path::new(default_cwd)
            .join(workdir)
            .to_string_lossy()
            .into_owned(),
        None => default_cwd.to_string(),
    }
}

fn humantime_seconds(duration: Duration) -> String {
    format!("{}s", duration.as_secs_f64())
}

fn guest_cwd(cwd: &Path) -> String {
    // Host directories are mounted at identical paths inside the guest (see
    // docs/architecture.md), so the guest cwd is always the host cwd verbatim.
    cwd.to_string_lossy().into_owned()
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

fn cwd_write_allowlist() -> Vec<String> {
    vec![".".to_string()]
}

fn preflight_host_share_cwd(layout: &Layout, cwd: &Path, no_host_shares: bool) -> Result<()> {
    if no_host_shares {
        return Ok(());
    }
    let host_home = host_home_for_cwd(cwd)?;
    preflight_host_share_cwd_with_home(layout, cwd, no_host_shares, &host_home)
}

fn preflight_host_share_cwd_with_home(
    layout: &Layout,
    cwd: &Path,
    no_host_shares: bool,
    host_home: &Path,
) -> Result<()> {
    if no_host_shares {
        return Ok(());
    }
    if !cwd.exists() {
        bail!(
            "working directory does not exist on macOS: {}",
            cwd.display()
        );
    }
    let state_root = host_share::state_root(&layout.instance_dir);
    for target in host_share::targets_for_absolute_path(cwd, cwd, Some(host_home)) {
        let path_state = host_share::path_state(&state_root, &target)?;
        if let Some(covering) = path_state.covering_whiteout {
            let hidden_path = target.share_root.join(&covering);
            bail!(
                "working directory is hidden by host-share copy-on-write state before the command can start: {}\nhidden path: {}\ninspect: lnx fs unshare {}\nrestore visibility: lnx fs unshare --remove {}",
                cwd.display(),
                hidden_path.display(),
                cwd.display(),
                hidden_path.display()
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn replace_home_write_allowlist(ctx: &VmHandle, cwd: &Path, host_home: &Path) -> Result<()> {
    ctx.replace_virtiofs_write_allowlist(
        "home",
        home_write_allowlist(cwd, host_home)
            .into_iter()
            .map(PathBuf::from)
            .collect(),
    )?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn replace_home_write_allowlist(_ctx: &VmHandle, _cwd: &Path, _host_home: &Path) -> Result<()> {
    Ok(())
}

/// Waits for the guest to quiesce for its final snapshot, then captures it.
fn serve_snapshot(
    listener: UnixListener,
    capture: &CaptureContext<'_>,
    timings: &TimingLog,
) -> Result<GenerationId> {
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
    timings.event("snapshot.request.read");
    let mut ready_stream;
    let mut ready = [0u8; 1];
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .context("set snapshot ready read timeout")?;
    match stream.read_exact(&mut ready) {
        Ok(()) => {}
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::UnexpectedEof
            ) =>
        {
            ready_stream = accept_unix(&listener, Duration::from_secs(30))
                .context("accept snapshot ready reconnect")?;
            ready_stream
                .set_nonblocking(false)
                .context("set snapshot ready stream blocking")?;
            ready_stream
                .read_exact(&mut ready)
                .context("read snapshot ready reconnect")?;
        }
        Err(e) => return Err(e).context("read snapshot ready"),
    }
    let _ = stream.set_read_timeout(None);
    if ready[0] != b'R' {
        bail!("bad snapshot ready");
    }
    capture.session.guest_quiesced();
    timings.event("snapshot.ready.read");
    timings.event("snapshot.capture.begin");
    let id = capture.capture(Origin::Snapshot {
        run: capture.session.run().id.clone(),
    })?;
    timings.event("snapshot.done");
    Ok(id)
}

#[cfg(target_os = "macos")]
fn clone_or_copy_file(src: &Path, dst: &Path) -> Result<()> {
    crate::sparse_copy::clone_or_copy_file(src, dst)
}

#[cfg(not(target_os = "macos"))]
fn clone_or_copy_file(src: &Path, dst: &Path) -> Result<()> {
    crate::sparse_copy::clone_or_copy_file(src, dst)
}

fn copy_snapshot_stamp(
    snapshot_path: &Path,
    initramfs_stamp: &Path,
    trace_log: Option<&TraceLog>,
    deterministic_clock_state: Option<&DeterministicClockState>,
) -> Result<()> {
    // Compatibility stamps live in the run dir; they travel with the snapshot
    // so a later restore can check agent, share-root, and deterministic mode.
    import_deterministic_timer_jumps(initramfs_stamp, trace_log)?;
    sync_deterministic_clock_event_sequence(initramfs_stamp, trace_log)?;
    ensure_deterministic_clock_state_file(initramfs_stamp, deterministic_clock_state)?;
    let running_agent_stamp = initramfs_stamp.with_file_name(RUNNING_AGENT_STAMP);
    let shares_stamp = initramfs_stamp.with_file_name(LAUNCH_METADATA);
    let deterministic_stamp = initramfs_stamp.with_file_name("deterministic.stamp");
    let deterministic_clock_state_path = initramfs_stamp.with_file_name(DETERMINISTIC_CLOCK_STATE);
    for (stamp, name) in [
        (running_agent_stamp.as_path(), "initramfs.stamp"),
        (shares_stamp.as_path(), LAUNCH_METADATA),
        (deterministic_stamp.as_path(), "deterministic.stamp"),
        (
            deterministic_clock_state_path.as_path(),
            DETERMINISTIC_CLOCK_STATE,
        ),
    ] {
        let target = snapshot_path.join(name);
        if name == DETERMINISTIC_CLOCK_STATE {
            if let Some(state) = deterministic_clock_state {
                match fs::read(stamp) {
                    Ok(bytes) => write_snapshot_metadata_file(&target, &bytes)?,
                    Err(e) if e.kind() == ErrorKind::NotFound => {
                        write_deterministic_clock_state(&target, state)?;
                    }
                    Err(e) => return Err(e).with_context(|| format!("read {}", stamp.display())),
                }
                continue;
            }
            if !stamp.exists() {
                continue;
            }
        }
        copy_snapshot_metadata_file(stamp, &target)?;
    }
    Ok(())
}

fn write_snapshot_metadata_file(dst: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut dst_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(dst)
        .with_context(|| format!("create {}", dst.display()))?;
    dst_file
        .write_all(bytes)
        .with_context(|| format!("write {}", dst.display()))?;
    dst_file
        .sync_all()
        .with_context(|| format!("sync {}", dst.display()))
}

fn copy_snapshot_metadata_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut src_file = fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
    let mut dst_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(dst)
        .with_context(|| format!("create {}", dst.display()))?;
    std::io::copy(&mut src_file, &mut dst_file)
        .with_context(|| format!("copy {} to {}", src.display(), dst.display()))?;
    dst_file
        .sync_all()
        .with_context(|| format!("sync {}", dst.display()))
}

/// Captures VM memory into `dir` together with the run's rootfs and
/// host-share copy-on-write state as they were at the same paused instant.
/// Copying either after the vCPUs resume would pair the memory image with
/// later disk state.
fn capture_vm_state(ctx: &VmHandle, dir: &Path, run: &store::Run) -> Result<()> {
    let rootfs = run.rootfs();
    let share_state = run.host_share_state();
    ctx.snapshot_while_paused(dir, |stage| {
        let copy = || -> Result<()> {
            clone_or_copy_file(&rootfs, &stage.join(store::ROOTFS))?;
            if share_state.exists() {
                crate::sparse_copy::clone_or_copy_tree(
                    &share_state,
                    &stage.join(store::HOST_SHARE_STATE),
                )?;
            }
            Ok(())
        };
        copy().map_err(|error| std::io::Error::other(format!("{error:#}")))
    })?;
    Ok(())
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

mod broker;
mod capture;
mod deterministic;
mod launch_meta;
mod locks;
mod logs;
mod protocol_io;
mod session;
mod snapshots;
mod stop;
pub(crate) use broker::*;
pub(crate) use capture::*;
pub(crate) use deterministic::*;
pub(crate) use launch_meta::*;
pub(crate) use locks::*;
pub(crate) use logs::*;
pub(crate) use protocol_io::*;
pub(crate) use session::*;
pub(crate) use snapshots::*;
pub(crate) use stop::*;

#[cfg(test)]
mod tests;
