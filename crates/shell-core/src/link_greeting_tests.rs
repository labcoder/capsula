use kumochi_protocol::message::{AudioSession, Capability, Hello, Incarnation, IssueSession};

use super::{write_control_hello, write_data_hello};

#[test]
fn outgoing_hello_frames_keep_their_bytes_on_both_legs() {
    let origin = IssueSession {
        incarnation: Incarnation(5),
        epoch: 128,
    };
    let sentinel = [0xa7; 32];
    let previous = IssueSession {
        incarnation: Incarnation(5),
        epoch: 127,
    };
    for (mode, repair, control_prefix, control_suffix, data_prefix, data_suffix) in [
        (
            AudioSession::Legacy,
            None,
            &b"\x23\0\0\0\0\x01b"[..],
            &b""[..],
            &b"\x23\0\0\0\0\x01b"[..],
            &b""[..],
        ),
        (
            AudioSession::QueuedRequired,
            None,
            &b"\x23\0\0\0\x25\x01b"[..],
            &b""[..],
            &b"\x26\0\0\0\x09\x01b"[..],
            &b"\x05\x80\x01"[..],
        ),
        (
            AudioSession::QueuedRequired,
            Some(previous),
            &b"\x25\0\0\0\x26\x01b"[..],
            &b"\x05\x7f"[..],
            &b"\x26\0\0\0\x09\x01b"[..],
            &b"\x05\x80\x01"[..],
        ),
    ] {
        for (hello, field) in [
            (
                Hello {
                    build_id: "b".to_owned(),
                    capability: Capability::new(sentinel),
                },
                sentinel,
            ),
            (Hello::session("b".to_owned()), [0; 32]),
        ] {
            let mut control = Vec::new();
            let mut data = Vec::new();
            write_control_hello(&mut control, hello.clone(), mode, repair).unwrap();
            write_data_hello(&mut data, hello, mode, origin).unwrap();
            assert_eq!(
                control,
                [control_prefix, &field, control_suffix].concat(),
                "{mode:?}, repair {repair:?}"
            );
            assert_eq!(
                data,
                [data_prefix, &field, data_suffix].concat(),
                "{mode:?}"
            );
        }
    }
}
