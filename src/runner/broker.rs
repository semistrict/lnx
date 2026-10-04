//! The VM owner's broker: client connections, forwarded ports, and the
//! bookkeeping that decides when the VM is idle and when it must stop taking
//! new work before its final snapshot.

use super::*;

pub(crate) const OWNER_STOPPING: &str = "VM owner is stopping after a final snapshot";
/// Sent instead of opening a channel when the owner is already stopping:
/// the command never started, so the client may run it again.
pub(crate) const OWNER_STOPPING_NOT_STARTED: &str =
    "VM owner is stopping after a final snapshot; the command was not started";

/// How long a connected client may take to say hello and send its request.
/// A client that stalls longer cannot keep the owner from going idle.
const CLIENT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a forwarded TCP connection keeps the VM awake after it closes,
/// so a client reconnecting to a forwarded port (a browser reloading a page)
/// still finds the VM running.
const FORWARD_LINGER: Duration = Duration::from_secs(60);
/// How long a new owner waits for the client that started it to connect
/// before it may idle out.
const FIRST_CLIENT_GRACE: Duration = Duration::from_secs(10);

/// An open channel between a client and the guest agent.
#[derive(Clone)]
pub(crate) struct BrokerChannel {
    pub(crate) tx: mpsc::Sender<Message>,
    /// Whether this channel holds one unit of `BrokerState::active`, released
    /// when the agent closes the channel. Forwarded connections instead hold
    /// an [`ActivityGuard`] for their whole lifetime.
    pub(crate) counts_as_active: bool,
}

/// What happened to a request to open a channel.
pub(crate) enum ChannelAdmission {
    Opened,
    Stopping,
    Collision,
}

/// Whether anything keeps the VM from being idle right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IdleStatus {
    /// Channels are open or a forwarded connection is lingering: the idle
    /// timer restarts.
    pub(crate) busy: bool,
    /// Connections are mid-handshake or waiting on a checkpoint: the owner
    /// must not stop yet, but they do not restart the idle timer, so status
    /// probes cannot keep a VM awake.
    pub(crate) pending: bool,
    /// Whether any channel has ever been opened (or the owner started idle).
    pub(crate) seen_active: bool,
}

/// State shared by every thread of a running VM owner.
pub(crate) struct BrokerState {
    channels: Mutex<HashMap<u64, BrokerChannel>>,
    /// Only set while holding `channels`. Everything that sends to the agent
    /// checks it under that lock, so once it is set no new work reaches the
    /// guest: this is the barrier before the final snapshot.
    stopping: AtomicBool,
    active: AtomicUsize,
    pending: AtomicUsize,
    seen_active: AtomicBool,
    awake_until: Mutex<Option<Instant>>,
    /// Until the first channel opens (or this passes), the owner may not
    /// stop: the client that started it is still on its way. Status probes
    /// and other connections that open nothing do not count.
    first_client_deadline: Mutex<Option<Instant>>,
    auto_forward_ports: Mutex<HashSet<(String, u16)>>,
    agent_tx: mpsc::Sender<Message>,
    /// Runs under the channel lock before any channel's opening message
    /// reaches the guest; the owner records there that the run now holds
    /// state a client relies on.
    before_dispatch: Box<dyn Fn() -> Result<()> + Send + Sync>,
    run_log: Arc<RunLog>,
}

impl BrokerState {
    pub(crate) fn new(
        agent_tx: mpsc::Sender<Message>,
        starts_idle: bool,
        before_dispatch: impl Fn() -> Result<()> + Send + Sync + 'static,
        run_log: Arc<RunLog>,
    ) -> Arc<Self> {
        Arc::new(Self {
            channels: Mutex::new(HashMap::new()),
            stopping: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            pending: AtomicUsize::new(0),
            seen_active: AtomicBool::new(starts_idle),
            awake_until: Mutex::new(None),
            first_client_deadline: Mutex::new(Some(Instant::now() + FIRST_CLIENT_GRACE)),
            auto_forward_ports: Mutex::new(HashSet::new()),
            agent_tx,
            before_dispatch: Box::new(before_dispatch),
            run_log,
        })
    }

