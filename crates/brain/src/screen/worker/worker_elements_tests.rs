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
    let mut requests = Vec::new();
    for (id, body) in bodies.iter().enumerate() {
        let mut input = Vec::new();
        write_message(
            &mut input,
            u64::try_from(id).unwrap(),
            body,
            ScreenKind::MAX_REQUEST,
        )
        .unwrap();
        requests.push((times.get(id).copied().unwrap_or(0), input));
    }
    let start = Instant::now();
    let time = Cell::new(start);
    let mut input = Requests {
        pending: requests.into_iter(),
        current: std::io::Cursor::new(Vec::new()),
        start,
        time: &time,
    };
    let mut output = Vec::new();
    serve_on(
        &mut input,
        &mut output,
        || Ok(Box::new(Good)),
        reader,
        &|| time.get(),
    )
    .unwrap();
    let mut bytes = output.as_slice();
    let mut replies = Vec::new();
    while let Some(reply) = read_request::<ReplyBody>(&mut bytes, ScreenKind::MAX_REPLY).unwrap() {
        replies.push(reply);
    }
    replies
}

struct Requests<'a> {
    pending: std::vec::IntoIter<(u64, Vec<u8>)>,
    current: std::io::Cursor<Vec<u8>>,
    start: Instant,
    time: &'a Cell<Instant>,
}

impl std::io::Read for Requests<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if self.current.position() == u64::try_from(self.current.get_ref().len()).unwrap() {
            let Some((elapsed, bytes)) = self.pending.next() else {
                return Ok(0);
            };
            self.time.set(self.start + Duration::from_millis(elapsed));
            self.current = std::io::Cursor::new(bytes);
        }
        std::io::Read::read(&mut self.current, output)
    }
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
            &[0, elapsed, elapsed, elapsed],
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
    let replies = exchange(&[capture(1, 1), elements(1)], &[0, 249]);
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
    static OPENS: Cell<u32> = const { Cell::new(0) };
}

#[test]
fn opener_failures_answer_fault_keep_ping_and_retry_for_the_next_capture() {
    let outer: OpenElements = |wake| {
        let attempt = OPENS.with(|opens| {
            opens.set(opens.get() + 1);
            opens.get()
        });
        if attempt == 1 {
            Err(-7)
        } else {
            open_reader(wake)
        }
    };
    let inner: OpenElements = |wake| {
        let mut attempts = 0;
        crate::screen::reader::ThreadedReader::new(
            move || {
                attempts += 1;
                if attempts == 1 {
                    Err(-7)
                } else {
                    Ok(crate::screen::elements::tests::FakeSource::flat(1))
                }
            },
            crate::screen::elements::tests::FakePlatform(false),
            wake,
        )
        .map(|reader| -> Box<dyn crate::screen::elements::ElementReader> { Box::new(reader) })
    };
    for opener in [outer, inner] {
        OPENS.with(|opens| opens.set(0));
        let replies = exchange_with(
            &[
                capture(1, 1),
                elements(1),
                elements(1),
                RequestBody::Ping,
                capture(2, 2),
                elements(2),
                RequestBody::Ping,
            ],
            &[],
            opener,
        );
        assert_eq!(
            &replies[..5],
            &[
                (0, captured(1)),
                (1, ReplyBody::Fault(-7)),
                (2, ReplyBody::Refused(Refusal::Reused)),
                (3, ReplyBody::Pong),
                (4, captured(2))
            ]
        );
        let ReplyBody::Elements(tree) = &replies[5].1 else {
            panic!("expected retried tree")
        };
        assert_eq!(replies[5].0, 5);
        assert_eq!(tree.capture, CaptureId(2));
        assert_eq!(tree.availability, crate::screen::Availability::Read);
        assert_eq!(tree.elements.len(), 1);
        assert_eq!(tree.cut, None);
        assert_eq!(replies[6], (6, ReplyBody::Pong));
        assert_eq!(replies.len(), 7);
    }
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
    let opener: OpenElements = |wake| {
        let (send, receive) = std::sync::mpsc::channel();
        RELEASE.with(|slot| *slot.borrow_mut() = Some(send));
        let mut receive = Some(receive);
        crate::screen::reader::ThreadedReader::new(
            move || Ok(Blocked(receive.take().unwrap())),
            crate::screen::elements::tests::FakePlatform(false),
            wake,
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
