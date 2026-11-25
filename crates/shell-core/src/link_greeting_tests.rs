use kumochi_protocol::message::{AudioSession, Capability, Hello, Incarnation, IssueSession};

use super::{write_control_hello, write_data_hello};

#[test]
fn outgoing_hello_frames_keep_their_bytes_on_both_legs() {
    let origin = IssueSession {
        incarnation: Incarnation(5),
        epoch: 128,
    };
    let sentinel = [0xa7; 32];
    for (mode, control_prefix, data_prefix, data_suffix) in [
        (
            AudioSession::Legacy,
            &b"\x23\0\0\0\0\x01b"[..],
            &b"\x23\0\0\0\0\x01b"[..],
            &b""[..],
        ),
        (
            AudioSession::QueuedRequired,
            &b"\x23\0\0\0\x25\x01b"[..],
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
            write_control_hello(&mut control, hello.clone(), mode).unwrap();
            write_data_hello(&mut data, hello, mode, origin).unwrap();
            assert_eq!(control, [control_prefix, &field].concat(), "{mode:?}");
            assert_eq!(
                data,
                [data_prefix, &field, data_suffix].concat(),
                "{mode:?}"
            );
        }
    }
}
