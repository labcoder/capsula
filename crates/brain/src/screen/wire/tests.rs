use super::*;

#[test]
fn shell_identity_round_trips_privately_and_legacy_tickets_default_to_zero() {
    let ticket = TicketWire::new(
        kumochi_protocol::message::RunId(1),
        CaptureId(7),
        3,
        *b"shell-pid-secret",
        2_000,
    )
    .for_shell(305_419_896);
    let value = serde_json::to_value(&ticket).unwrap();
    let decoded: TicketWire = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(decoded.shell_pid(), 305_419_896);
    assert_eq!(decoded.nonce, *b"shell-pid-secret");
    let printed = format!("{decoded:?}");
    assert!(!printed.contains("305419896"));
    assert!(!printed.contains("shell-pid-secret"));
    assert!(!printed.contains(&format!("{:?}", b"shell-pid-secret")));
    let mut legacy = value;
    legacy.as_object_mut().unwrap().remove("shell_pid");
    assert_eq!(
        serde_json::from_value::<TicketWire>(legacy)
            .unwrap()
            .shell_pid(),
        0
    );
}
use crate::supervise::{read_request, write_message};

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
