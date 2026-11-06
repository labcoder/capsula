use super::*;
use crate::supervise::{read_request, write_message};

#[test]
fn shell_identity_round_trips_privately_through_the_worker_wire() {
    let ticket = TicketWire::new(
        kumochi_protocol::message::RunId(1),
        CaptureId(7),
        3,
        *b"shell-pid-secret",
        2_000,
    )
    .for_shell(305_419_896);
    let mut bytes = Vec::new();
    write_message(&mut bytes, 9, &ticket, MAX_REPLY_BYTES).expect("small ticket");
    let (id, decoded) = read_request::<TicketWire>(&mut bytes.as_slice(), MAX_REPLY_BYTES)
        .expect("one ticket")
        .expect("not end of input");
    assert_eq!(id, 9);
    assert_eq!(decoded.shell_pid(), 305_419_896);
    assert_eq!(decoded.nonce, *b"shell-pid-secret");
    let printed = format!("{decoded:?}");
    assert!(!printed.contains("305419896"));
    assert!(!printed.contains("shell-pid-secret"));
    assert!(!printed.contains(&format!("{:?}", b"shell-pid-secret")));
}

#[test]
fn request_debug_is_redacted() {
    assert_eq!(format!("{:?}", RequestBody::Ping), "RequestBody(***)");
}

#[test]
fn capture_replies_round_trip() {
    let captured = ReplyBody::Captured(FrameMeta {
        capture: CaptureId(u64::MAX),
        display: PhysicalRect {
            origin: [i32::MIN, -1440],
            size: [8192, 8192],
        },
        dpi: 288,
        acquired_after_ms: u32::MAX,
    });
    for body in [captured, ReplyBody::Refused(Refusal::DisplayChanged)] {
        let mut bytes = Vec::new();
        write_message(&mut bytes, 4, &body, MAX_REPLY_BYTES).expect("small reply");
        assert_eq!(
            read_request::<ReplyBody>(&mut bytes.as_slice(), MAX_REPLY_BYTES).expect("one reply"),
            Some((4, body))
        );
    }
}
