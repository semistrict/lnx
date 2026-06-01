use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    net::TcpListener,
    os::fd::AsRawFd,
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use anyhow::{Context, Result, bail};

use crate::{
    initramfs,
    krun::Context as KrunContext,
    paths::Layout,
    protocol::{MAX_MESSAGE_SIZE, Message, PROTOCOL_VERSION},
};

const AGENT_PORT: u32 = 10240;
const SNAPSHOT_PORT: u32 = 10241;
const CONTROL_PORT: u32 = 10242;
const FRAME_SNAPSHOT: u8 = b'K';
const BROKER_HELLO_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub layout: Layout,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub cpus: u8,
    pub memory_mib: u32,
    pub restore_snapshot: Option<PathBuf>,
}

pub fn run(config: RunConfig) -> Result<i32> {
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
        run_existing_broker_client(&broker_socket, &config.command, &config.cwd, Some(&run_log))?
    {
        return Ok(status);
    }

    let bootstrap_lock = match acquire_bootstrap_or_run_client(
        &config.layout.run_dir.join("bootstrap.lock.d"),
        &broker_socket,
        &config.command,
        &config.cwd,
        &run_log,
    )? {
        BootstrapOutcome::Lock(lock) => lock,
        BootstrapOutcome::Status(status) => return Ok(status),
    };
    if let Some(status) =
        run_existing_broker_client(&broker_socket, &config.command, &config.cwd, Some(&run_log))?
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

    let timings = Arc::new(TimingLog::open(
        &config.layout,
        &config.command,
        config.restore_snapshot.as_deref(),
    )?);
    timings.install_for_libkrun();
    timings.event("dirs.ready");

    let (initrd, rebuilt_initramfs) =
        initramfs::write_from_agent(Path::new(env!("LNX_AGENT")), config.layout.run_dir.clone())?;
    timings.event(if rebuilt_initramfs {
        "initramfs.rebuilt"
    } else {
        "initramfs.cached"
    });
    let requested_restore_snapshot = config.restore_snapshot.clone();
    let restore_snapshot = if rebuilt_initramfs && config.restore_snapshot.is_some() {
        timings.event("snapshot.restore.skipped.initramfs_rebuilt");
        run_log.line("snapshot.restore.skipped reason=initramfs_rebuilt");
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
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_file(&snapshot_socket);
    let _ = fs::remove_file(&control_socket);
    let _ = fs::remove_file(&broker_socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("listen on {}", socket.display()))?;
    let snapshot_listener = UnixListener::bind(&snapshot_socket)
        .with_context(|| format!("listen on {}", snapshot_socket.display()))?;
    let control_listener = UnixListener::bind(&control_socket)
        .with_context(|| format!("listen on {}", control_socket.display()))?;
    let broker_listener = UnixListener::bind(&broker_socket)
        .with_context(|| format!("listen on {}", broker_socket.display()))?;
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
    let rootfs = requested_restore_snapshot
        .as_ref()
        .map(|snapshot| snapshot.join("rootfs.ext4"))
        .filter(|path| path.exists())
        .unwrap_or_else(|| config.layout.rootfs.clone());
    log_file_summary(&run_log, "rootfs.selected", &rootfs);
    ctx.add_root_pmem(&rootfs)?;
    ctx.set_kernel(
        &config.layout.kernel,
        Some(&initrd),
        "console=hvc0 reboot=k panic=1 root=/dev/pmem0 rw rootfstype=ext4 rootflags=dax",
    )?;
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

    let owner = run_broker_owner(
        listener,
        config.layout.console_log.clone(),
        Arc::clone(&ctx),
        config.layout.snapshot_dir.join("latest"),
        rootfs,
        snapshot_listener,
        control_listener,
        broker_listener,
        broker_socket.clone(),
        restore_snapshot,
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
                &[&broker_socket, &socket, &snapshot_socket, &control_socket],
            );
            return Err(e);
        }
    };
    let status = match run_broker_client_retry(
        &broker_socket,
        &config.command,
        &config.cwd,
        Duration::from_secs(5),
    )
    .with_context(|| console_hint(&config.layout.console_log))
    {
        Ok(status) => status,
        Err(e) => {
            timings.event(&format!("restore.client.error {e:#}"));
            run_log.line(format!("client.error {e:#}"));
            log_console_tail(&run_log, &config.layout.console_log);
            return Err(e);
        }
    };
    let _ = owner.join();
    let result = Ok(status);
    match &result {
        Ok(status) => {
            timings.event(&format!("run.done status={status}"));
            run_log.line(format!("run.done status={status}"));
        }
        Err(e) => {
            timings.event(&format!("run.error {e:#}"));
            run_log.line(format!("run.error {e:#}"));
        }
    }
    drop(network);
    drop(bootstrap_lock);
    result
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
            Ok(()) => Ok(Some(Self {
                path: path.to_path_buf(),
            })),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(e).with_context(|| format!("create {}", path.display())),
        }
    }
}

