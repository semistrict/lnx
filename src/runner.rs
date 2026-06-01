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
const FRAME_OUTPUT: u8 = b'O';
const FRAME_STDERR: u8 = b'E';
const FRAME_INPUT: u8 = b'I';
const FRAME_STATUS: u8 = b'S';
const FRAME_SNAPSHOT: u8 = b'K';
const FRAME_CONTROL_SNAPSHOT_EXIT: u8 = b'X';
const FRAME_CONTROL_OK: u8 = b'x';
const REQUEST_FLAG_PTY: u8 = 1;
const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;
const MID_COMMAND_META: &str = "lnx-mid-command";

#[derive(Debug)]
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
    let broker_socket = config.layout.run_dir.join("broker.sock");
    if let Ok(status) = run_broker_client(&broker_socket, &config.command, &config.cwd) {
        return Ok(status);
    }

    let bootstrap_lock = BootstrapLock::acquire(config.layout.run_dir.join("bootstrap.lock.d"))?;
    if let Ok(status) = run_broker_client(&broker_socket, &config.command, &config.cwd) {
        drop(bootstrap_lock);
        return Ok(status);
    }
    if broker_socket.exists() {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if UnixStream::connect(&broker_socket).is_ok() {
                drop(bootstrap_lock);
                return run_broker_client_retry(
                    &broker_socket,
                    &config.command,
                    &config.cwd,
                    Duration::from_secs(2),
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
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
        None
    } else {
        config.restore_snapshot.clone()
    };

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
    let network = start_gvproxy(&config.layout.run_dir)?;
    timings.event("gvproxy.ready");

    KrunContext::set_log_level(2)?;
    let ctx = Arc::new(KrunContext::create()?);
    ctx.set_console_output(&config.layout.console_log)?;
    ctx.set_vm_config(config.cpus, config.memory_mib)?;
    let rootfs = requested_restore_snapshot
        .as_ref()
        .map(|snapshot| snapshot.join("rootfs.ext4"))
        .filter(|path| path.exists())
        .unwrap_or_else(|| config.layout.rootfs.clone());
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
    thread::spawn(move || {
        vm_timings.event("krun.start_enter.begin");
        let rc = vm_ctx.start_enter();
        vm_timings.event(&format!("krun.start_enter.return rc={rc}"));
        if rc < 0 {
            eprintln!(
                "krun_start_enter failed: {}{}",
                std::io::Error::from_raw_os_error(-rc),
                console_hint(&console_log)
            );
            std::process::exit(1);
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
    );
    drop(bootstrap_lock);
    let status = run_broker_client_retry(
        &broker_socket,
        &config.command,
        &config.cwd,
        Duration::from_secs(5),
    )
    .with_context(|| console_hint(&config.layout.console_log))?;
    let result = match owner {
        Ok(handle) => {
            let _ = handle.join();
            Ok(status)
        }
        Err(e) => Err(e),
    };
    match &result {
        Ok(status) => timings.event(&format!("run.done status={status}")),
        Err(e) => timings.event(&format!("run.error {e:#}")),
    }
    drop(network);
    result
}

struct TimingLog {
    path: PathBuf,
    state_path: PathBuf,
    base_unix_nanos: u128,
    state: Mutex<TimingState>,
}

struct TimingState {
    file: fs::File,
    state_file: fs::File,
}

struct BootstrapLock {
    path: PathBuf,
}

impl BootstrapLock {
    fn acquire(path: PathBuf) -> Result<Self> {
        let start = Instant::now();
        loop {
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if start.elapsed() > Duration::from_secs(120) {
                        bail!("timed out waiting for {}", path.display());
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
            }
        }
    }
}

impl Drop for BootstrapLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
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

fn run_broker_client(socket: &Path, command: &[String], cwd: &Path) -> Result<i32> {
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("connect {}", socket.display()))?;
    let channel_id = new_request_id()?;
    write_message(
        &mut stream,
        &Message::Hello {
            version: PROTOCOL_VERSION,
        },
    )?;
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
        let stdin_fd = std::io::stdin().as_raw_fd();
        let flags = unsafe { libc::fcntl(stdin_fd, libc::F_GETFL) };
        if flags >= 0 {
            unsafe {
                libc::fcntl(stdin_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
        let mut input = [0u8; 8192];
        loop {
            match std::io::stdin().read(&mut input) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&input[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).context("read stdin"),
            }
        }
        if flags >= 0 {
            unsafe {
                libc::fcntl(stdin_fd, libc::F_SETFL, flags);
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

    let stdin_handle = std::io::stdin();
    let stdin_fd = stdin_handle.as_raw_fd();
    let stream_fd = stream.as_raw_fd();
    let mut stdin = stdin_handle.lock();
    let mut stdin_open = true;
    let mut input = [0u8; 8192];
    loop {
        let mut fds = [
            libc::pollfd {
                fd: stream_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if stdin_open { stdin_fd } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e).context("poll broker and terminal");
        }
        if stdin_open && (fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0 {
            match stdin.read(&mut input) {
                Ok(0) => {
                    stdin_open = false;
                    write_message(&mut stream, &Message::Eof { channel_id })?;
                }
                Ok(n) => {
                    write_message(
                        &mut stream,
                        &Message::Data {
                            channel_id,
                            bytes: input[..n].to_vec(),
                        },
                    )?;
                    if (fds[1].revents & (libc::POLLHUP | libc::POLLERR)) != 0 {
                        stdin_open = false;
                        write_message(&mut stream, &Message::Eof { channel_id })?;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e).context("read stdin"),
            }
        }
        if (fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) == 0 {
            continue;
        }
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
        match run_broker_client(socket, command, cwd) {
            Ok(status) => return Ok(status),
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
    let mut agent_stream = accept_unix_with_progress(
        &listener,
        agent_timeout,
        Some((&timings, "agent.accept.waiting")),
    )
    .with_context(|| console_hint(&console_log))?;
    timings.event("agent.accepted");
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
    });

    broker_listener
        .set_nonblocking(true)
        .context("set broker listener nonblocking")?;
    let owner_timings = Arc::clone(&timings);
    let force_full_snapshot = restore_snapshot.is_none();
    Ok(thread::spawn(move || {
        owner_timings.event("broker.ready");
        loop {
            match broker_listener.accept() {
                Ok((client, _)) => {
                    let tx = agent_tx.clone();
                    let clients = Arc::clone(&client_senders);
                    let active = Arc::clone(&active);
                    let seen = Arc::clone(&seen_active);
                    thread::spawn(move || {
                        let _ = handle_broker_client(client, tx, clients, active, seen);
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
        owner_timings.event("snapshot.request.guest");
        let _ = agent_tx.send(Message::SnapshotReady);
        let _ = serve_snapshot(
            snapshot_listener,
            &ctx,
            &snapshot_path,
            &rootfs,
            force_full_snapshot,
            &owner_timings,
        );
        let _ = fs::remove_file(&broker_socket);
    }))
}

fn handle_broker_client(
    mut client: UnixStream,
    agent_tx: mpsc::Sender<Message>,
    clients: Arc<Mutex<HashMap<u64, mpsc::Sender<Message>>>>,
    active: Arc<AtomicUsize>,
    seen_active: Arc<AtomicBool>,
) -> Result<()> {
    match read_message(&mut client)? {
        Message::Hello { version } if version == PROTOCOL_VERSION => {}
        other => bail!("bad client hello: {other:?}"),
    }
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
    active.fetch_add(1, Ordering::SeqCst);
    seen_active.store(true, Ordering::SeqCst);
    agent_tx.send(first).context("send open exec to agent")?;
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

fn run_exec_client(
    listener: UnixListener,
    command: Vec<String>,
    cwd: PathBuf,
    console_log: PathBuf,
    ctx: Arc<KrunContext>,
    snapshot_path: PathBuf,
    rootfs: PathBuf,
    snapshot_listener: UnixListener,
    control_listener: UnixListener,
    restore_snapshot: Option<PathBuf>,
    timings: Arc<TimingLog>,
) -> Result<i32> {
    listener
        .set_nonblocking(true)
        .context("set lnx-agent listener nonblocking")?;
    let agent_timeout = if restore_snapshot.is_some() {
        Duration::from_secs(10)
    } else {
        Duration::from_secs(90)
    };
    timings.event("agent.accept.begin");
    let mut stream = accept_unix_with_progress(
        &listener,
        agent_timeout,
        Some((&timings, "agent.accept.waiting")),
    )
    .with_context(|| console_hint(&console_log))?;
    timings.event("agent.accepted");
    drop(listener);
    stream
        .set_nonblocking(false)
        .context("set lnx-agent stream blocking")?;
    let restored_request_id = restore_snapshot
        .as_deref()
        .and_then(read_mid_command_request_id)
        .transpose()?;
    let request_id = match restored_request_id {
        Some(request_id) => request_id,
        None => new_request_id()?,
    };
    let force_full_snapshot = restored_request_id.is_some() || restore_snapshot.is_none();
    if std::env::var_os("LNX_DEBUG_FRAMES").is_some() {
        eprintln!("request id: {request_id:#x}");
    }
    timings.event(&format!("request.ready id={request_id:#x}"));
    let use_pty = should_request_pty();
    let raw_mode = if use_pty { RawTerminal::enter() } else { None };
    timings.event(&format!(
        "tty.mode pty={use_pty} raw={}",
        raw_mode.is_some()
    ));
    let (control_tx, control_rx) = mpsc::channel();
    spawn_control_server(
        control_listener,
        Arc::clone(&ctx),
        snapshot_path.clone(),
        rootfs.clone(),
        stream
            .try_clone()
            .context("clone agent stream for control")?,
        control_tx,
        force_full_snapshot,
        Arc::clone(&timings),
    );
    timings.event("control.thread.spawned");
    if restored_request_id.is_none() {
        write_exec_request(&mut stream, request_id, &command, &cwd, use_pty)?;
        timings.event("exec.request.sent");
    } else {
        timings.event("exec.request.skipped.mid_command_restore");
    }
    timings.event("frames.read.begin");
    let frame_result = read_frames(&mut stream, request_id, &timings);
    drop(raw_mode);
    timings.event("tty.restored");
    let status = match frame_result {
        Ok(status) => {
            timings.event(&format!("guest.status status={status}"));
            clear_mid_command_meta(&snapshot_path)?;
            serve_snapshot(
                snapshot_listener,
                &ctx,
                &snapshot_path,
                &rootfs,
                force_full_snapshot,
                &timings,
            )
            .with_context(|| console_hint(&console_log))?;
            status
        }
        Err(e) => match control_rx.recv_timeout(Duration::from_secs(300)) {
            Ok(status) => {
                timings.event(&format!("control.done status={status}"));
                status
            }
            Err(_) => return Err(e).with_context(|| console_hint(&console_log)),
        },
    };
    Ok(status)
}

fn accept_unix(listener: &UnixListener, timeout: Duration) -> Result<UnixStream> {
    accept_unix_with_progress(listener, timeout, None)
}

fn accept_unix_with_progress(
    listener: &UnixListener,
    timeout: Duration,
    progress: Option<(&TimingLog, &str)>,
) -> Result<UnixStream> {
    let start = Instant::now();
    let mut last = None;
    while start.elapsed() < timeout {
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

fn write_exec_request(
    stream: &mut UnixStream,
    request_id: u64,
    command: &[String],
    cwd: &Path,
    use_pty: bool,
) -> Result<()> {
    stream.write_all(&request_id.to_be_bytes())?;
    let cwd = guest_cwd(cwd);
    write_u32(stream, cwd.len().try_into()?)?;
    stream.write_all(cwd.as_bytes())?;
    stream.write_all(&[if use_pty { REQUEST_FLAG_PTY } else { 0 }])?;
    if use_pty {
        let term = std::env::var("TERM")
            .ok()
            .filter(|value| !value.is_empty() && value != "dumb")
            .unwrap_or_else(|| "xterm-256color".to_string());
        write_string(stream, &term)?;
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        write_string(stream, &colorterm)?;
        let (rows, cols) = terminal_size();
        write_u16(stream, rows)?;
        write_u16(stream, cols)?;
    }
    write_u32(stream, command.len().try_into()?)?;
    for arg in command {
        let bytes = arg.as_bytes();
        write_u32(stream, bytes.len().try_into()?)?;
        stream.write_all(bytes)?;
    }
    Ok(())
}

fn should_request_pty() -> bool {
    is_tty(std::io::stdin().as_raw_fd()) && is_tty(std::io::stdout().as_raw_fd())
}

fn is_tty(fd: i32) -> bool {
    (unsafe { libc::isatty(fd) }) == 1
}

fn write_string(stream: &mut UnixStream, value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    write_u32(stream, bytes.len().try_into()?)?;
    stream.write_all(bytes)?;
    Ok(())
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

fn read_frames(stream: &mut UnixStream, request_id: u64, timings: &TimingLog) -> Result<i32> {
    let stdin_handle = std::io::stdin();
    let stdin_fd = stdin_handle.as_raw_fd();
    let stream_fd = stream.as_raw_fd();
    let mut stdin = stdin_handle.lock();
    let mut stdin_open = true;
    let mut input = [0u8; 8192];
    let mut saw_output = false;

    loop {
        let mut fds = [
            libc::pollfd {
                fd: stream_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if stdin_open { stdin_fd } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e).context("poll terminal and lnx-agent");
        }

        if stdin_open && (fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0 {
            match stdin.read(&mut input) {
                Ok(0) => {
                    stdin_open = false;
                    write_input_frame(stream, request_id, &[])?;
                    timings.event("stdin.eof.sent");
                }
                Ok(n) => write_input_frame(stream, request_id, &input[..n])?,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e).context("read stdin"),
            }
        }

        if (fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) == 0 {
            continue;
        }

        let mut frame_type = [0u8; 1];
        stream
            .read_exact(&mut frame_type)
            .context("agent closed before exit status")?;
        let len = read_u32(stream).context("read frame length")?;
        if len > MAX_FRAME_SIZE {
            bail!("agent frame too large: {len}");
        }
        match frame_type[0] {
            FRAME_OUTPUT | FRAME_STDERR => {
                if len < 8 {
                    bail!("bad output frame length: {len}");
                }
                let is_stderr = frame_type[0] == FRAME_STDERR;
                let mut frame_request_id = [0u8; 8];
                stream.read_exact(&mut frame_request_id)?;
                let frame_request_id = u64::from_be_bytes(frame_request_id);
                let mut remaining = len as usize - 8;
                let mut buf = [0u8; 8192];
                while remaining > 0 {
                    let n = remaining.min(buf.len());
                    stream.read_exact(&mut buf[..n])?;
                    if frame_request_id == request_id {
                        if !saw_output {
                            saw_output = true;
                            timings.event("guest.output.first");
                        }
                        if is_stderr {
                            std::io::stderr().write_all(&buf[..n])?;
                        } else {
                            std::io::stdout().write_all(&buf[..n])?;
                        }
                    }
                    remaining -= n;
                }
                if frame_request_id == request_id {
                    if is_stderr {
                        std::io::stderr().flush()?;
                    } else {
                        std::io::stdout().flush()?;
                    }
                }
            }
            FRAME_STATUS => {
                if len != 12 {
                    bail!("bad status frame length: {len}");
                }
                let mut frame_request_id = [0u8; 8];
                stream.read_exact(&mut frame_request_id)?;
                let frame_request_id = u64::from_be_bytes(frame_request_id);
                let mut status = [0u8; 4];
                stream.read_exact(&mut status)?;
                if frame_request_id == request_id {
                    return Ok(i32::from_be_bytes(status));
                }
                if std::env::var_os("LNX_DEBUG_FRAMES").is_some() {
                    eprintln!(
                        "ignored stale status frame: got {frame_request_id:#x}, want {request_id:#x}"
                    );
                }
            }
            other => bail!("unknown agent frame type: {other}"),
        }
    }
}

fn write_input_frame(stream: &mut UnixStream, request_id: u64, payload: &[u8]) -> Result<()> {
    let len = u32::try_from(payload.len() + 8)?;
    stream.write_all(&[FRAME_INPUT])?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&request_id.to_be_bytes())?;
    stream.write_all(payload)?;
    Ok(())
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

fn spawn_control_server(
    listener: UnixListener,
    ctx: Arc<KrunContext>,
    snapshot_path: PathBuf,
    rootfs: PathBuf,
    exec_stream: UnixStream,
    done: mpsc::Sender<i32>,
    force_full: bool,
    timings: Arc<TimingLog>,
) {
    thread::spawn(move || {
        timings.event("control.accept.begin");
        let result = handle_control_request(listener, &ctx, &snapshot_path, &rootfs, force_full);
        match &result {
            Ok(()) => timings.event("control.snapshot.done"),
            Err(e) => timings.event(&format!("control.error {e:#}")),
        }
        let status = if result.is_ok() { 0 } else { 1 };
        let _ = done.send(status);
        let _ = exec_stream.shutdown(std::net::Shutdown::Both);
    });
}

fn handle_control_request(
    listener: UnixListener,
    ctx: &KrunContext,
    snapshot_path: &Path,
    rootfs: &Path,
    force_full: bool,
) -> Result<()> {
    let mut stream = accept_unix(&listener, Duration::from_secs(u64::MAX / 2))?;
    stream
        .set_nonblocking(false)
        .context("set control stream blocking")?;
    let mut frame_type = [0u8; 1];
    stream.read_exact(&mut frame_type)?;
    let len = read_u32(&mut stream)?;
    if frame_type[0] != FRAME_CONTROL_SNAPSHOT_EXIT || len != 8 {
        bail!("bad control request");
    }
    let mut request_id = [0u8; 8];
    stream.read_exact(&mut request_id)?;
    let request_id = u64::from_be_bytes(request_id);
    if force_full {
        snapshot_with_file_copy_full(ctx, snapshot_path, rootfs)?;
    } else {
        ctx.snapshot_with_file_copy(snapshot_path, rootfs, "rootfs.ext4")?;
    }
    write_mid_command_meta(snapshot_path, request_id)?;
    let _ = stream.write_all(&[FRAME_CONTROL_OK]);
    let _ = stream.write_all(&0u32.to_be_bytes());
    Ok(())
}

fn mid_command_meta_path(snapshot_path: &Path) -> PathBuf {
    snapshot_path.join(MID_COMMAND_META)
}

fn write_mid_command_meta(snapshot_path: &Path, request_id: u64) -> Result<()> {
    fs::create_dir_all(snapshot_path)
        .with_context(|| format!("create {}", snapshot_path.display()))?;
    fs::write(
        mid_command_meta_path(snapshot_path),
        format!("{request_id:016x}\n"),
    )
    .with_context(|| format!("write {}", mid_command_meta_path(snapshot_path).display()))
}

fn read_mid_command_request_id(snapshot_path: &Path) -> Option<Result<u64>> {
    let path = mid_command_meta_path(snapshot_path);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(Err(e).with_context(|| format!("read {}", path.display()))),
    };
    Some(u64::from_str_radix(raw.trim(), 16).with_context(|| format!("parse {}", path.display())))
}

fn clear_mid_command_meta(snapshot_path: &Path) -> Result<()> {
    match fs::remove_file(mid_command_meta_path(snapshot_path)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e)
            .with_context(|| format!("remove {}", mid_command_meta_path(snapshot_path).display())),
    }
}

fn write_u32(stream: &mut UnixStream, value: u32) -> Result<()> {
    stream.write_all(&value.to_be_bytes()).map_err(Into::into)
}

fn write_u16(stream: &mut UnixStream, value: u16) -> Result<()> {
    stream.write_all(&value.to_be_bytes()).map_err(Into::into)
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
