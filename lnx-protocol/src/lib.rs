use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 5;
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
    Checkpoint {
        channel_id: u64,
        path: String,
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
    SnapshotReady,
}

#[cfg(test)]
mod tests;
