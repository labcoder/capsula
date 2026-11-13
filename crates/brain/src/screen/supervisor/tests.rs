use kumochi_protocol::geometry::PhysicalRect;

use super::super::wire::CaptureId;
use super::*;
use crate::supervise::WorkerLost;

#[tokio::test]
async fn capture_accepts_only_its_own_frame_and_passes_refusals_through() {
    let (inner, mut receive) = supervise::detached::<Screen>();
    let handle = ScreenHandle {
        inner,
        deadline: REQUEST_DEADLINE,
    };
    let display = DisplayRef(PhysicalRect {
        origin: [0, 0],
        size: [2560, 1440],
    });
    let meta = |capture, display| FrameMeta {
        capture: CaptureId(capture),
        display,
        dpi: 96,
        acquired_after_ms: 55,
    };
    let other = PhysicalRect {
        origin: [2560, 0],
        size: [2560, 1440],
    };
    for (reply, expected) in [
        (
            Ok(ReplyBody::Captured(meta(7, display.0))),
            Ok(meta(7, display.0)),
        ),
        (
            Ok(ReplyBody::Captured(meta(8, display.0))),
            Err(CaptureError::Screen(ScreenError::WorkerLost)),
        ),
        (
            Ok(ReplyBody::Captured(meta(7, other))),
            Err(CaptureError::Screen(ScreenError::WorkerLost)),
        ),
        (
            Ok(ReplyBody::Refused(Refusal::Stale)),
            Err(CaptureError::Refused(Refusal::Stale)),
        ),
        (
            Ok(ReplyBody::Pong),
            Err(CaptureError::Screen(ScreenError::WorkerLost)),
        ),
        (
            Ok(ReplyBody::Fault(-9)),
            Err(CaptureError::Screen(ScreenError::Fault(-9))),
        ),
        (
            Err(WorkerLost::Lost),
            Err(CaptureError::Screen(ScreenError::WorkerLost)),
        ),
        (
            Err(WorkerLost::ExitUnconfirmed),
            Err(CaptureError::Screen(ScreenError::ExitUnconfirmed)),
        ),
    ] {
        let ticket = TicketWire::new(CaptureId(7), 1, [0; 16], 2_000);
        let capture = handle.capture(ticket, display, Exclusion::KumochiWindows);
        let answer = async {
            let (body, answer) = receive.recv().await.expect("queued capture");
            assert!(matches!(
                body,
                RequestBody::Capture {
                    exclude: Exclusion::KumochiWindows,
                    ..
                }
            ));
            assert!(
                answer
                    .send(reply.map(|reply| (reply, Generation(0))))
                    .is_ok(),
                "the capture is waiting"
            );
        };
        let (result, ()) = tokio::join!(capture, answer);
        assert_eq!(result, expected);
    }
}

#[path = "supervisor_elements_tests.rs"]
mod elements;
