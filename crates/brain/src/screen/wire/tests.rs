use super::*;
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
