use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_MESSAGE_SIZE: u32 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    SnapshotExit {
        channel_id: u64,
    },
    SnapshotReady,
}
