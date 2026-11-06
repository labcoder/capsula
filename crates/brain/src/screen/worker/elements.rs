use std::cell::Cell;
use std::time::Duration;

use super::super::*;
use crate::screen::elements::tests::{RECT, open_reader};
use crate::screen::{CaptureId, ElementTarget, Exclusion, FrameMeta, Refusal, TicketWire};
use kumochi_protocol::geometry::DisplayRef;
use kumochi_protocol::message::RunId;

struct Good;
impl Capturer for Good {
    fn capture_into(
        &mut self,
        display: &DisplayRef,
        _: Exclusion,
        until: Instant,
        frame: &mut crate::screen::RawFrame,
    ) -> Result<crate::screen::Acquired, Failure> {
        frame.begin(display.0.size[0], display.0.size[1])?.fill(7);
        Ok(crate::screen::Acquired {
            arrived: until.checked_sub(Duration::from_millis(2_000)).unwrap(),
            dpi: 96,
        })
    }
}

fn capture(id: u64, generation: u64) -> RequestBody {
    RequestBody::Capture {
        ticket: TicketWire::new(RunId(1), CaptureId(id), generation, [0; 16], 2_000),
        display: DisplayRef(RECT),
        exclude: Exclusion::KumochiWindows,
    }
}

fn elements(id: u64) -> RequestBody {
    RequestBody::Elements {
        capture: CaptureId(id),
        target: ElementTarget::Foreground,
    }
}

fn captured(id: u64) -> ReplyBody {
    ReplyBody::Captured(FrameMeta {
        capture: CaptureId(id),
        display: RECT,
        dpi: 96,
        acquired_after_ms: 0,
    })
}

fn exchange(bodies: &[RequestBody], times: &[u64]) -> Vec<(u64, ReplyBody)> {
    exchange_with(bodies, times, open_reader)
}

fn exchange_with(
    bodies: &[RequestBody],
    times: &[u64],
    reader: super::super::OpenElements,
) -> Vec<(u64, ReplyBody)> {
    let mut input = Vec::new();
    for (id, body) in bodies.iter().enumerate() {
        write_message(
            &mut input,
            u64::try_from(id).unwrap(),
            body,
            ScreenKind::MAX_REQUEST,
        )
        .unwrap();
    }
    let now = Instant::now();
    let calls = Cell::new(0);
    let clock = || {
        let call = calls.get();
        calls.set(call + 1);
        now + Duration::from_millis(times.get(call).copied().unwrap_or(0))
    };
    let mut output = Vec::new();
    serve_on(
        &mut input.as_slice(),
        &mut output,
        || Ok(Box::new(Good)),
        reader,
        &clock,
    )
    .unwrap();
    let mut bytes = output.as_slice();
    let mut replies = Vec::new();
    while let Some(reply) = read_request::<ReplyBody>(&mut bytes, ScreenKind::MAX_REPLY).unwrap() {
        replies.push(reply);
    }
    replies
}

#[test]
fn elements_need_the_held_capture_and_stale_does_not_spend_it() {
    let replies = exchange(
        &[
            elements(1),
            capture(1, 1),
            elements(2),
            elements(1),
            elements(1),
        ],
        &[],
    );
    assert_eq!(replies[0], (0, ReplyBody::Refused(Refusal::Stale)));
    assert_eq!(replies[1], (1, captured(1)));
    assert_eq!(replies[2], (2, ReplyBody::Refused(Refusal::Stale)));
    let ReplyBody::Elements(tree) = &replies[3].1 else {
        panic!("expected own tree")
    };
    assert_eq!(tree.capture, CaptureId(1));
    assert_eq!(tree.elements.len(), 1);
    assert_eq!(replies[4], (4, ReplyBody::Refused(Refusal::Reused)));
    assert_eq!(replies.len(), 5);
}

