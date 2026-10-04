use serde::{Deserialize, Serialize};

/// The host/agent protocol. Bump it whenever the host and the guest agent
/// would disagree about anything: messages, their meaning, or what the agent
/// does with them. A VM resumed from a memory snapshot keeps running the agent
/// it was booted with, and lnx resumes it only if this host still speaks that
/// agent's version (see [`OLDEST_AGENT_PROTOCOL`]).
/// (11: the agent applies every environment variable the host sends, runs
/// each command in its own session, and hangs it up on Close.
/// 12: `SetClock`.)
pub const PROTOCOL_VERSION: u16 = 12;

/// The oldest agent protocol this host still speaks, so memory snapshots
/// taken with older agents keep restoring. Raising it makes every snapshot
/// whose agent is older unrestorable. Postcard encodes a variant by its
/// position, so new messages go at the end of [`Message`], and the host
/// sends them only to agents whose `Hello` says they know them.
pub const OLDEST_AGENT_PROTOCOL: u16 = 11;

/// Whether this host can talk to an agent that speaks `version`.
pub fn agent_protocol_supported(version: u16) -> bool {
    (OLDEST_AGENT_PROTOCOL..=PROTOCOL_VERSION).contains(&version)
}
pub const MAX_MESSAGE_SIZE: u32 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    Hello {
        version: u16,
    },
    OpenExec {
        channel_id: u64,
        argv: Vec<String>,
        cwd: String,
        pty: bool,
        term: String,
        colorterm: String,
        rows: u16,
        cols: u16,
        uid: u32,
        gid: u32,
        group: String,
        env: Vec<(String, String)>,
    },
    OpenTcp {
        channel_id: u64,
        host: String,
        port: u16,
    },
    /// A client asks a running owner for a capture. `request` is the
    /// JSON-encoded checkpoint request (what to name it, where to export it).
    Checkpoint {
        channel_id: u64,
        request: String,
    },
    CheckpointCreated {
        channel_id: u64,
    },
    Data {
        channel_id: u64,
        bytes: Vec<u8>,
    },
    Stderr {
        channel_id: u64,
        bytes: Vec<u8>,
    },
    ExecStarted {
        channel_id: u64,
    },
    Eof {
        channel_id: u64,
    },
    WindowResize {
        channel_id: u64,
        rows: u16,
        cols: u16,
    },
    ExitStatus {
        channel_id: u64,
        status: i32,
    },
    Close {
        channel_id: u64,
    },
    Error {
        channel_id: u64,
        message: String,
    },
    RestoreSync {
        channel_id: u64,
        entropy: Vec<u8>,
    },
    RestoreSynced {
        channel_id: u64,
    },
    SnapshotExit {
        channel_id: u64,
    },
    OpenUrl {
        channel_id: u64,
        url: String,
    },
    OpenUrlResult {
        channel_id: u64,
        ok: bool,
    },
    PortListeners {
        ports: Vec<u16>,
    },
    SnapshotReady,
    /// A client asks a running owner to forward a host port into the guest
    /// (broker only; the agent never sees it).
    AddForward {
        channel_id: u64,
        listen_host: String,
        listen_port: u16,
        guest_host: String,
        guest_port: u16,
    },
    ForwardAdded {
        channel_id: u64,
    },
    /// Sets the guest's wall clock (protocol 12). A VM paused for a capture,
    /// or restored from a snapshot, resumes with the clock where it stopped.
    SetClock {
        unix_nanos: u64,
    },
    /// A client asks a running owner to keep the VM running until it is
    /// stopped, instead of suspending it once idle (broker only).
    KeepRunning {
        channel_id: u64,
    },
    KeepingRunning {
        channel_id: u64,
    },
}

impl Message {
    /// The first protocol whose agents understand this message.
    pub fn min_protocol(&self) -> u16 {
        match self {
            Message::SetClock { .. }
            | Message::KeepRunning { .. }
            | Message::KeepingRunning { .. } => 12,
            _ => OLDEST_AGENT_PROTOCOL,
        }
    }

    /// Tells the guest the host's wall-clock time now.
    pub fn set_clock_now() -> Message {
        let unix_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or_default();
        Message::SetClock { unix_nanos }
    }
}

#[cfg(test)]
mod tests;
