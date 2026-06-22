use std::{
    collections::{BTreeMap, HashMap},
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
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

use crate::{initramfs, krun::Context as KrunContext, paths::Layout};

const AGENT_PORT: u32 = 10240;
const SNAPSHOT_PORT: u32 = 10241;
const CONTROL_PORT: u32 = 10242;
const FRAME_SNAPSHOT: u8 = b'K';
const INTERRUPT_POLL_TIMEOUT: Duration = Duration::from_millis(100);
// Accepted vmstate.bin container version. Source backend lives in the META
// section, not in the header version.
const SNAPSHOT_VMSTATE_VERSION: u32 = 4;

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
const FRESH_OWNER_SLOT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_AGENT_ACCEPT_TIMEOUT: Duration = Duration::from_secs(90);
const ROOTFS_BACKEND_ENV: &str = "LNX_ROOTFS_BACKEND";
const RESTORE_ENTROPY_BYTES: usize = 64;
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
    pub no_host_shares: bool,
    pub deterministic: Option<DeterministicConfig>,
    pub trace_events: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeterministicConfig {
    pub seed: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeterministicClockState {
    realtime_unix_nanos: u64,
    monotonic_nanos: u64,
    counter_frequency_hz: u64,
    event_sequence: u64,
    timer_jump_count: u64,
    last_timer_deadline_ticks: u64,
}

/// Marks an owner start failure that happened while restoring a snapshot's
/// memory, as opposed to an unrelated boot failure. Only these refusals are
/// worth retrying as a cold boot.
#[derive(Debug)]
struct RestoreRefused;

impl std::fmt::Display for RestoreRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "snapshot memory restore refused by the devices")
    }
}