    pub(crate) fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    fn lock_channels(&self) -> Result<std::sync::MutexGuard<'_, HashMap<u64, BrokerChannel>>> {
        self.channels
            .lock()
            .map_err(|_| anyhow!("broker channel table lock poisoned"))
    }

    /// Runs `action` unless the owner is stopping, atomically with respect
    /// to the stopping barrier.
    pub(crate) fn unless_stopping<T>(&self, action: impl FnOnce() -> T) -> Result<Option<T>> {
        let _channels = self.lock_channels()?;
        if self.is_stopping() {
            return Ok(None);
        }
        Ok(Some(action()))
    }

    /// Sends `message` to the guest agent unless the owner is stopping.
    /// Returns whether it was sent; an error means the agent writer is gone.
    pub(crate) fn send_to_agent(&self, message: Message) -> Result<bool> {
        match self.unless_stopping(|| self.agent_tx.send(message).is_ok())? {
            Some(true) => Ok(true),
            Some(false) => bail!("guest agent writer has stopped"),
            None => Ok(false),
        }
    }

    /// Sends `message` to the guest agent if `channel_id` is still open and
    /// the owner is not stopping.
    pub(crate) fn send_to_agent_on(&self, channel_id: u64, message: Message) -> bool {
        let Ok(channels) = self.lock_channels() else {
            return false;
        };
        !self.is_stopping()
            && channels.contains_key(&channel_id)
            && self.agent_tx.send(message).is_ok()
    }

    /// Registers a channel and sends its opening message to the agent, as
    /// one step with respect to the stopping barrier. `prepare` runs first,
    /// under the same lock, once admission is certain.
    pub(crate) fn open_channel(
        &self,
        channel_id: u64,
        channel: BrokerChannel,
        open: Message,
        prepare: impl FnOnce() -> Result<()>,
    ) -> Result<ChannelAdmission> {
        let mut channels = self.lock_channels()?;
        if self.is_stopping() {
            return Ok(ChannelAdmission::Stopping);
        }
        let Entry::Vacant(entry) = channels.entry(channel_id) else {
            return Ok(ChannelAdmission::Collision);
        };
        prepare()?;
        (self.before_dispatch)()?;
        let counts_as_active = channel.counts_as_active;
        if counts_as_active {
            self.active.fetch_add(1, Ordering::SeqCst);
        }
        self.seen_active.store(true, Ordering::SeqCst);
        if let Ok(mut deadline) = self.first_client_deadline.lock() {
            *deadline = None;
        }
        entry.insert(channel);
        if let Err(error) = self.agent_tx.send(open) {
            channels.remove(&channel_id);
            if counts_as_active {
                self.active.fetch_sub(1, Ordering::SeqCst);
            }
            return Err(error).context("send channel open to agent");
        }
        Ok(ChannelAdmission::Opened)
    }

    /// Forgets a channel, releasing its share of activity.
    pub(crate) fn close_channel(&self, channel_id: u64) {
        let removed = self
            .lock_channels()
            .ok()
            .and_then(|mut channels| channels.remove(&channel_id));
        if removed.is_some_and(|channel| channel.counts_as_active) {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Hands an agent message to the client that owns its channel.
    pub(crate) fn deliver_to_client(&self, channel_id: u64, message: Message) {
        let channel = self
            .lock_channels()
            .ok()
            .and_then(|channels| channels.get(&channel_id).cloned());
        let closes = matches!(message, Message::Close { .. });
        if let Some(channel) = channel {
            let _ = channel.tx.send(message);
        }
        if closes {
            self.close_channel(channel_id);
        }
    }

    /// Fails every open channel with `error` (when given) and forgets them.
    /// Returns how many active channels were released.
    pub(crate) fn drain(&self, error: Option<&str>) -> usize {
        match self.channels.lock() {
            Ok(mut channels) => self.drain_locked(&mut channels, error),
            Err(_) => 0,
        }
    }

    /// Closes the stopping barrier, then fails and forgets every open
    /// channel, and asks the guest to end what they were running, so the
    /// final snapshot does not freeze commands nobody is waiting for any
    /// more (O7). The closes reach the agent before the snapshot request,
    /// which goes through the same queue. Returns how many active channels
    /// were released.
    pub(crate) fn begin_shutdown(&self, error: &str) -> usize {
        match self.channels.lock() {
            Ok(mut channels) => {
                self.stopping.store(true, Ordering::SeqCst);
                for &channel_id in channels.keys() {
                    let _ = self.agent_tx.send(Message::Close { channel_id });
                }
                self.drain_locked(&mut channels, Some(error))
            }
            Err(_) => {
                self.stopping.store(true, Ordering::SeqCst);
                0
            }
        }
    }

    fn drain_locked(
        &self,
        channels: &mut HashMap<u64, BrokerChannel>,
        error: Option<&str>,
    ) -> usize {
        let drained: Vec<_> = channels.drain().collect();
        let released = drained
            .iter()
            .filter(|(_, channel)| channel.counts_as_active)
            .count();
        if let Some(error) = error {
            for (channel_id, channel) in &drained {
                let _ = channel.tx.send(Message::Error {
                    channel_id: *channel_id,
                    message: error.to_string(),
                });
            }
        }
        if released > 0 {
            self.active.fetch_sub(released, Ordering::SeqCst);
        }
        released
    }

    /// Keeps the VM busy for as long as the guard lives.
    pub(crate) fn hold_awake(self: &Arc<Self>) -> ActivityGuard {
        self.active.fetch_add(1, Ordering::SeqCst);
        self.seen_active.store(true, Ordering::SeqCst);
        ActivityGuard {
            state: Arc::clone(self),
        }
    }

    /// Keeps the owner from stopping, without restarting its idle timer, for
    /// as long as the guard lives.
    pub(crate) fn pending_connection(self: &Arc<Self>) -> PendingConnection {
        self.pending.fetch_add(1, Ordering::SeqCst);
        PendingConnection {
            state: Arc::clone(self),
        }
    }

    pub(crate) fn keep_awake_for(&self, duration: Duration) {
        if let Ok(mut awake_until) = self.awake_until.lock() {
            let until = Instant::now() + duration;
            *awake_until = Some(awake_until.map_or(until, |current| current.max(until)));
        }
    }

    pub(crate) fn idle_status(&self) -> IdleStatus {
        let lingering = self
            .awake_until
            .lock()
            .ok()
            .and_then(|awake_until| *awake_until)
            .is_some_and(|until| Instant::now() < until);
        let awaiting_first_client = self
            .first_client_deadline
            .lock()
            .ok()
            .and_then(|deadline| *deadline)
            .is_some_and(|deadline| Instant::now() < deadline);
        IdleStatus {
            busy: self.active.load(Ordering::SeqCst) > 0 || lingering,
            pending: self.pending.load(Ordering::SeqCst) > 0 || awaiting_first_client,
            seen_active: self.seen_active.load(Ordering::SeqCst),
        }
    }

    pub(crate) fn active_channels(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    pub(crate) fn run_log(&self) -> &RunLog {
        &self.run_log
    }
}

/// One unit of activity that keeps the VM awake until dropped.
pub(crate) struct ActivityGuard {
    state: Arc<BrokerState>,
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.state.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A broker connection that has not opened a channel yet.
pub(crate) struct PendingConnection {
    state: Arc<BrokerState>,
}

impl Drop for PendingConnection {
    fn drop(&mut self) {
        self.state.pending.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Everything a broker client thread needs besides its connection.
pub(crate) struct ClientContext {
    pub(crate) state: Arc<BrokerState>,
    pub(crate) captures: mpsc::Sender<CaptureJob>,
    pub(crate) vm: Arc<VmHandle>,
    pub(crate) host_home: PathBuf,
    pub(crate) no_host_shares: bool,
    pub(crate) trace_log: Option<Arc<TraceLog>>,
}

pub(crate) fn handle_broker_client(
    mut client: UnixStream,
    pending: PendingConnection,
    context: &ClientContext,
) -> Result<()> {
    let run_log = &context.state.run_log;
    let trace_log = &context.trace_log;
    client
        .set_nonblocking(false)
        .context("set broker client blocking")?;
    client
        .set_read_timeout(Some(CLIENT_HANDSHAKE_TIMEOUT))
        .context("set broker client handshake timeout")?;
    match read_message(&mut client)? {
        Message::Hello { version } if version == PROTOCOL_VERSION => {}
        Message::Hello { version } => {
            run_log.line(format!(
                "broker.client.protocol_mismatch expected={} actual={} action=close",
                PROTOCOL_VERSION, version
            ));
            return Ok(());
        }
        other => bail!("bad client hello: {other:?}"),
    }
    write_message(
        &mut client,
        &Message::Hello {
            version: PROTOCOL_VERSION,
        },
    )?;
    let first = read_message(&mut client)?;
    client
        .set_read_timeout(None)
        .context("clear broker client handshake timeout")?;
    let first_activity = krun::deterministic_host_activity();
    if let Message::Checkpoint {
        channel_id,
        request,
    } = first
    {
        if let Some(trace) = trace_log {
            trace.event(
                "client_checkpoint_request",
                vec![
                    trace_text("channel_id", format!("{channel_id:016x}")),
                    trace_text("request", request.as_str()),
                ],
            );
        }
        let spec: CheckpointSpec =
            serde_json::from_str(&request).context("parse checkpoint request")?;
        let (reply_tx, reply_rx) = mpsc::channel();
        let job = CaptureJob::Checkpoint(CheckpointRequest {
            spec,
            reply: reply_tx,
        });
        let queued = context
            .state
            .unless_stopping(|| context.captures.send(job))?
            .transpose()
            .context("queue checkpoint capture")?;
        let reply = match queued {
            // The pending connection keeps the owner from stopping until the
            // capture this checkpoint was queued behind has finished.
            Some(()) => match reply_rx.recv().context("receive checkpoint result")? {
                Ok(()) => Message::CheckpointCreated { channel_id },
                Err(message) => Message::Error {
                    channel_id,
                    message,
                },
            },
            None => Message::Error {
                channel_id,
                message: OWNER_STOPPING.to_string(),
            },
        };
        write_message(&mut client, &reply)?;
        drop(pending);
        return Ok(());
    }
    let channel_id = match &first {
        Message::OpenExec { channel_id, .. } | Message::OpenTcp { channel_id, .. } => *channel_id,
        _ => bail!("client did not open a channel"),
    };
    run_log.line(format!("broker.client.open channel={channel_id:016x}"));
    if let Some(trace) = trace_log {
        trace_client_open(trace, &first);
    }
    let exec_cwd = match &first {
        Message::OpenExec { cwd, .. } if !context.no_host_shares => Some(PathBuf::from(cwd)),
        _ => None,
    };
    let (to_client_tx, to_client_rx) = mpsc::channel::<Message>();
    let admission = context.state.open_channel(
        channel_id,
        BrokerChannel {
            tx: to_client_tx,
            counts_as_active: true,
        },
        first,
        || match &exec_cwd {
            Some(cwd) => replace_home_write_allowlist(&context.vm, cwd, &context.host_home),
            None => Ok(()),
        },
    )?;
    let rejection = match admission {
        ChannelAdmission::Opened => None,
        ChannelAdmission::Stopping => Some(OWNER_STOPPING_NOT_STARTED.to_string()),
        ChannelAdmission::Collision => {
            let message = format!(
                "channel id collision for live channel {channel_id:016x}; deterministic mode cannot run identical commands concurrently"
            );
            run_log.line(format!("broker.client.channel_collision {message}"));
            Some(message)
        }
    };
    if let Some(message) = rejection {
        write_message(
            &mut client,
            &Message::Error {
                channel_id,
                message,
            },
        )?;
        return Ok(());
    }
    // The open channel now keeps the VM awake.
    drop(pending);
    drop(first_activity);
    let mut writer = client.try_clone().context("clone broker client")?;
    thread::spawn(move || {
        while let Ok(message) = to_client_rx.recv() {
            let _activity = krun::deterministic_host_activity();
            if write_message(&mut writer, &message).is_err() {
                break;
            }
        }
    });
    loop {
        let message = match read_message(&mut client) {
            Ok(message) => message,
            Err(_) => {
                // The client is gone. A command still running has no one to
                // report to, so end it (its whole process group) rather than
                // leave it running and keeping the VM awake.
                let _activity = krun::deterministic_host_activity();
                run_log.line(format!("broker.client.read_eof channel={channel_id:016x}"));
                context
                    .state
                    .send_to_agent_on(channel_id, Message::Close { channel_id });
                return Ok(());
            }
        };
        let _activity = krun::deterministic_host_activity();
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
        if let Some(trace) = trace_log {
            trace_client_message(trace, &message);
        }
        match context.state.send_to_agent(message) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(error) => {
                // The agent writer is gone; this channel can never complete,
                // so release its share of activity.
                context.state.close_channel(channel_id);
                return Err(error);
            }
        }
    }
}

/// Reads the guest agent's messages and routes them until the agent
/// disconnects. Snapshot-exit requests go to the capture worker; host-side
/// requests (opening URLs, forwarding newly listening ports) are served here.
pub(crate) fn run_agent_reader(
    mut agent_reader: UnixStream,
    state: Arc<BrokerState>,
    captures: mpsc::Sender<CaptureJob>,
    snapshot_started: Arc<AtomicBool>,
    agent_failed_before_snapshot: Arc<AtomicBool>,
    trace_log: Option<Arc<TraceLog>>,
) {
    let run_log = Arc::clone(&state.run_log);
    let reader_error = loop {
        let message = match read_message(&mut agent_reader) {
            Ok(message) => message,
            Err(error) => break error,
        };
        let _activity = krun::deterministic_host_activity();
        match message {
            Message::ExecStarted { channel_id } => {
                if let Some(trace) = &trace_log {
                    trace_agent_message(trace, &Message::ExecStarted { channel_id });
                }
            }
            Message::SnapshotExit { channel_id } => {
                if let Some(trace) = &trace_log {
                    trace.event(
                        "guest_snapshot_exit",
                        vec![trace_text("channel_id", format!("{channel_id:016x}"))],
                    );
                }
                let _ = captures.send(CaptureJob::SnapshotExit { channel_id });
            }
            Message::OpenUrl { channel_id, url } => {
                if state.is_stopping() {
                    continue;
                }
                if let Some((host, port)) = localhost_url_forward(&url)
                    && let Err(error) = ensure_auto_forward_port(host, port, &state)
                {
                    run_log.line(format!(
                        "open_url.forward_error channel_id={channel_id:016x} host={host} port={port} error={error:#}"
                    ));
                }
                let ok = match open_url_on_host(&url) {
                    Ok(()) => true,
                    Err(error) => {
                        run_log.line(format!(
                            "open_url.error channel_id={channel_id:016x} error={error:#}"
                        ));
                        false
                    }
                };
                if let Some(trace) = &trace_log {
                    trace.event(
                        "guest_open_url",
                        vec![
                            trace_text("channel_id", format!("{channel_id:016x}")),
                            trace_text("url", url),
                            trace_bool("ok", ok),
                        ],
                    );
                }
                let _ = state.send_to_agent(Message::OpenUrlResult { channel_id, ok });
            }
            Message::PortListeners { ports } => {
                if state.is_stopping() {
                    continue;
                }
                for port in ports.into_iter().filter(|port| *port > 1024) {
                    if let Err(error) = ensure_auto_forward_port("127.0.0.1", port, &state) {
                        run_log.line(format!("auto_forward.skip port={port} reason={error:#}"));
                    }
                }
            }
            message => {
                let channel_id = match &message {
                    Message::Data { channel_id, .. }
                    | Message::Stderr { channel_id, .. }
                    | Message::Eof { channel_id }
                    | Message::ExitStatus { channel_id, .. }
                    | Message::Close { channel_id }
                    | Message::Error { channel_id, .. } => *channel_id,
                    _ => continue,
                };
                if let Some(trace) = &trace_log {
                    trace_agent_message(trace, &message);
                }
                state.deliver_to_client(channel_id, message);
            }
        }
    };
    let snapshot_started = snapshot_started.load(Ordering::SeqCst);
    let error_message = (!snapshot_started).then(|| {
        agent_failed_before_snapshot.store(true, Ordering::SeqCst);
        format!("guest agent disconnected before command completed: {reader_error:#}")
    });
    let dropped = state.drain(error_message.as_deref());
    run_log.line(format!(
        "broker.agent.reader_eof dropped_channels={dropped} snapshot_started={snapshot_started} error={reader_error:#}"
    ));
}

/// Starts forwarding a host port the guest is listening on, unless it is
/// privileged, already forwarded, or the owner is stopping.
pub(crate) fn ensure_auto_forward_port(
    listen_host: &str,
    port: u16,
    state: &Arc<BrokerState>,
) -> Result<bool> {
    if port <= 1024 || state.is_stopping() {
        return Ok(false);
    }
    let key = (listen_host.to_string(), port);
    {
        let mut ports = state
            .auto_forward_ports
            .lock()
            .map_err(|_| anyhow!("auto-forward ports lock poisoned"))?;
        if !ports.insert(key.clone()) {
            return Ok(false);
        }
    }
    let forward = PortForward {
        listen_host: listen_host.to_string(),
        listen_port: port,
        guest_host: listen_host.to_string(),
        guest_port: port,
    };
    if let Err(error) = start_forward_listener(forward, state) {
        if let Ok(mut ports) = state.auto_forward_ports.lock() {
            ports.remove(&key);
        }
        return Err(error);
    }
    state.run_log.line(format!(
        "auto_forward.listen host={listen_host} port={port} guest_port={port}"
    ));
    Ok(true)
}

/// Records a user-requested forward so auto-forwarding does not try to bind
/// the same port again.
pub(crate) fn reserve_forward_port(state: &BrokerState, forward: &PortForward) {
    if forward.listen_host == "127.0.0.1"
        && forward.listen_port > 1024
        && let Ok(mut ports) = state.auto_forward_ports.lock()
    {
        ports.insert((forward.listen_host.clone(), forward.listen_port));
    }
}

pub(crate) fn start_forward_listener(forward: PortForward, state: &Arc<BrokerState>) -> Result<()> {
    let listener = TcpListener::bind((forward.listen_host.as_str(), forward.listen_port))
        .with_context(|| format!("listen on {}:{}", forward.listen_host, forward.listen_port))?;
    listener.set_nonblocking(true).with_context(|| {
        format!(
            "set forward listener nonblocking {}:{}",
            forward.listen_host, forward.listen_port
        )
    })?;
    let run_log = Arc::clone(&state.run_log);
    run_log.line(format!(
        "forward.listen host={} port={} guest_host={} guest_port={}",
        forward.listen_host, forward.listen_port, forward.guest_host, forward.guest_port
    ));
    let state = Arc::clone(state);
    thread::spawn(move || {
        loop {
            if state.is_stopping() {
                break;
            }
            match listener.accept() {
                Ok((stream, peer)) => {
                    let _ = stream.set_nonblocking(false);
                    run_log.line(format!(
                        "forward.accept listen_port={} peer={peer}",
                        forward.listen_port
                    ));
                    let forward = forward.clone();
                    let state = Arc::clone(&state);
                    let run_log = Arc::clone(&run_log);
                    thread::spawn(move || {
                        if let Err(error) = handle_forward_connection(stream, forward, &state) {
                            run_log.line(format!("forward.connection.error {error:#}"));
                        }
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    run_log.line(format!("forward.accept.error {error:#}"));
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
    state: &Arc<BrokerState>,
) -> Result<()> {
    if state.is_stopping() {
        return Ok(());
    }
    let _awake = state.hold_awake();
    let channel_id = new_request_id()?;
    let (to_forward_tx, to_forward_rx) = mpsc::channel::<Message>();
    let admission = state.open_channel(
        channel_id,
        BrokerChannel {
            tx: to_forward_tx,
            counts_as_active: false,
        },
        Message::OpenTcp {
            channel_id,
            host: forward.guest_host,
            port: forward.guest_port,
        },
        || Ok(()),
    )?;
    if !matches!(admission, ChannelAdmission::Opened) {
        return Ok(());
    }

    let mut local_reader = local.try_clone().context("clone local forward stream")?;
    let input_state = Arc::clone(state);
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            if input_state.is_stopping() {
                break;
            }
            match local_reader.read(&mut buf) {
                Ok(0) => {
                    input_state.send_to_agent_on(channel_id, Message::Eof { channel_id });
                    break;
                }
                Ok(n) => {
                    let sent = input_state.send_to_agent_on(
                        channel_id,
                        Message::Data {
                            channel_id,
                            bytes: buf[..n].to_vec(),
                        },
                    );
                    if !sent {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(_) => {
                    input_state.send_to_agent_on(channel_id, Message::Close { channel_id });
                    break;
                }
            }
        }
    });

    let result = (|| -> Result<()> {
        while let Ok(message) = to_forward_rx.recv() {
            match message {
                Message::Data {
                    channel_id: id,
                    bytes,
                } if id == channel_id => {
                    if local.write_all(&bytes).is_err() {
                        state
                            .run_log
                            .line(format!("forward.local_write.error channel={channel_id}"));
                        break;
                    }
                }
                Message::Eof { channel_id: id } if id == channel_id => {
                    let _ = local.shutdown(Shutdown::Write);
                }
                Message::Close { channel_id: id } if id == channel_id => break,
                Message::Error {
                    channel_id: id,
                    message,
                } if id == channel_id => bail!("{message}"),
                _ => {}
            }
        }
        Ok(())
    })();
    state.send_to_agent_on(channel_id, Message::Close { channel_id });
    state.close_channel(channel_id);
    state.keep_awake_for(FORWARD_LINGER);
    result
}