impl Drop for BootstrapLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
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
    if version != 1 {
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

    wait_for_path(&socket, Duration::from_secs(5))
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

fn write_message(stream: &mut UnixStream, message: &Message) -> Result<()> {
    let bytes = postcard::to_allocvec(message).context("encode protocol message")?;
    if bytes.len() > MAX_MESSAGE_SIZE as usize {
        bail!("protocol message too large: {}", bytes.len());
    }
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}

fn read_message(stream: &mut UnixStream) -> Result<Message> {
    let len = read_u32(stream)?;
    if len > MAX_MESSAGE_SIZE {
        bail!("protocol message too large: {len}");
    }
    let mut bytes = vec![0u8; len as usize];
    stream.read_exact(&mut bytes)?;
    postcard::from_bytes(&bytes).context("decode protocol message")
}

fn acquire_bootstrap_or_run_client(
    lock_path: &Path,
    socket: &Path,
    command: &[String],
    cwd: &Path,
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
        if let Some(status) = run_existing_broker_client(socket, command, cwd, Some(run_log))? {
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
            run_broker_session(stream, command, cwd).map(Some)
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

fn connect_broker(socket: &Path) -> Result<UnixStream> {
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("connect {}", socket.display()))?;
    write_message(
        &mut stream,
        &Message::Hello {
            version: PROTOCOL_VERSION,
        },
    )?;
    stream
        .set_read_timeout(Some(BROKER_HELLO_TIMEOUT))
        .context("set broker hello timeout")?;
    match read_message(&mut stream) {
        Ok(Message::Hello { version }) if version == PROTOCOL_VERSION => {}
        Ok(other) => bail!("bad broker hello: {other:?}"),
        Err(e) => return Err(e).context("read broker hello"),
    }
    stream
        .set_read_timeout(None)
        .context("clear broker hello timeout")?;
    Ok(stream)
}

fn run_broker_session(mut stream: UnixStream, command: &[String], cwd: &Path) -> Result<i32> {
    let channel_id = new_request_id()?;
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
            cwd: guest_cwd(cwd),
            pty: use_pty,
            term,
            colorterm,
            rows,
            cols,
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
            match read_message(&mut stream)? {
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
        match read_message(&mut stream)? {
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

fn run_broker_client_retry(
    socket: &Path,
    command: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Result<i32> {
    let start = Instant::now();
    let mut last = None;
    while start.elapsed() < timeout {
        match connect_broker(socket) {
            Ok(stream) => return run_broker_session(stream, command, cwd),
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
    restore_snapshot: Option<PathBuf>,
    timings: Arc<TimingLog>,
    run_log: Arc<RunLog>,
    vm_error_rx: mpsc::Receiver<i32>,
) -> Result<thread::JoinHandle<()>> {
    listener
        .set_nonblocking(true)
        .context("set lnx-agent listener nonblocking")?;
    let agent_timeout = if restore_snapshot.is_some() {
        Duration::from_secs(10)
    } else {
        Duration::from_secs(90)
    };
    timings.event("agent.accept.begin");
    run_log.line(format!(
        "agent.accept.begin timeout_ms={} restore={}",
        agent_timeout.as_millis(),
        restore_snapshot
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "false".to_string())
    ));
    let mut agent_stream = match accept_unix_with_progress(
        &listener,
        agent_timeout,
        Some((&timings, "agent.accept.waiting")),
        Some(&vm_error_rx),
    ) {
        Ok(stream) => stream,
        Err(e) => {
            run_log.line(format!("agent.accept.error {e:#}"));
            log_console_tail(&run_log, &console_log);
            return Err(e).with_context(|| console_hint(&console_log));
        }
    };
    timings.event("agent.accepted");
    run_log.line("agent.accepted");
    agent_stream
        .set_nonblocking(false)
        .context("set lnx-agent stream blocking")?;
    match read_message(&mut agent_stream)? {
        Message::Hello { version } if version == PROTOCOL_VERSION => {}
        other => bail!("bad lnx-agent hello: {other:?}"),
    }
    write_message(
        &mut agent_stream,
        &Message::Hello {
            version: PROTOCOL_VERSION,
        },
    )?;

    let (agent_tx, agent_rx) = mpsc::channel::<Message>();
    let client_senders = Arc::new(Mutex::new(HashMap::<u64, mpsc::Sender<Message>>::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let seen_active = Arc::new(AtomicBool::new(false));

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
    thread::spawn(move || {
        while let Ok(message) = read_message(&mut agent_reader) {
            let channel_id = match &message {
                Message::Data { channel_id, .. }
                | Message::Stderr { channel_id, .. }
                | Message::ExitStatus { channel_id, .. }
                | Message::Close { channel_id }
                | Message::Error { channel_id, .. } => Some(*channel_id),
                _ => None,
            };
            if let Some(channel_id) = channel_id {
                let tx = reader_clients
                    .lock()
                    .ok()
                    .and_then(|clients| clients.get(&channel_id).cloned());
                if let Some(tx) = tx {
                    let _ = tx.send(message.clone());
                }
                if matches!(message, Message::Close { .. }) {
                    if let Ok(mut clients) = reader_clients.lock() {
                        clients.remove(&channel_id);
                    }
                    reader_active.fetch_sub(1, Ordering::SeqCst);
                }
            }
        }
        if let Ok(mut clients) = reader_clients.lock() {
            let dropped = clients.len();
            clients.clear();
            if dropped > 0 {
                reader_active.fetch_sub(dropped, Ordering::SeqCst);
            }
        }
    });

    broker_listener
        .set_nonblocking(true)
        .context("set broker listener nonblocking")?;
    let owner_timings = Arc::clone(&timings);
    let owner_log = Arc::clone(&run_log);
    let force_full_snapshot = restore_snapshot.is_none();
    Ok(thread::spawn(move || {
        owner_timings.event("broker.ready");
        owner_log.line(format!("broker.ready socket={}", broker_socket.display()));
        loop {
            match broker_listener.accept() {
                Ok((client, _)) => {
                    owner_log.line("broker.client.accepted");
                    let tx = agent_tx.clone();
                    let clients = Arc::clone(&client_senders);
                    let active = Arc::clone(&active);
                    let seen = Arc::clone(&seen_active);
                    let client_log = Arc::clone(&owner_log);
                    thread::spawn(move || {
                        if let Err(e) = handle_broker_client(client, tx, clients, active, seen) {
                            client_log.line(format!("broker.client.error {e:#}"));
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if seen_active.load(Ordering::SeqCst) && active.load(Ordering::SeqCst) == 0 {
                        break;
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

fn handle_broker_client(
    mut client: UnixStream,
    agent_tx: mpsc::Sender<Message>,
    clients: Arc<Mutex<HashMap<u64, mpsc::Sender<Message>>>>,
    active: Arc<AtomicUsize>,
    seen_active: Arc<AtomicBool>,
) -> Result<()> {
    let mut active_reservation = ActiveReservation::new(Arc::clone(&active));
    seen_active.store(true, Ordering::SeqCst);
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
    let Message::OpenExec { channel_id, .. } = &first else {
        bail!("client did not open an exec channel");
    };
    let channel_id = *channel_id;
    let (to_client_tx, to_client_rx) = mpsc::channel::<Message>();
    clients
        .lock()
        .map_err(|_| anyhow::anyhow!("lock broker clients"))?
        .insert(channel_id, to_client_tx);
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
                agent_tx
                    .send(message)
                    .context("send client message to agent")?;
            }
            Err(_) => {
                let _ = agent_tx.send(Message::Eof { channel_id });
                return Ok(());
            }
        }
    }
}

fn accept_unix(listener: &UnixListener, timeout: Duration) -> Result<UnixStream> {
    accept_unix_with_progress(listener, timeout, None, None)
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

fn new_request_id() -> Result<u64> {
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

    const TIOCGWINSZ: libc::c_ulong = 0x40087468;
    let mut size = Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::ioctl(std::io::stdin().as_raw_fd(), TIOCGWINSZ, &mut size) };
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

fn guest_cwd(cwd: &Path) -> String {
    let cwd = cwd.to_string_lossy();
    if cwd.starts_with("/Users/") {
        "/".to_string()
    } else {
        cwd.into_owned()
    }
}

fn serve_snapshot(
    listener: UnixListener,
    ctx: &KrunContext,
    snapshot_path: &Path,
    rootfs: &Path,
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
        snapshot_with_file_copy_full(ctx, snapshot_path, rootfs)?;
    } else {
        ctx.snapshot_with_file_copy(snapshot_path, rootfs, "rootfs.ext4")?;
    }
    timings.event("snapshot.done");
    Ok(())
}

fn snapshot_with_file_copy_full(
    ctx: &KrunContext,
    snapshot_path: &Path,
    rootfs: &Path,
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
    stream.read_exact(&mut buf)?;
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