impl std::error::Error for RestoreRefused {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortForward {
    pub listen_host: String,
    pub listen_port: u16,
    pub guest_host: String,
    pub guest_port: u16,
}

pub fn run(config: RunConfig) -> Result<i32> {
    if config.trace_events && config.deterministic.is_none() {
        bail!("trace events require deterministic mode");
    }
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
    let no_daemon_reuse = debug_flag_enabled("nodaemonreuse");
    if no_daemon_reuse {
        run_log.line("debug.nodaemonreuse enabled");
        eprintln!(
            "debug[nodaemonreuse]: daemon reuse disabled; this command will start a fresh VM owner and will not keep it running for reuse."
        );
    }
    if config.forwards.is_empty() && !no_daemon_reuse {
        if broker_socket.exists() {
            validate_runtime_deterministic_compatibility(
                &config.layout,
                config.deterministic.as_ref(),
            )?;
        }
        if let Some(status) = run_existing_broker_client(
            &broker_socket,
            &config.command,
            &config.cwd,
            config.run_as_root,
            config.no_host_shares,
            config.deterministic.as_ref(),
            Some(&run_log),
        )? {
            run_log.line(format!("run.done status={status}"));
            return Ok(status);
        }
    } else {
        wait_for_fresh_owner_slot(&config.layout, &run_log)?;
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

pub fn validate_runtime_deterministic_compatibility(
    layout: &Layout,
    deterministic: Option<&DeterministicConfig>,
) -> Result<()> {
    let current = deterministic_stamp_content(deterministic);
    let stamp_path = layout.run_dir.join("deterministic.stamp");
    match fs::read_to_string(&stamp_path) {
        Ok(stamp) if stamp == current => Ok(()),
        Ok(stamp) => bail!(
            "running VM deterministic stamp is incompatible ({}): {}",
            describe_deterministic_stamp_mismatch(&stamp, &current),
            stamp_path.display()
        ),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                && current == deterministic_stamp_content(None) =>
        {
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "running VM has no deterministic compatibility stamp: {}",
            stamp_path.display()
        ),
        Err(e) => Err(e).with_context(|| format!("read {}", stamp_path.display())),
    }
}

fn wait_for_fresh_owner_slot(layout: &Layout, run_log: &RunLog) -> Result<()> {
    let lock_path = layout.run_dir.join("bootstrap.lock.d");
    let start = Instant::now();
    let mut logged_wait = false;
    while lock_path.exists() {
        if bootstrap_lock_is_stale(&lock_path)? {
            run_log.line(format!(
                "fresh_owner.slot.stale_lock.remove lock={}",
                lock_path.display()
            ));
            let _ = fs::remove_dir_all(&lock_path);
            continue;
        }
        if !logged_wait {
            run_log.line(format!(
                "fresh_owner.slot.wait lock={} timeout_ms={}",
                lock_path.display(),
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

fn acquire_bootstrap_for_forward(lock_path: &Path, run_log: &RunLog) -> Result<BootstrapLock> {
    match BootstrapLock::try_acquire(lock_path)? {
        Some(lock) => {
            run_log.line(format!(
                "bootstrap.lock.acquired path={} forward=true",
                lock_path.display()
            ));
            Ok(lock)
        }
        None => bail!(
            "starting a fresh VM owner requires exclusive ownership, but another owner started for this instance"
        ),
    }
}

pub fn run_owner(config: RunConfig) -> Result<()> {
    if config.trace_events && config.deterministic.is_none() {
        bail!("trace events require deterministic mode");
    }
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
    reset_owner_attempt_logs(&config.layout, &run_log);
    if broker_socket.exists() {
        run_log.line(format!(
            "broker.stale_socket.remove path={}",
            broker_socket.display()
        ));
        let _ = fs::remove_file(&broker_socket);
    }

    let idle = IdlePolicy {
        ttl: owner_idle_ttl(),
        starts_idle: !debug_flag_enabled("nodaemonreuse"),
    };
    let vm = match start_vm(&config, &run_log, &broker_socket, idle) {
        Ok(vm) => vm,
        // Keep restore refusals distinct from unrelated boot failures so
        // the client can report a hard memory-restore failure.
        Err(e) if e.downcast_ref::<RestoreRefused>().is_some() => {
            run_log.line(format!("owner.start.restore_failed error={e:#}"));
            drop(bootstrap_lock);
            std::process::exit(EXIT_RESTORE_FAILED);
        }
        Err(e) => return Err(e),
    };
    let _ = vm.owner.join();
    flush_deterministic_trace_events(&config.layout, vm.trace_log.as_deref())?;
    run_log.line("owner.done");
    drop(vm.network);
    drop(bootstrap_lock);
    Ok(())
}

fn run_foreground(config: RunConfig, run_log: Arc<RunLog>, broker_socket: PathBuf) -> Result<i32> {
    let lock_path = config.layout.run_dir.join("bootstrap.lock.d");
    let bootstrap_lock = if config.forwards.is_empty() {
        match acquire_bootstrap_or_run_client(
            &lock_path,
            &broker_socket,
            &config.command,
            &config.cwd,
            config.run_as_root,
            config.no_host_shares,
            config.deterministic.as_ref(),
            debug_flag_enabled("nodaemonreuse"),
            &run_log,
        )? {
            BootstrapOutcome::Lock(lock) => lock,
            BootstrapOutcome::Status(status) => return Ok(status),
        }
    } else {
        acquire_bootstrap_for_forward(&lock_path, &run_log)?
    };
    if config.forwards.is_empty() && !debug_flag_enabled("nodaemonreuse") {
        if let Some(status) = run_existing_broker_client(
            &broker_socket,
            &config.command,
            &config.cwd,
            config.run_as_root,
            config.no_host_shares,
            config.deterministic.as_ref(),
            Some(&run_log),
        )? {
            drop(bootstrap_lock);
            return Ok(status);
        }
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
        config.no_host_shares,
        config.deterministic.as_ref(),
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
    flush_deterministic_trace_events(&config.layout, vm.trace_log.as_deref())?;
    vm.timings.event(&format!("run.done status={status}"));
    run_log.line(format!("run.done status={status}"));
    drop(vm.network);
    drop(bootstrap_lock);
    Ok(status)
}

fn restore_entropy(config: Option<&DeterministicConfig>) -> Result<Vec<u8>> {
    if let Some(config) = config {
        return Ok(deterministic_restore_entropy(&config.seed));
    }
    fresh_restore_entropy()
}

fn deterministic_restore_entropy(seed: &str) -> Vec<u8> {
    let mut entropy = Vec::with_capacity(RESTORE_ENTROPY_BYTES);
    let mut counter = 0u64;
    while entropy.len() < RESTORE_ENTROPY_BYTES {
        let mut hasher = Sha256::new();
        hasher.update(b"lnx deterministic restore entropy v1\0");
        hasher.update(seed.as_bytes());
        hasher.update(b"\0");
        hasher.update(counter.to_le_bytes());
        entropy.extend_from_slice(&hasher.finalize());
        counter = counter.saturating_add(1);
    }
    entropy.truncate(RESTORE_ENTROPY_BYTES);
    entropy
}

fn fresh_restore_entropy() -> Result<Vec<u8>> {
    let mut entropy = vec![0u8; RESTORE_ENTROPY_BYTES];
    fs::File::open("/dev/urandom")
        .context("open host /dev/urandom for restore entropy")?
        .read_exact(&mut entropy)
        .context("read host restore entropy")?;
    Ok(entropy)
}

struct VmHandles {
    owner: thread::JoinHandle<()>,
    network: NetworkBacking,
    timings: Arc<TimingLog>,
    trace_log: Option<Arc<TraceLog>>,
}

/// How the VM reaches the network: a routable per-VM address on the ingress
/// daemon's vmnet network when available, else a private gvproxy NAT.
enum NetworkBacking {
    Gvproxy(Gvproxy),
    #[cfg(target_os = "macos")]
    Vmnet {
        fd: Option<std::os::fd::OwnedFd>,
        ip: std::net::Ipv4Addr,
        prefix: u8,
        gateway: std::net::Ipv4Addr,
        // Held for the VM's lifetime; closing it tells the ingress daemon to
        // tear down the vmnet interface and free the address.
        _keepalive: std::os::unix::net::UnixStream,
    },
}

impl NetworkBacking {
    /// One line of network backing identity. Part of the snapshot stamp: a
    /// snapshot taken on one backing must not restore on another, while forked
    /// instances on the same vmnet subnet can still resume from the checkpoint.
    fn stamp_line(&self) -> String {
        match self {
            NetworkBacking::Gvproxy(_) => "net=gvproxy".to_string(),
            #[cfg(target_os = "macos")]
            NetworkBacking::Vmnet {
                prefix, gateway, ..
            } => format!("net=vmnet:prefix={prefix}:gateway={gateway}"),
        }
    }

    /// LNX_NET_IP / LNX_NET_GATEWAY values for the guest agent; empty means
    /// the agent uses the gvproxy static configuration.
    fn guest_env(&self) -> (String, String) {
        match self {
            NetworkBacking::Gvproxy(_) => (String::new(), String::new()),
            #[cfg(target_os = "macos")]
            NetworkBacking::Vmnet {
                ip,
                prefix,
                gateway,
                ..
            } => (format!("{ip}/{prefix}"), gateway.to_string()),
        }
    }
}

fn start_network(
    config: &RunConfig,
    run_log: &RunLog,
    timings: &TimingLog,
) -> Result<NetworkBacking> {
    #[cfg(target_os = "macos")]
    if config.deterministic.is_none() {
        let attachment = crate::ingress::load_config()
            .and_then(|ingress| {
                crate::ingress::request_network_attachment(&ingress, &config.layout.instance)
            })
            .unwrap_or_else(|e| {
                run_log.line(format!("network.vmnet.unavailable error={e:#}"));
                None
            });
        if let Some(attachment) = attachment {
            timings.event("network.vmnet.ready");
            run_log.line(format!(
                "network.vmnet ip={}/{} gateway={}",
                attachment.ip, attachment.prefix, attachment.gateway
            ));
            return Ok(NetworkBacking::Vmnet {
                fd: Some(attachment.fd),
                ip: attachment.ip,
                prefix: attachment.prefix,
                gateway: attachment.gateway,
                _keepalive: attachment.keepalive,
            });
        }
    } else {
        timings.event("network.deterministic.gvproxy");
        run_log.line("network.deterministic.gvproxy");
    }
    let gvproxy = start_gvproxy(&config.layout.run_dir)?;
    timings.event("gvproxy.ready");
    run_log.line(format!(
        "gvproxy.ready socket={}",
        config.layout.run_dir.join("gvproxy.sock").display()
    ));
    Ok(NetworkBacking::Gvproxy(gvproxy))
}

/// A stable, locally administered unicast MAC derived from the instance
/// name. All instances share the vmnet network's L2 segment, so each needs
/// its own address, and keeping it stable preserves neighbor caches across
/// reboots and restores.
#[cfg(target_os = "macos")]
fn instance_mac(instance: &str) -> [u8; 6] {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(format!("lnx-mac:{instance}"));
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&digest[..6]);
    mac[0] = (mac[0] & 0xFE) | 0x02;
    mac
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
    let requested_restore_snapshot = config.restore_snapshot.clone();
    let initramfs_stamp = config.layout.run_dir.join("initramfs.stamp");
    let mut network = start_network(&config, &run_log, &timings)?;
    let current_host_home = host_home_for_cwd(&config.cwd)?;
    let current_outside_home_cwd =
        (!config.cwd.starts_with(&current_host_home)).then(|| config.cwd.clone());
    let current_shares_stamp = shares_stamp_content(
        &current_host_home,
        current_outside_home_cwd.as_deref(),
        &network.stamp_line(),
        config.no_host_shares,
    );
    let mut share_layout = ShareLayout {
        host_home: current_host_home,
        outside_home_cwd: current_outside_home_cwd,
        no_host_shares: config.no_host_shares,
    };
    let mut shares_stamp = current_shares_stamp.clone();
    if let Some(snapshot) = &config.restore_snapshot {
        if let Some(snapshot_share_layout) = snapshot_share_layout(snapshot)? {
            if shares_stamps_match_ignoring_cwd(&snapshot_share_layout.stamp, &current_shares_stamp)
            {
                run_log.line(format!(
                    "snapshot.shares.restore_layout path={}",
                    snapshot.join("shares.stamp").display()
                ));
                shares_stamp = snapshot_share_layout.stamp;
                share_layout = snapshot_share_layout.layout;
            }
        }
    }
    let shares_stamp_path = config.layout.run_dir.join("shares.stamp");
    fs::write(&shares_stamp_path, &shares_stamp)
        .with_context(|| format!("write {}", shares_stamp_path.display()))?;
    let deterministic_stamp = deterministic_stamp_content(config.deterministic.as_ref());
    let deterministic_stamp_path = config.layout.run_dir.join("deterministic.stamp");
    fs::write(&deterministic_stamp_path, &deterministic_stamp)
        .with_context(|| format!("write {}", deterministic_stamp_path.display()))?;
    configure_libkrun_deterministic_time(config.deterministic.is_some());
    let deterministic_clock_state = deterministic_clock_state_for_start(
        config.deterministic.as_ref(),
        config.restore_snapshot.as_deref(),
    )?;
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
            trace_bool("restore_snapshot", config.restore_snapshot.is_some()),
            trace_text("network", network.stamp_line()),
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
    let restore_snapshot = if let Some(snapshot) = &config.restore_snapshot {
        if !snapshot_initramfs_is_compatible(snapshot, &initramfs_stamp) {
            run_log.line(format!(
                "snapshot.initramfs_stamp_mismatch ignored snapshot={} current={}",
                snapshot.join("initramfs.stamp").display(),
                initramfs_stamp.display()
            ));
        }
        if let Some(reason) = snapshot_shares_incompatibility(snapshot, &shares_stamp) {
            bail!(
                "snapshot host-share/network stamp is incompatible ({reason}): {}",
                snapshot.join("shares.stamp").display()
            );
        }
        if let Some(reason) = snapshot_deterministic_incompatibility(snapshot, &deterministic_stamp)
        {
            bail!(
                "snapshot deterministic stamp is incompatible ({reason}): {}",
                snapshot.join("deterministic.stamp").display()
            );
        }
        match snapshot_vm_config(snapshot) {
            Ok(Some(snapshot_config))
                if !snapshot_config.matches(config.cpus, config.memory_mib) =>
            {
                bail!(
                    "snapshot VM config mismatch: snapshot_cpus={} configured_cpus={} snapshot_memory_mib={} configured_memory_mib={}",
                    snapshot_config.vcpu_count,
                    config.cpus,
                    snapshot_config.memory_mib(),
                    config.memory_mib
                );
            }
            Ok(_) => Some(snapshot.clone()),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "read snapshot header from {}",
                        snapshot.join("vmstate.bin").display()
                    )
                });
            }
        }
    } else {
        None
    };
    if let Some(snapshot) = &requested_restore_snapshot {
        log_snapshot_summary(&run_log, "snapshot.requested", snapshot);
    }
    configure_snapshot_restore_compat(restore_snapshot.as_deref(), &run_log);

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
    let krun_log_level = std::env::var("LNX_KRUN_LOG_LEVEL")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(2);
    KrunContext::set_log_level(krun_log_level)?;
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
    let rootfs_label = if rootfs != config.layout.rootfs {
        "snapshot rootfs"
    } else {
        "rootfs"
    };
    crate::init::ensure_ext4_has_no_errors(&rootfs, rootfs_label).map_err(|e| {
        run_log.line(format!(
            "rootfs.health.error path={} error={e:#}",
            rootfs.display()
        ));
        e
    })?;
    log_file_summary(&run_log, "rootfs.selected", &rootfs);
    let rootfs_backend = RootfsBackend::from_env(std::env::var(ROOTFS_BACKEND_ENV).ok())?;
    let root_device = match rootfs_backend {
        RootfsBackend::Pmem => {
            ctx.add_root_pmem(&rootfs)?;
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
                .map(|cwd| guest_cwd(cwd, &share_layout.host_home))
                .unwrap_or_default(),
        )
    };
    if share_layout.no_host_shares {
        run_log.line("host_shares.disabled");
    } else {
        ctx.add_host_virtiofs(
            "home",
            &share_layout.host_home,
            &home_write_allowlist(&config.cwd, &share_layout.host_home),
            &host_share_unshare_dir(&config.layout, "home"),
        )?;
        if let Some(cwd) = &share_layout.outside_home_cwd {
            ctx.add_host_virtiofs(
                "cwd",
                cwd,
                &cwd_write_allowlist(),
                &host_share_unshare_dir(&config.layout, "cwd"),
            )?;
        }
    }
    let mut kernel_cmdline =
        format!("console=hvc0 reboot=k panic=1 root={root_device} rw rootfstype=ext4");
    #[cfg(target_arch = "aarch64")]
    kernel_cmdline.push_str(" arm64.nopauth");
    kernel_cmdline.push_str(" rootflags=dax");
    if config.nested_kvm {
        kernel_cmdline.push_str(" kvm.allow_unsafe_mappings=1");
    }
    ctx.set_kernel(&config.layout.kernel, Some(&initrd), &kernel_cmdline)?;
    ctx.add_vsock_connector(AGENT_PORT, &socket)?;
    ctx.add_vsock_connector(SNAPSHOT_PORT, &snapshot_socket)?;
    ctx.add_vsock_connector(CONTROL_PORT, &control_socket)?;
    match &mut network {
        NetworkBacking::Gvproxy(gvproxy) => ctx.add_gvproxy_network(&gvproxy.socket)?,
        #[cfg(target_os = "macos")]
        NetworkBacking::Vmnet { fd, .. } => {
            let fd = fd.take().context("vmnet attachment fd already consumed")?;
            ctx.add_fd_network(fd, instance_mac(&config.layout.instance))?;
        }
    }
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
    let init_unix_secs = match &config.deterministic {
        Some(_) => 0,
        None => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("host clock is before Unix epoch")?
            .as_secs(),
    };
    let (net_ip, net_gateway) = network.guest_env();
    ctx.set_exec(
        "/init",
        &["--init".to_string()],
        &[
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
            "container=lnx".to_string(),
            format!("LNX_HOST_UNIX_SECS={init_unix_secs}"),
            format!("LNX_ROOT_DEVICE={root_device}"),
            format!("LNX_NET_IP={net_ip}"),
            format!("LNX_NET_GATEWAY={net_gateway}"),
            format!(
                "LNX_VIRTIOFS_DAX={}",
                if crate::krun::host_share_dax_enabled() {
                    "1"
                } else {
                    "0"
                }
            ),
            format!("LNX_VIRTIOFS_HOME={guest_home}"),
            share_layout
                .outside_home_cwd
                .as_ref()
                .filter(|_| !share_layout.no_host_shares)
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
    let latest_snapshot = config.layout.snapshot_dir.join("latest");
    let promote_rootfs_after_snapshot = rootfs != config.layout.rootfs
        && snapshot_output == latest_snapshot
        && requested_restore_snapshot.as_deref() == Some(latest_snapshot.as_path());
    let owner = run_broker_owner(
        listener,
        config.layout.clone(),
        config.layout.console_log.clone(),
        Arc::clone(&ctx),
        snapshot_output,
        rootfs,
        config.layout.rootfs.clone(),
        promote_rootfs_after_snapshot,
        snapshot_listener,
        control_listener,
        broker_listener,
        broker_socket.to_path_buf(),
        initramfs_stamp,
        restore_snapshot,
        config.forwards.clone(),
        share_layout.host_home.clone(),
        share_layout.no_host_shares,
        config.deterministic.clone(),
        idle,
        Arc::clone(&timings),
        Arc::clone(&run_log),
        trace_log.clone(),
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
        trace_log,
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

struct TraceLog {
    path: PathBuf,
    state: Mutex<TraceState>,
}

struct SnapshotVmConfig {
    #[cfg_attr(
        any(not(all(target_os = "linux", target_arch = "aarch64")), not(test)),
        allow(dead_code)
    )]
    version: u32,
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

struct TraceState {
    connection: Connection,
    next_sequence: i64,
}

struct TraceField {
    key: &'static str,
    ordinal: Option<i64>,
    value: TraceValue,
}

enum TraceValue {
    Text(String),
    Integer(i64),
    Blob(Vec<u8>),
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

fn drain_broker_channels(
    clients: &Mutex<HashMap<u64, BrokerChannel>>,
    active: &AtomicUsize,
    error_message: Option<String>,
) -> usize {
    let drained = match clients.lock() {
        Ok(mut clients) => clients.drain().collect::<Vec<_>>(),
        Err(_) => return 0,
    };
    let active_owned = drained
        .iter()
        .filter(|(_, channel)| channel.active_owned_by_reader)
        .count();
    if let Some(message) = error_message {
        for (channel_id, channel) in &drained {
            let _ = channel.tx.send(Message::Error {
                channel_id: *channel_id,
                message: message.clone(),
            });
        }
    }
    if active_owned > 0 {
        active.fetch_sub(active_owned, Ordering::SeqCst);
    }
    active_owned
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

impl TraceLog {
    fn open(layout: &Layout) -> Result<Self> {
        let path = layout.run_dir.join("deterministic-trace.sqlite3");
        remove_path_if_exists(&path)?;
        let connection =
            Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        connection
            .execute_batch(
                r#"
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                PRAGMA foreign_keys = ON;

                CREATE TABLE trace_metadata (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                ) STRICT;

                CREATE TABLE events (
                    sequence INTEGER PRIMARY KEY NOT NULL,
                    event TEXT NOT NULL
                ) STRICT;

                CREATE INDEX events_event_idx ON events(event);

                CREATE TABLE event_text_fields (
                    sequence INTEGER NOT NULL REFERENCES events(sequence) ON DELETE CASCADE,
                    key TEXT NOT NULL,
                    ordinal INTEGER,
                    value TEXT NOT NULL
                ) STRICT;

                CREATE TABLE event_integer_fields (
                    sequence INTEGER NOT NULL REFERENCES events(sequence) ON DELETE CASCADE,
                    key TEXT NOT NULL,
                    ordinal INTEGER,
                    value INTEGER NOT NULL
                ) STRICT;

                CREATE TABLE event_blob_fields (
                    sequence INTEGER NOT NULL REFERENCES events(sequence) ON DELETE CASCADE,
                    key TEXT NOT NULL,
                    ordinal INTEGER,
                    value BLOB NOT NULL
                ) STRICT;

                CREATE INDEX event_text_fields_lookup_idx
                    ON event_text_fields(sequence, key, ordinal);
                CREATE INDEX event_integer_fields_lookup_idx
                    ON event_integer_fields(sequence, key, ordinal);
                CREATE INDEX event_blob_fields_lookup_idx
                    ON event_blob_fields(sequence, key, ordinal);
                "#,
            )
            .with_context(|| format!("initialize {}", path.display()))?;
        connection
            .execute(
                "INSERT INTO trace_metadata (key, value) VALUES (?1, ?2)",
                params!["format", "lnx-deterministic-trace-v1"],
            )
            .with_context(|| format!("write trace metadata {}", path.display()))?;
        Ok(Self {
            path,
            state: Mutex::new(TraceState {
                connection,
                next_sequence: 0,
            }),
        })
    }

    fn event(&self, event: &str, fields: Vec<TraceField>) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        let sequence = state.next_sequence;
        if insert_trace_event(&mut state.connection, sequence, event, &fields).is_err() {
            return;
        }
        state.next_sequence = state.next_sequence.saturating_add(1);
    }

    fn set_next_sequence(&self, sequence: u64) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        state.next_sequence = i64::try_from(sequence).unwrap_or(i64::MAX);
    }

    fn next_sequence(&self) -> u64 {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return 0,
        };
        u64::try_from(state.next_sequence).unwrap_or(0)
    }
}

fn insert_trace_event(
    connection: &mut Connection,
    sequence: i64,
    event: &str,
    fields: &[TraceField],
) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute(
        "INSERT INTO events (sequence, event) VALUES (?1, ?2)",
        params![sequence, event],
    )?;
    for field in fields {
        match &field.value {
            TraceValue::Text(value) => {
                transaction.execute(
                    "INSERT INTO event_text_fields (sequence, key, ordinal, value) VALUES (?1, ?2, ?3, ?4)",
                    params![sequence, field.key, field.ordinal, value],
                )?;
            }
            TraceValue::Integer(value) => {
                transaction.execute(
                    "INSERT INTO event_integer_fields (sequence, key, ordinal, value) VALUES (?1, ?2, ?3, ?4)",
                    params![sequence, field.key, field.ordinal, value],
                )?;
            }
            TraceValue::Blob(value) => {
                transaction.execute(
                    "INSERT INTO event_blob_fields (sequence, key, ordinal, value) VALUES (?1, ?2, ?3, ?4)",
                    params![sequence, field.key, field.ordinal, value],
                )?;
            }
        }
    }
    transaction.commit()?;
    Ok(())
}

fn trace_text(key: &'static str, value: impl Into<String>) -> TraceField {
    TraceField {
        key,
        ordinal: None,
        value: TraceValue::Text(value.into()),
    }
}

fn trace_text_ordinal(key: &'static str, ordinal: usize, value: impl Into<String>) -> TraceField {
    TraceField {
        key,
        ordinal: Some(ordinal as i64),
        value: TraceValue::Text(value.into()),
    }
}

fn trace_integer(key: &'static str, value: impl Into<i64>) -> TraceField {
    TraceField {
        key,
        ordinal: None,
        value: TraceValue::Integer(value.into()),
    }
}

fn trace_bool(key: &'static str, value: bool) -> TraceField {
    trace_integer(key, if value { 1 } else { 0 })
}

fn trace_blob(key: &'static str, value: &[u8]) -> TraceField {
    TraceField {
        key,
        ordinal: None,
        value: TraceValue::Blob(value.to_vec()),
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
        version,
        memory_bytes: u64::from_le_bytes(header[16..24].try_into().unwrap()),
        vcpu_count: u32::from_le_bytes(header[32..36].try_into().unwrap()),
    }))
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn configure_snapshot_restore_compat(_restore_snapshot: Option<&Path>, _run_log: &RunLog) {}

#[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
fn configure_snapshot_restore_compat(_restore_snapshot: Option<&Path>, _run_log: &RunLog) {}

fn snapshot_initramfs_is_compatible(snapshot_path: &Path, current_stamp: &Path) -> bool {
    let Some(snapshot_key) = initramfs_stamp_key(&snapshot_path.join("initramfs.stamp")) else {
        return false;
    };
    let Some(current_key) = initramfs_stamp_key(current_stamp) else {
        return false;
    };
    snapshot_key == current_key
}

const HOST_SHARE_CACHE_DAX_STAMP: &str =
    "host-share-cache=dax+keep-cache+writeback+restore-sync-v1";
const HOST_SHARE_CACHE_NODAX_STAMP: &str =
    "host-share-cache=nodax+keep-cache+writeback+restore-sync-v1";
const HOST_SHARES_DISABLED_STAMP: &str = "host-shares=disabled-v1";

#[derive(Clone, Debug, Eq, PartialEq)]
struct ShareLayout {
    host_home: PathBuf,
    outside_home_cwd: Option<PathBuf>,
    no_host_shares: bool,
}

struct SnapshotShareLayout {
    stamp: String,
    layout: ShareLayout,
}

// A restored guest keeps its snapshot-time share mounts, network
// configuration, and kernel-side virtiofs caches. A snapshot is only valid for
// the same host share roots, network backing, and host-share cache policy: a
// drifted root would silently back the old guest mount points with a different
// host directory, a drifted network would strand the guest's addresses, and an
// old cache policy can preserve stale host-file contents or size after the
// host changed while the VM was stopped.
fn shares_stamp_content(
    host_home: &Path,
    outside_home_cwd: Option<&Path>,
    net_stamp_line: &str,
    no_host_shares: bool,
) -> String {
    let host_share_cache_stamp = if crate::krun::host_share_dax_enabled() {
        HOST_SHARE_CACHE_DAX_STAMP
    } else {
        HOST_SHARE_CACHE_NODAX_STAMP
    };
    shares_stamp_content_with_cache_stamp(
        host_home,
        outside_home_cwd,
        net_stamp_line,
        no_host_shares,
        host_share_cache_stamp,
    )
}

fn shares_stamp_content_with_cache_stamp(
    host_home: &Path,
    outside_home_cwd: Option<&Path>,
    net_stamp_line: &str,
    no_host_shares: bool,
    host_share_cache_stamp: &str,
) -> String {
    if no_host_shares {
        return format!("{HOST_SHARES_DISABLED_STAMP}\n{net_stamp_line}\n");
    }
    let mut content = format!("{host_share_cache_stamp}\nhome={}\n", host_home.display());
    if let Some(cwd) = outside_home_cwd {
        content.push_str(&format!("cwd={}\n", cwd.display()));
    }
    content.push_str(net_stamp_line);
    content.push('\n');
    content
}

pub fn snapshot_shares_incompatibility_for_import(
    snapshot_path: &Path,
    cwd: &Path,
    no_host_shares: bool,
) -> Result<Option<String>> {
    #[cfg(target_os = "macos")]
    {
        let _ = (snapshot_path, cwd, no_host_shares);
        // macOS may restore through vmnet when ingress is available, and the
        // vmnet stamp is allocated by the ingress daemon at VM start time.
        // Keep import validation conservative there and let start perform the
        // definitive restore check.
        Ok(None)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let host_home = host_home_for_cwd(cwd)?;
        let outside_home_cwd = (!cwd.starts_with(&host_home)).then(|| cwd.to_path_buf());
        let shares_stamp = shares_stamp_content(
            &host_home,
            outside_home_cwd.as_deref(),
            "net=gvproxy",
            no_host_shares,
        );
        Ok(snapshot_shares_incompatibility(
            snapshot_path,
            &shares_stamp,
        ))
    }
}

fn snapshot_shares_incompatibility(snapshot_path: &Path, current: &str) -> Option<String> {
    match fs::read_to_string(snapshot_path.join("shares.stamp")) {
        Ok(stamp) if stamp == current => None,
        Ok(stamp)
            if !stamp.lines().any(|line| {
                line == HOST_SHARE_CACHE_DAX_STAMP
                    || line == HOST_SHARE_CACHE_NODAX_STAMP
                    || line == HOST_SHARES_DISABLED_STAMP
            }) =>
        {
            Some("host_share_cache_policy: snapshot was created before host-share cache policy was recorded".to_string())
        }
        Ok(stamp) if shares_stamps_match_ignoring_cwd(&stamp, current) => None,
        Ok(stamp) => Some(describe_shares_stamp_mismatch(&stamp, current)),
        // Missing stamps predate the host-share cache-policy stamp. Do not
        // memory-restore them: the guest may hold stale page-cache, inode-size,
        // or dentry state for host-owned files.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(
            "host_share_cache_policy: snapshot has no host-share/network compatibility stamp"
                .to_string(),
        ),
        Err(e) => Some(format!("shares_stamp_unreadable: {e}")),
    }
}

fn describe_shares_stamp_mismatch(snapshot: &str, current: &str) -> String {
    let snapshot_fields = parse_shares_stamp(snapshot);
    let current_fields = parse_shares_stamp(current);
    let mut mismatches = Vec::new();

    let snapshot_disabled = snapshot_fields.contains_key("host-shares");
    let current_disabled = current_fields.contains_key("host-shares");
    if snapshot_disabled != current_disabled {
        mismatches.push(format!(
            "host-shares: snapshot={} current={}",
            if snapshot_disabled {
                "disabled"
            } else {
                "enabled"
            },
            if current_disabled {
                "disabled"
            } else {
                "enabled"
            }
        ));
    }

    for key in ["host-share-cache", "home", "net"] {
        let snapshot_value = snapshot_fields
            .get(key)
            .map(String::as_str)
            .unwrap_or("<absent>");
        let current_value = current_fields
            .get(key)
            .map(String::as_str)
            .unwrap_or("<absent>");
        if snapshot_value != current_value {
            mismatches.push(format!(
                "{key}: snapshot={snapshot_value} current={current_value}"
            ));
        }
    }

    if mismatches.is_empty() {
        "share_mismatch: snapshot and current stamps differ only in unrecognized fields".to_string()
    } else {
        format!("share_mismatch: {}", mismatches.join("; "))
    }
}

fn shares_stamps_match_ignoring_cwd(snapshot: &str, current: &str) -> bool {
    parse_shares_stamp(snapshot)
        .into_iter()
        .filter(|(key, _)| key != "cwd")
        .collect::<BTreeMap<_, _>>()
        == parse_shares_stamp(current)
            .into_iter()
            .filter(|(key, _)| key != "cwd")
            .collect::<BTreeMap<_, _>>()
}

fn snapshot_share_layout(snapshot_path: &Path) -> Result<Option<SnapshotShareLayout>> {
    let path = snapshot_path.join("shares.stamp");
    let stamp = match fs::read_to_string(&path) {
        Ok(stamp) => stamp,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let fields = parse_shares_stamp(&stamp);
    let no_host_shares = fields.contains_key("host-shares");
    let host_home = fields.get("home").map(PathBuf::from).unwrap_or_default();
    let outside_home_cwd = fields.get("cwd").map(PathBuf::from);
    Ok(Some(SnapshotShareLayout {
        stamp,
        layout: ShareLayout {
            host_home,
            outside_home_cwd,
            no_host_shares,
        },
    }))
}

fn parse_shares_stamp(stamp: &str) -> BTreeMap<String, String> {
    stamp
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn deterministic_stamp_content(config: Option<&DeterministicConfig>) -> String {
    match config {
        Some(config) => format!(
            "deterministic=enabled-v1\nseed={}\ninitial_realtime_unix_secs=0\nclock_state=deterministic-clock-state-v1\nrestore_timer_rebase=disabled-v1\nvirtual_counter=kvm-controlled-counter-v1\nkvm_halt_poll=disabled-v1\nrtc=deterministic-zero-v1\ntrng=deterministic-smccc-v1\nvirtio_rng=deterministic-stateless-v1\nvsock_timesync=disabled-v1\nrestore_entropy=sha256-seed-v1\nexec_user=uid1000-gid1000-lnxuser\nexec_env=c-utf8-utc-v1\nexec_tty=none-24x80-xterm-256color-v1\nnetwork=gvproxy-fixed-v1\n",
            config.seed
        ),
        None => "deterministic=disabled-v1\n".to_string(),
    }
}

fn configure_libkrun_deterministic_time(enabled: bool) {
    unsafe {
        if enabled {
            std::env::set_var("KRUN_DETERMINISTIC_TIME", "1");
        } else {
            std::env::remove_var("KRUN_DETERMINISTIC_TIME");
        }
    }
}

fn configure_libkrun_deterministic_clock_state(path: Option<&Path>) {
    unsafe {
        if let Some(path) = path {
            std::env::set_var("KRUN_DETERMINISTIC_CLOCK_STATE", path);
        } else {
            std::env::remove_var("KRUN_DETERMINISTIC_CLOCK_STATE");
        }
    }
}

fn configure_libkrun_deterministic_timer_jumps(path: Option<&Path>) {
    unsafe {
        if let Some(path) = path {
            std::env::set_var("KRUN_DETERMINISTIC_TIMER_JUMPS", path);
        } else {
            std::env::remove_var("KRUN_DETERMINISTIC_TIMER_JUMPS");
        }
    }
}

fn initial_deterministic_clock_state() -> DeterministicClockState {
    DeterministicClockState {
        realtime_unix_nanos: 0,
        monotonic_nanos: 0,
        counter_frequency_hz: 1_000_000_000,
        event_sequence: 0,
        timer_jump_count: 0,
        last_timer_deadline_ticks: 0,
    }
}

fn deterministic_clock_state_content(state: &DeterministicClockState) -> String {
    format!(
        "clock_state=deterministic-clock-state-v1\nrealtime_unix_nanos={}\nmonotonic_nanos={}\ncounter_frequency_hz={}\nevent_sequence={}\ntimer_jump_count={}\nlast_timer_deadline_ticks={}\n",
        state.realtime_unix_nanos,
        state.monotonic_nanos,
        state.counter_frequency_hz,
        state.event_sequence,
        state.timer_jump_count,
        state.last_timer_deadline_ticks
    )
}

fn parse_deterministic_clock_state(raw: &str) -> Result<DeterministicClockState> {
    let fields = parse_shares_stamp(raw);
    match fields.get("clock_state").map(String::as_str) {
        Some("deterministic-clock-state-v1") => {}
        Some(other) => bail!("unsupported deterministic clock state {other}"),
        None => bail!("missing deterministic clock state version"),
    }
    Ok(DeterministicClockState {
        realtime_unix_nanos: parse_clock_state_u64(&fields, "realtime_unix_nanos")?,
        monotonic_nanos: parse_clock_state_u64(&fields, "monotonic_nanos")?,
        counter_frequency_hz: parse_clock_state_u64(&fields, "counter_frequency_hz")?,
        event_sequence: parse_clock_state_u64(&fields, "event_sequence")?,
        timer_jump_count: parse_clock_state_u64(&fields, "timer_jump_count")?,
        last_timer_deadline_ticks: parse_clock_state_u64(&fields, "last_timer_deadline_ticks")?,
    })
}

fn parse_clock_state_u64(fields: &BTreeMap<String, String>, key: &str) -> Result<u64> {
    let value = fields
        .get(key)
        .with_context(|| format!("missing deterministic clock state field {key}"))?;
    value
        .parse()
        .with_context(|| format!("parse deterministic clock state field {key}={value}"))
}

fn deterministic_clock_state_for_start(
    deterministic: Option<&DeterministicConfig>,
    restore_snapshot: Option<&Path>,
) -> Result<Option<DeterministicClockState>> {
    if deterministic.is_none() {
        return Ok(None);
    }
    let Some(snapshot) = restore_snapshot else {
        return Ok(Some(initial_deterministic_clock_state()));
    };
    read_deterministic_clock_state(snapshot)
        .with_context(|| {
            format!(
                "read {}",
                snapshot.join(DETERMINISTIC_CLOCK_STATE).display()
            )
        })
        .map(Some)
}

fn read_deterministic_clock_state(snapshot: &Path) -> Result<DeterministicClockState> {
    let path = snapshot.join(DETERMINISTIC_CLOCK_STATE);
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    parse_deterministic_clock_state(&raw)
}

fn write_deterministic_clock_state(path: &Path, state: &DeterministicClockState) -> Result<()> {
    fs::write(path, deterministic_clock_state_content(state))
        .with_context(|| format!("write {}", path.display()))
}

fn flush_deterministic_trace_events(layout: &Layout, trace_log: Option<&TraceLog>) -> Result<()> {
    let initramfs_stamp = layout.run_dir.join("initramfs.stamp");
    import_deterministic_timer_jumps(&initramfs_stamp, trace_log)?;
    sync_deterministic_clock_event_sequence(&initramfs_stamp, trace_log)
}

fn sync_deterministic_clock_event_sequence(
    initramfs_stamp: &Path,
    trace_log: Option<&TraceLog>,
) -> Result<()> {
    let Some(trace_log) = trace_log else {
        return Ok(());
    };
    let path = initramfs_stamp.with_file_name(DETERMINISTIC_CLOCK_STATE);
    if !path.exists() {
        return Ok(());
    }
    let mut state = parse_deterministic_clock_state(
        &fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?,
    )?;
    state.event_sequence = trace_log.next_sequence();
    write_deterministic_clock_state(&path, &state)
}

fn import_deterministic_timer_jumps(
    initramfs_stamp: &Path,
    trace_log: Option<&TraceLog>,
) -> Result<()> {
    let Some(trace_log) = trace_log else {
        return Ok(());
    };
    let jumps_path = initramfs_stamp.with_file_name(DETERMINISTIC_TIMER_JUMPS);
    if !jumps_path.exists() {
        return Ok(());
    }
    let cursor_path = initramfs_stamp.with_file_name(DETERMINISTIC_TIMER_JUMPS_CURSOR);
    let cursor = fs::read_to_string(&cursor_path)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let raw = fs::read_to_string(&jumps_path)
        .with_context(|| format!("read {}", jumps_path.display()))?;
    let start = cursor.min(raw.len());
    let mut consumed = start;
    for line in raw[start..].lines() {
        consumed = consumed.saturating_add(line.len() + 1);
        let fields = parse_timer_jump_line(line);
        let Some(deadline_ticks) = fields.get("deadline_ticks").copied() else {
            continue;
        };
        let Some(counter_frequency_hz) = fields.get("counter_frequency_hz").copied() else {
            continue;
        };
        let Some(deadline_nanos) = fields.get("deadline_nanos").copied() else {
            continue;
        };
        trace_log.event(
            "timer_jump",
            vec![
                trace_integer("deadline_ticks", deadline_ticks as i64),
                trace_integer("counter_frequency_hz", counter_frequency_hz as i64),
                trace_integer("deadline_nanos", deadline_nanos as i64),
            ],
        );
    }
    fs::write(&cursor_path, consumed.to_string())
        .with_context(|| format!("write {}", cursor_path.display()))
}

fn parse_timer_jump_line(line: &str) -> BTreeMap<String, u64> {
    line.split_whitespace()
        .filter_map(|part| {
            let (key, value) = part.split_once('=')?;
            Some((key.to_string(), value.parse().ok()?))
        })
        .collect()
}

fn snapshot_deterministic_incompatibility(snapshot_path: &Path, current: &str) -> Option<String> {
    match fs::read_to_string(snapshot_path.join("deterministic.stamp")) {
        Ok(stamp) if stamp == current => None,
        Ok(stamp) => Some(describe_deterministic_stamp_mismatch(&stamp, current)),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                && current == deterministic_stamp_content(None) =>
        {
            None
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Some("snapshot has no deterministic compatibility stamp".to_string())
        }
        Err(e) => Some(format!("deterministic_stamp_unreadable: {e}")),
    }
}

fn describe_deterministic_stamp_mismatch(snapshot: &str, current: &str) -> String {
    let snapshot_fields = parse_shares_stamp(snapshot);
    let current_fields = parse_shares_stamp(current);
    let mut mismatches = Vec::new();
    for key in [
        "deterministic",
        "seed",
        "initial_realtime_unix_secs",
        "clock_state",
        "restore_timer_rebase",
        "virtual_counter",
        "kvm_halt_poll",
        "rtc",
        "trng",
        "virtio_rng",
        "vsock_timesync",
        "restore_entropy",
        "exec_user",
        "exec_env",
        "exec_tty",
        "network",
    ] {
        let snapshot_value = snapshot_fields
            .get(key)
            .map(String::as_str)
            .unwrap_or("<absent>");
        let current_value = current_fields
            .get(key)
            .map(String::as_str)
            .unwrap_or("<absent>");
        if snapshot_value != current_value {
            mismatches.push(format!(
                "{key}: snapshot={snapshot_value} current={current_value}"
            ));
        }
    }
    if mismatches.is_empty() {
        "snapshot and current deterministic stamps differ only in unrecognized fields".to_string()
    } else {
        mismatches.join("; ")
    }
}

fn initramfs_stamp_key(path: &Path) -> Option<String> {
    let stamp = fs::read_to_string(path).ok()?;
    for line in stamp.lines() {
        if let Some(value) = line.strip_prefix("source=") {
            return Some(format!("source={value}"));
        }
    }
    for line in stamp.lines() {
        if let Some(value) = line.strip_prefix("sha256=") {
            return Some(format!("sha256={value}"));
        }
    }
    None
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
    let gvproxy = resolve_gvproxy_path();
    if !gvproxy.exists() {
        bail!(
            "gvproxy not found at {}. Install gvproxy or set GVPROXY_PATH.",
            gvproxy.display()
        );
    }

    let socket = run_dir.join("gvproxy.sock");
    let api_socket = run_dir.join("gvproxy-api.sock");
    let log = run_dir.join("gvproxy.log");
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_file(&api_socket);
    let log_file = fs::File::create(&log).with_context(|| format!("create {}", log.display()))?;
    let ssh_port = unused_local_port().context("find unused localhost port for gvproxy ssh")?;
    let child = Command::new(&gvproxy)
        .arg("--listen")
        .arg(format!("unix://{}", api_socket.display()))
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

fn resolve_gvproxy_path() -> PathBuf {
    if let Some(path) = std::env::var_os("GVPROXY_PATH") {
        return PathBuf::from(path);
    }
    if let Some(path) = find_on_path("gvproxy") {
        return path;
    }
    PathBuf::from("/opt/homebrew/opt/podman/libexec/podman/gvproxy")
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
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
    no_host_shares: bool,
    deterministic: Option<&DeterministicConfig>,
    no_daemon_reuse: bool,
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
        if !no_daemon_reuse {
            if let Some(status) = run_existing_broker_client(
                socket,
                command,
                cwd,
                run_as_root,
                no_host_shares,
                deterministic,
                Some(run_log),
            )? {
                return Ok(BootstrapOutcome::Status(status));
            }
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
    no_host_shares: bool,
    deterministic: Option<&DeterministicConfig>,
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
            run_broker_session(
                stream,
                command,
                cwd,
                run_as_root,
                no_host_shares,
                deterministic,
            )
            .map(Some)
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
    no_host_shares: bool,
    deterministic: Option<&DeterministicConfig>,
) -> Result<i32> {
    INTERRUPTED.store(false, Ordering::SeqCst);
    let host_home = host_home_for_cwd(cwd)?;
    let guest_cwd = if no_host_shares {
        "/".to_string()
    } else {
        guest_cwd(cwd, &host_home)
    };
    let use_pty = if deterministic.is_some() {
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
    let (uid, gid, group) = exec_identity(run_as_root, deterministic);
    let env = exec_env(deterministic);
    let channel_id = match deterministic {
        Some(config) => deterministic_exec_request_id(
            &config.seed,
            command,
            &guest_cwd,
            run_as_root,
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
    no_host_shares: bool,
    deterministic: Option<&DeterministicConfig>,
    timeout: Duration,
) -> Result<i32> {
    let start = Instant::now();
    let mut last = None;
    while start.elapsed() < timeout {
        match connect_broker(socket) {
            Ok(stream) => {
                return run_broker_session(
                    stream,
                    command,
                    cwd,
                    run_as_root,
                    no_host_shares,
                    deterministic,
                );
            }
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
    layout: Layout,
    console_log: PathBuf,
    ctx: Arc<KrunContext>,
    snapshot_path: PathBuf,
    rootfs: PathBuf,
    canonical_rootfs: PathBuf,
    promote_rootfs_after_snapshot: bool,
    snapshot_listener: UnixListener,
    _control_listener: UnixListener,
    broker_listener: UnixListener,
    broker_socket: PathBuf,
    initramfs_stamp: PathBuf,
    restore_snapshot: Option<PathBuf>,
    forwards: Vec<PortForward>,
    host_home: PathBuf,
    no_host_shares: bool,
    deterministic: Option<DeterministicConfig>,
    idle: IdlePolicy,
    timings: Arc<TimingLog>,
    run_log: Arc<RunLog>,
    trace_log: Option<Arc<TraceLog>>,
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
    let restore_snapshot_unblocker = if restore_snapshot.is_some() {
        Some(spawn_restore_snapshot_unblocker(
            &snapshot_listener,
            agent_timeout,
            Arc::clone(&timings),
            Arc::clone(&run_log),
        )?)
    } else {
        None
    };
    if restore_snapshot.is_some() {
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
            // refused the memory image; tag it so the client retries cold.
            if restore_snapshot.is_some() {
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

    if restore_snapshot.is_some() {
        let channel_id = match deterministic.as_ref() {
            Some(config) => deterministic_restore_sync_request_id(&config.seed),
            None => new_request_id()?,
        };
        timings.event("snapshot.restore.sync.begin");
        run_log.line(format!(
            "snapshot.restore.sync.begin channel_id={channel_id:016x}"
        ));
        agent_stream
            .set_read_timeout(Some(DEFAULT_AGENT_ACCEPT_TIMEOUT))
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
    let (checkpoint_tx, checkpoint_rx) = mpsc::channel::<CheckpointRequest>();
    let (snapshot_exit_tx, snapshot_exit_rx) = mpsc::channel::<u64>();
    let client_senders = Arc::new(Mutex::new(HashMap::<u64, BrokerChannel>::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let seen_active = Arc::new(AtomicBool::new(idle.starts_idle));
    let agent_failed_before_snapshot = Arc::new(AtomicBool::new(false));
    let snapshot_started = Arc::new(AtomicBool::new(false));

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
    let reader_agent_failed_before_snapshot = Arc::clone(&agent_failed_before_snapshot);
    let reader_snapshot_started = Arc::clone(&snapshot_started);
    let reader_log = Arc::clone(&run_log);
    let reader_trace = trace_log.clone();
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
                if let Some(trace) = &reader_trace {
                    trace.event(
                        "guest_snapshot_exit",
                        vec![trace_text("channel_id", format!("{channel_id:016x}"))],
                    );
                }
                let _ = reader_snapshot_exit_tx.send(channel_id);
                continue;
            }
            if let Some(channel_id) = channel_id {
                if let Some(trace) = &reader_trace {
                    trace_agent_message(trace, &message);
                }
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
        let snapshot_started = reader_snapshot_started.load(Ordering::SeqCst);
        let error_message = if snapshot_started {
            None
        } else {
            reader_agent_failed_before_snapshot.store(true, Ordering::SeqCst);
            Some(format!(
                "guest agent disconnected before command completed: {reader_err:#}"
            ))
        };
        let dropped = drain_broker_channels(&reader_clients, &reader_active, error_message.clone());
        reader_log.line(format!(
            "broker.agent.reader_eof dropped_channels={dropped} snapshot_started={snapshot_started} error={reader_err:#}"
        ));
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
                    let client_trace = trace_log.clone();
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
                            no_host_shares,
                            Arc::clone(&client_log),
                            client_trace,
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
                        if let Some(trace) = &trace_log {
                            trace.event(
                                "checkpoint_request",
                                vec![trace_text("path", request.path.display().to_string())],
                            );
                        }
                        let result = (|| -> Result<()> {
                            seed_incremental_snapshot(
                                &request.path,
                                restore_snapshot.as_deref(),
                                &snapshot_path,
                                &owner_log,
                            )?;
                            owner_log.line(format!(
                                "checkpoint.capture.begin path={}",
                                request.path.display()
                            ));
                            ctx.snapshot_with_file_copy(&request.path, &rootfs, "rootfs.ext4")?;
                            validate_snapshot_rootfs(&request.path)?;
                            owner_log.line(format!(
                                "checkpoint.capture.done path={}",
                                request.path.display()
                            ));
                            copy_snapshot_stamp(
                                &request.path,
                                &initramfs_stamp,
                                trace_log.as_deref(),
                            )?;
                            copy_host_share_state_to_snapshot(&layout, &request.path)?;
                            owner_log.line(format!(
                                "checkpoint.stamp.done path={}",
                                request.path.display()
                            ));
                            Ok(())
                        })()
                        .map_err(|e| format!("{e:#}"));
                        if result.is_ok() {
                            owner_log
                                .line(format!("checkpoint.done path={}", request.path.display()));
                            if let Some(trace) = &trace_log {
                                trace.event(
                                    "checkpoint_done",
                                    vec![trace_text("path", request.path.display().to_string())],
                                );
                            }
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
                            trace_log.as_deref(),
                        )
                        .and_then(|()| copy_host_share_state_to_snapshot(&layout, &snapshot_path))
                        .and_then(|()| {
                            if promote_rootfs_after_snapshot {
                                promote_snapshot_rootfs(
                                    &snapshot_path,
                                    &canonical_rootfs,
                                    &owner_timings,
                                    &owner_log,
                                )
                            } else {
                                Ok(())
                            }
                        });
                        match result {
                            Ok(()) => {
                                owner_log.line(format!(
                                    "snapshot_exit.done channel_id={channel_id} path={}",
                                    snapshot_path.display()
                                ));
                                if let Some(trace) = &trace_log {
                                    trace.event(
                                        "snapshot_exit_done",
                                        vec![
                                            trace_text("channel_id", format!("{channel_id:016x}")),
                                            trace_text("path", snapshot_path.display().to_string()),
                                        ],
                                    );
                                }
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
                    if agent_failed_before_snapshot.load(Ordering::SeqCst) {
                        owner_timings.event("snapshot.skipped.agent_failed");
                        owner_log.line(
                            "snapshot.skipped reason=guest_agent_disconnected_before_snapshot",
                        );
                        let _ = fs::remove_file(&broker_socket);
                        drop(broker_listener);
                        return;
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
        if agent_failed_before_snapshot.load(Ordering::SeqCst) {
            owner_timings.event("snapshot.skipped.agent_failed");
            owner_log.line("snapshot.skipped reason=guest_agent_disconnected_before_snapshot");
            return;
        }
        snapshot_started.store(true, Ordering::SeqCst);
        owner_timings.event("snapshot.request.guest");
        owner_log.line(format!(
            "snapshot.request.guest path={} full={force_full_snapshot}",
            snapshot_path.display()
        ));
        if let Some(trace) = &trace_log {
            trace.event(
                "snapshot_request_guest",
                vec![
                    trace_text("path", snapshot_path.display().to_string()),
                    trace_bool("full", force_full_snapshot),
                ],
            );
        }
        let _ = agent_tx.send(Message::SnapshotReady);
        match serve_snapshot(
            snapshot_listener,
            &ctx,
            &snapshot_path,
            &rootfs,
            &initramfs_stamp,
            &layout,
            trace_log.as_deref(),
            force_full_snapshot,
            promote_rootfs_after_snapshot.then_some(canonical_rootfs.as_path()),
            &owner_timings,
            &owner_log,
        ) {
            Ok(()) => {
                owner_log.line("snapshot.done");
                if let Some(trace) = &trace_log {
                    trace.event(
                        "snapshot_done",
                        vec![trace_text("path", snapshot_path.display().to_string())],
                    );
                }
                log_snapshot_summary(&owner_log, "snapshot.latest", &snapshot_path);
            }
            Err(e) => owner_log.line(format!("snapshot.error {e:#}")),
        }
    }))
}

fn maybe_spawn_restore_proof_snapshotter(ctx: Arc<KrunContext>, run_log: Arc<RunLog>) {
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
    while Instant::now() < deadline {
        if INTERRUPTED.load(Ordering::SeqCst) {
            return Ok(130);
        }
        match connect_broker(socket) {
            Ok(stream) => {
                return run_broker_session(
                    stream,
                    command,
                    cwd,
                    config.run_as_root,
                    config.no_host_shares,
                    config.deterministic.as_ref(),
                );
            }
            Err(e) => last = Some(e),
        }
        if let Some(status) = owner.try_wait().context("check lnx _vm-owner")? {
            // An owner that exits zero lost the bootstrap race to another
            // owner, so keep retrying until that one's broker comes up.
            if status.success() {
                run_log.line("owner.exited.early status=0 retry=spawn");
                *owner = spawn_owner_process(config, run_log)?;
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
    no_host_shares: bool,
    run_log: Arc<RunLog>,
    trace_log: Option<Arc<TraceLog>>,
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
        if let Some(trace) = &trace_log {
            trace.event(
                "client_checkpoint_request",
                vec![
                    trace_text("channel_id", format!("{channel_id:016x}")),
                    trace_text("path", path.as_str()),
                ],
            );
        }
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
    if let Some(trace) = &trace_log {
        trace_client_open(trace, &first);
    }
    if !no_host_shares {
        if let Message::OpenExec { cwd, .. } = &first {
            set_home_write_allowlist(ctx.as_ref(), Path::new(cwd), &host_home)?;
        }
    }
    seen_active.store(true, Ordering::SeqCst);
    let (to_client_tx, to_client_rx) = mpsc::channel::<Message>();
    {
        let mut clients = clients
            .lock()
            .map_err(|_| anyhow::anyhow!("lock broker clients"))?;
        if clients.contains_key(&channel_id) {
            let message = format!(
                "channel id collision for live channel {channel_id:016x}; deterministic mode cannot run identical commands concurrently"
            );
            run_log.line(format!("broker.client.channel_collision {message}"));
            write_message(
                &mut client,
                &Message::Error {
                    channel_id,
                    message,
                },
            )?;
            return Ok(());
        }
        clients.insert(
            channel_id,
            BrokerChannel {
                tx: to_client_tx,
                active_owned_by_reader: true,
            },
        );
    }
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
                if let Some(trace) = &trace_log {
                    trace_client_message(trace, &message);
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

fn deterministic_exec_request_id(
    seed: &str,
    command: &[String],
    guest_cwd: &str,
    run_as_root: bool,
    pty: bool,
    rows: u16,
    cols: u16,
) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"lnx deterministic exec request id v1\0");
    hasher.update(seed.as_bytes());
    hasher.update(b"\0cwd\0");
    hasher.update(guest_cwd.as_bytes());
    hasher.update(b"\0root\0");
    hasher.update([u8::from(run_as_root)]);
    hasher.update(b"\0pty\0");
    hasher.update([u8::from(pty)]);
    hasher.update(b"\0rows\0");
    hasher.update(rows.to_le_bytes());
    hasher.update(b"\0cols\0");
    hasher.update(cols.to_le_bytes());
    for arg in command {
        hasher.update(b"\0arg\0");
        hasher.update(arg.as_bytes());
    }
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let id = u64::from_le_bytes(bytes);
    if id == 0 { 1 } else { id }
}

fn deterministic_restore_sync_request_id(seed: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"lnx deterministic restore-sync request id v1\0");
    hasher.update(seed.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let id = u64::from_le_bytes(bytes);
    if id == 0 { 1 } else { id }
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

fn cwd_write_allowlist() -> Vec<String> {
    vec![".".to_string()]
}

fn host_share_unshare_dir(layout: &Layout, tag: &str) -> PathBuf {
    host_share_state_root(layout).join(tag)
}

fn host_share_state_root(layout: &Layout) -> PathBuf {
    layout.instance_dir.join("host-share-state")
}

#[cfg(target_os = "macos")]
fn set_home_write_allowlist(ctx: &KrunContext, cwd: &Path, host_home: &Path) -> Result<()> {
    ctx.set_host_virtiofs_write_allowlist("home", &home_write_allowlist(cwd, host_home))
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
    layout: &Layout,
    trace_log: Option<&TraceLog>,
    force_full: bool,
    promote_rootfs_to: Option<&Path>,
    timings: &TimingLog,
    run_log: &RunLog,
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
    timings.event("snapshot.ready.read");
    timings.event("snapshot.capture.begin");
    if force_full {
        snapshot_with_file_copy_full(ctx, snapshot_path, rootfs, initramfs_stamp, trace_log)?;
    } else {
        ctx.snapshot_with_file_copy(snapshot_path, rootfs, "rootfs.ext4")?;
        if let Err(e) = validate_snapshot_rootfs(snapshot_path) {
            quarantine_snapshot(snapshot_path, run_log, "rootfs_ext4_errors");
            return Err(e);
        }
        copy_snapshot_stamp(snapshot_path, initramfs_stamp, trace_log)?;
    }
    copy_host_share_state_to_snapshot(layout, snapshot_path)?;
    if let Some(canonical_rootfs) = promote_rootfs_to {
        promote_snapshot_rootfs(snapshot_path, canonical_rootfs, timings, run_log)?;
    }
    timings.event("snapshot.done");
    Ok(())
}

fn promote_snapshot_rootfs(
    snapshot_path: &Path,
    canonical_rootfs: &Path,
    timings: &TimingLog,
    run_log: &RunLog,
) -> Result<()> {
    let snapshot_rootfs = snapshot_path.join("rootfs.ext4");
    if snapshot_rootfs == canonical_rootfs {
        return Ok(());
    }
    if !snapshot_rootfs.exists() {
        bail!(
            "snapshot rootfs is missing after snapshot capture: {}",
            snapshot_rootfs.display()
        );
    }
    crate::init::ensure_ext4_has_no_errors(&snapshot_rootfs, "snapshot rootfs")?;
    let parent = canonical_rootfs
        .parent()
        .context("canonical rootfs path has no parent")?;
    let file_name = canonical_rootfs
        .file_name()
        .and_then(|name| name.to_str())
        .context("canonical rootfs path has no file name")?;
    let temp = parent.join(format!(".{file_name}.promote"));
    timings.event("snapshot.rootfs.promote.begin");
    run_log.line(format!(
        "snapshot.rootfs.promote source={} dest={}",
        snapshot_rootfs.display(),
        canonical_rootfs.display()
    ));
    remove_path_if_exists(&temp)?;
    clone_or_copy_file(&snapshot_rootfs, &temp)?;
    fs::rename(&temp, canonical_rootfs).with_context(|| {
        format!(
            "rename {} to {}",
            temp.display(),
            canonical_rootfs.display()
        )
    })?;
    timings.event("snapshot.rootfs.promote.done");
    Ok(())
}

fn snapshot_with_file_copy_full(
    ctx: &KrunContext,
    snapshot_path: &Path,
    rootfs: &Path,
    initramfs_stamp: &Path,
    trace_log: Option<&TraceLog>,
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
    if let Err(e) = validate_snapshot_rootfs(&temp) {
        let _ = remove_path_if_exists(&temp);
        return Err(e);
    }
    remove_path_if_exists(snapshot_path)?;
    fs::rename(&temp, snapshot_path)
        .with_context(|| format!("rename {} to {}", temp.display(), snapshot_path.display()))?;
    copy_snapshot_stamp(snapshot_path, initramfs_stamp, trace_log)?;
    Ok(())
}

fn validate_snapshot_rootfs(snapshot_path: &Path) -> Result<()> {
    crate::init::ensure_ext4_has_no_errors(&snapshot_path.join("rootfs.ext4"), "snapshot rootfs")
}

fn quarantine_snapshot(snapshot_path: &Path, run_log: &RunLog, reason: &str) {
    if !snapshot_path.exists() {
        return;
    }
    let Some(parent) = snapshot_path.parent() else {
        return;
    };
    let Some(name) = snapshot_path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let quarantine = parent.join(format!("{name}.bad.{stamp}"));
    match fs::rename(snapshot_path, &quarantine) {
        Ok(()) => run_log.line(format!(
            "snapshot.quarantine path={} dest={} reason={reason}",
            snapshot_path.display(),
            quarantine.display()
        )),
        Err(e) => run_log.line(format!(
            "snapshot.quarantine.error path={} dest={} reason={reason} error={e}",
            snapshot_path.display(),
            quarantine.display()
        )),
    }
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
        "deterministic.stamp",
        DETERMINISTIC_CLOCK_STATE,
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
) -> Result<()> {
    // Compatibility stamps live in the run dir; they travel with the snapshot
    // so a later restore can check agent, share-root, and deterministic mode.
    import_deterministic_timer_jumps(initramfs_stamp, trace_log)?;
    sync_deterministic_clock_event_sequence(initramfs_stamp, trace_log)?;
    let shares_stamp = initramfs_stamp.with_file_name("shares.stamp");
    let deterministic_stamp = initramfs_stamp.with_file_name("deterministic.stamp");
    let deterministic_clock_state = initramfs_stamp.with_file_name(DETERMINISTIC_CLOCK_STATE);
    for stamp in [
        initramfs_stamp,
        shares_stamp.as_path(),
        deterministic_stamp.as_path(),
        deterministic_clock_state.as_path(),
    ] {
        if !stamp.exists()
            && stamp
                .file_name()
                .is_some_and(|name| name == DETERMINISTIC_CLOCK_STATE)
        {
            continue;
        }
        let name = stamp.file_name().context("stamp file name")?;
        let target = snapshot_path.join(name);
        fs::copy(stamp, &target)
            .with_context(|| format!("copy {} to {}", stamp.display(), target.display()))?;
    }
    Ok(())
}

fn copy_host_share_state_to_snapshot(layout: &Layout, snapshot_path: &Path) -> Result<()> {
    let source = host_share_state_root(layout);
    if !source.exists() {
        return Ok(());
    }
    let target = snapshot_path.join("host-share-state");
    remove_path_if_exists(&target)?;
    clone_or_copy_tree(&source, &target)
}

fn clone_or_copy_tree(source: &Path, target: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(source).with_context(|| format!("stat {}", source.display()))?;
    if metadata.is_dir() {
        fs::create_dir_all(target).with_context(|| format!("create {}", target.display()))?;
        for entry in fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
            let entry = entry.with_context(|| format!("read {}", source.display()))?;
            clone_or_copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if metadata.file_type().is_symlink() {
        let link =
            fs::read_link(source).with_context(|| format!("readlink {}", source.display()))?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        std::os::unix::fs::symlink(&link, target)
            .with_context(|| format!("symlink {} to {}", link.display(), target.display()))?;
        return Ok(());
    }
    clone_or_copy_file(source, target)
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

#[cfg(test)]
mod tests;
