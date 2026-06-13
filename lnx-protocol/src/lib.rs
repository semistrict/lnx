use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 4;
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
mod tests {
    use super::*;

    #[test]
    fn postcard_round_trips_open_exec() {
        let message = Message::OpenExec {
            channel_id: 42,
            argv: vec!["bash".into(), "-lc".into(), "echo hi".into()],
            cwd: "/Users/ramon/src/project".into(),
            pty: true,
            term: "xterm-256color".into(),
            colorterm: "truecolor".into(),
            rows: 48,
            cols: 160,
            uid: 501,
            gid: 20,
            group: "staff".into(),
            env: vec![
                ("LANG".into(), "en_US.UTF-8".into()),
                ("COLORTERM".into(), "truecolor".into()),
            ],
        };

        let encoded = postcard::to_allocvec(&message).expect("encode");
        assert!(encoded.len() < MAX_MESSAGE_SIZE as usize);
        let decoded: Message = postcard::from_bytes(&encoded).expect("decode");

        assert_eq!(decoded, message);
    }

    #[test]
    fn protocol_version_is_encoded_in_hello() {
        let encoded = postcard::to_allocvec(&Message::Hello {
            version: PROTOCOL_VERSION,
        })
        .expect("encode");
        let decoded: Message = postcard::from_bytes(&encoded).expect("decode");

        assert_eq!(
            decoded,
            Message::Hello {
                version: PROTOCOL_VERSION
            }
        );
    }
}
