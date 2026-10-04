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

#[test]
fn restore_sync_carries_entropy() {
    let message = Message::RestoreSync {
        channel_id: 7,
        entropy: vec![1, 2, 3, 4],
    };
    let encoded = postcard::to_allocvec(&message).expect("encode");
    let decoded: Message = postcard::from_bytes(&encoded).expect("decode");

    assert_eq!(decoded, message);
}

#[test]
fn exec_started_round_trips() {
    let message = Message::ExecStarted { channel_id: 11 };
    let encoded = postcard::to_allocvec(&message).expect("encode");
    let decoded: Message = postcard::from_bytes(&encoded).expect("decode");

    assert_eq!(decoded, message);
}

#[test]
fn open_url_round_trips_with_result() {
    let request = Message::OpenUrl {
        channel_id: 9,
        url: "http://p3773-default.lnx/pair#token=abc".into(),
    };
    let encoded = postcard::to_allocvec(&request).expect("encode request");
    let decoded: Message = postcard::from_bytes(&encoded).expect("decode request");
    assert_eq!(decoded, request);

    let response = Message::OpenUrlResult {
        channel_id: 9,
        ok: true,
    };
    let encoded = postcard::to_allocvec(&response).expect("encode response");
    let decoded: Message = postcard::from_bytes(&encoded).expect("decode response");
    assert_eq!(decoded, response);
}

#[test]
fn port_listeners_round_trip() {
    let message = Message::PortListeners {
        ports: vec![3000, 5173, 3773],
    };
    let encoded = postcard::to_allocvec(&message).expect("encode");
    let decoded: Message = postcard::from_bytes(&encoded).expect("decode");

    assert_eq!(decoded, message);
}

/// Agents restored from snapshots still speak protocol 11, so every message
/// they know must keep its postcard variant index. A new message that lands
/// anywhere but the end of `Message` fails here.
#[test]
fn protocol_11_messages_keep_their_wire_positions() {
    let protocol_11 = [
        (Message::Hello { version: 11 }, 0),
        (Message::Close { channel_id: 1 }, 11),
        (
            Message::Error {
                channel_id: 1,
                message: String::new(),
            },
            12,
        ),
        (
            Message::RestoreSync {
                channel_id: 1,
                entropy: Vec::new(),
            },
            13,
        ),
        (Message::RestoreSynced { channel_id: 1 }, 14),
        (Message::SnapshotReady, 19),
        (Message::ForwardAdded { channel_id: 1 }, 21),
    ];
    for (message, index) in protocol_11 {
        let encoded = postcard::to_allocvec(&message).expect("encode");
        assert_eq!(encoded[0], index, "{message:?}");
    }
    assert_eq!(
        postcard::to_allocvec(&Message::Hello { version: 11 }).expect("encode"),
        vec![0, 11]
    );
    assert_eq!(
        postcard::to_allocvec(&Message::SetClock { unix_nanos: 1 }).expect("encode")[0],
        22
    );
}

#[test]
fn host_speaks_every_agent_protocol_from_the_oldest_supported() {
    assert!(!agent_protocol_supported(OLDEST_AGENT_PROTOCOL - 1));
    assert!(agent_protocol_supported(OLDEST_AGENT_PROTOCOL));
    assert!(agent_protocol_supported(PROTOCOL_VERSION));
    assert!(!agent_protocol_supported(PROTOCOL_VERSION + 1));
    assert_eq!(Message::SnapshotReady.min_protocol(), OLDEST_AGENT_PROTOCOL);
    assert_eq!(Message::SetClock { unix_nanos: 0 }.min_protocol(), 12);
}
