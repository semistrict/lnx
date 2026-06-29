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