#[test]
fn refused_capture_drops_authority_and_invalid_walk_spends_it() {
    let replies = exchange(
        &[
            capture(1, 1),
            capture(2, 1),
            elements(1),
            elements(2),
            capture(3, 2),
            RequestBody::Elements {
                capture: CaptureId(3),
                target: ElementTarget::Window {
                    window: kumochi_protocol::geometry::WindowRef(0),
                    pid: 0,
                },
            },
            elements(3),
        ],
        &[],
    );
    assert_eq!(
        &replies[..5],
        &[
            (0, captured(1)),
            (1, ReplyBody::Refused(Refusal::Reused)),
            (2, ReplyBody::Refused(Refusal::Stale)),
            (3, ReplyBody::Refused(Refusal::Stale)),
            (4, captured(3))
        ]
    );
    assert_eq!(replies[5], (5, ReplyBody::Refused(Refusal::Invalid)));
    assert_eq!(replies[6], (6, ReplyBody::Refused(Refusal::Reused)));
    assert_eq!(replies.len(), 7);
}

#[test]
fn expired_walk_stays_spent_after_valid_input() {
    for elapsed in [250, 2_000] {
        let replies = exchange(
            &[capture(1, 1), elements(1), elements(1), RequestBody::Ping],
            &[0, 0, 0, elapsed, elapsed],
        );
        assert_eq!(
            replies,
            [
                (0, captured(1)),
                (1, ReplyBody::Refused(Refusal::Expired)),
                (2, ReplyBody::Refused(Refusal::Reused)),
                (3, ReplyBody::Pong)
            ]
        );
    }
}

#[test]
fn walk_can_start_one_millisecond_before_the_frame_age_limit() {
    let replies = exchange(&[capture(1, 1), elements(1)], &[0, 0, 0, 249, 249]);
    assert_eq!(replies[0], (0, captured(1)));
    let ReplyBody::Elements(tree) = &replies[1].1 else {
        panic!("expected last valid walk")
    };
    assert_eq!(tree.capture, CaptureId(1));
    assert_eq!(tree.availability, crate::screen::Availability::Read);
    assert_eq!(tree.elements.len(), 1);
    assert_eq!(tree.cut, None);
    assert_eq!(replies.len(), 2);
}

thread_local! {
    static RELEASE: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const { std::cell::RefCell::new(None) };
}

struct Blocked(std::sync::mpsc::Receiver<()>);
impl crate::screen::elements::ElementSource for Blocked {
    type Node = ();
    fn root(
        &mut self,
        _: ElementTarget,
    ) -> Result<
        ((), kumochi_protocol::geometry::PhysicalRect, u32),
        crate::screen::elements::SourceError,
    > {
        Ok(((), RECT, 8))
    }
    fn read(
        &mut self,
        (): &(),
    ) -> Result<crate::screen::elements::NodeProps, crate::screen::elements::SourceError> {
        self.0.recv_timeout(Duration::from_secs(3)).unwrap();
        Ok(crate::screen::elements::NodeProps {
            name: String::new(),
            password: false,
            control_type: 0,
            rect: RECT,
        })
    }
    fn first_child(&mut self, (): &()) -> Result<Option<()>, crate::screen::elements::SourceError> {
        Ok(None)
    }
    fn next_sibling(
        &mut self,
        (): &(),
    ) -> Result<Option<()>, crate::screen::elements::SourceError> {
        Ok(None)
    }
}

#[test]
fn first_busy_answers_and_second_busy_replies_before_retiring() {
    let opener: OpenElements = || {
        let (send, receive) = std::sync::mpsc::channel();
        RELEASE.with(|slot| *slot.borrow_mut() = Some(send));
        crate::screen::reader::ThreadedReader::new(
            move || Ok(Blocked(receive)),
            crate::screen::elements::tests::FakePlatform(false),
        )
        .map(|reader| -> Box<dyn crate::screen::elements::ElementReader> { Box::new(reader) })
    };
    let replies = exchange_with(
        &[
            capture(1, 1),
            elements(1),
            RequestBody::Ping,
            capture(2, 2),
            elements(2),
            RequestBody::Ping,
        ],
        &[],
        opener,
    );
    RELEASE.with(|slot| slot.borrow_mut().take().unwrap().send(()).unwrap());
    assert_eq!(
        replies,
        [
            (0, captured(1)),
            (
                1,
                ReplyBody::Elements(crate::screen::ElementTree::unavailable(
                    CaptureId(1),
                    crate::screen::Unavailable::Busy
                ))
            ),
            (2, ReplyBody::Pong),
            (3, captured(2)),
            (
                4,
                ReplyBody::Elements(crate::screen::ElementTree::unavailable(
                    CaptureId(2),
                    crate::screen::Unavailable::Busy
                ))
            )
        ]
    );
}
