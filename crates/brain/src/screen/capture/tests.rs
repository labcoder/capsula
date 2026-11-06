use std::cell::Cell;
use std::rc::Rc;

use kumochi_protocol::geometry::PhysicalRect;
use kumochi_protocol::message::RunId;

use super::*;
use crate::screen::frame::MAX_SIDE;
use crate::screen::wire::CaptureId;

const TTL: u32 = 2_000;

/// What the fake platform does, shared with the test that steers it.
struct Knobs {
    clock: Cell<Instant>,
    calls: Cell<u32>,
    size: Cell<[u32; 2]>,
    fill: Cell<u8>,
    takes: Cell<Duration>,
    fail: Cell<Option<Failure>>,
}

struct Fake(Rc<Knobs>);

impl Capturer for Fake {
    fn capture_into(
        &mut self,
        _: &DisplayRef,
        _: Exclusion,
        until: Instant,
        frame: &mut RawFrame,
    ) -> Result<Acquired, Failure> {
        let knobs = &self.0;
        knobs.calls.set(knobs.calls.get() + 1);
        let arrived = knobs.clock.get();
        if arrived >= until {
            return Err(Refusal::Expired.into());
        }
        let [width, height] = knobs.size.get();
        let bytes = frame.begin(width, height)?;
        if let Some(failure) = knobs.fail.get() {
            bytes[0] = knobs.fill.get();
            return Err(failure);
        }
        bytes.fill(knobs.fill.get());
        knobs.clock.set(arrived + knobs.takes.get());
        Ok(Acquired { arrived, dpi: 144 })
    }
}

struct Rig {
    screen: Screen,
    knobs: Rc<Knobs>,
    start: Instant,
}

impl Rig {
    fn new(size: [u32; 2], fill: u8) -> Self {
        let start = Instant::now();
        let knobs = Rc::new(Knobs {
            clock: Cell::new(start),
            calls: Cell::new(0),
            size: Cell::new(size),
            fill: Cell::new(fill),
            takes: Cell::new(Duration::ZERO),
            fail: Cell::new(None),
        });
        let screen = Screen {
            capturer: Some(Box::new(Fake(Rc::clone(&knobs)))),
            ..Screen::default()
        };
        Self {
            screen,
            knobs,
            start,
        }
    }

    /// Captures `display` under `generation`, received at `start` and asked at `start + at`.
    fn take(&mut self, generation: u64, display: [u32; 2], at: u64) -> Result<FrameMeta, Failure> {
        self.knobs.clock.set(self.start + Duration::from_millis(at));
        let knobs = Rc::clone(&self.knobs);
        self.screen.capture(
            &ticket(generation),
            &rect(display),
            Exclusion::KumochiWindows,
            self.start,
            &move || knobs.clock.get(),
            (|| Err(9), crate::screen::elements::tests::open_reader),
        )
    }
}

fn ticket(generation: u64) -> TicketWire {
    TicketWire::new(
        RunId(3),
        CaptureId(generation * 10),
        generation,
        [0xAB; 16],
        TTL,
    )
}

fn rect(size: [u32; 2]) -> DisplayRef {
    DisplayRef(PhysicalRect {
        origin: [-2560, 0],
        size,
    })
}

fn meta(generation: u64, size: [u32; 2], after: u32) -> FrameMeta {
    FrameMeta {
        capture: CaptureId(generation * 10),
        display: rect(size).0,
        dpi: 144,
        acquired_after_ms: after,
    }
}

#[test]
fn local_validity_ends_at_exactly_ttl_and_drops_the_previous_image() {
    let mut rig = Rig::new([4, 3], 7);
    assert_eq!(rig.take(1, [4, 3], 1_999), Ok(meta(1, [4, 3], 1_999)));
    assert_eq!(
        rig.screen.frame().image(),
        Some((meta(1, [4, 3], 1_999), [7_u8; 48].as_slice()))
    );
    assert_eq!(rig.take(2, [4, 3], 2_000), Err(Refusal::Expired.into()));
    assert_eq!(rig.knobs.calls.get(), 1);
    assert_eq!(rig.screen.frame().image(), None);
}

#[test]
fn generations_at_or_below_the_high_water_mark_stay_spent() {
    let mut book = TicketBook::default();
    assert_eq!(book.spend(5), Ok(()));
    assert_eq!(book.spend(5), Err(Refusal::Reused));
    assert_eq!(book.spend(4), Err(Refusal::Stale));
    assert_eq!(book.spend(7), Ok(()));
    assert_eq!(book.spend(6), Err(Refusal::Stale));
    assert_eq!(book.spend(7), Err(Refusal::Reused));
    let mut rig = Rig::new([4, 3], 7);
    rig.knobs.fail.set(Some(Failure::Fault(-5)));
    assert_eq!(rig.take(9, [4, 3], 0), Err(Failure::Fault(-5)));
    rig.knobs.fail.set(None);
    assert_eq!(rig.take(9, [4, 3], 0), Err(Refusal::Reused.into()));
    assert_eq!(rig.take(8, [4, 3], 0), Err(Refusal::Stale.into()));
    assert_eq!(rig.knobs.calls.get(), 1);
    assert_eq!(rig.take(10, [4, 3], 0), Ok(meta(10, [4, 3], 0)));
}

#[test]
fn a_frame_that_completes_after_expiry_is_never_published() {
    let mut rig = Rig::new([4, 3], 7);
    assert_eq!(rig.take(1, [4, 3], 0), Ok(meta(1, [4, 3], 0)));
    rig.knobs.fill.set(9);
    rig.knobs.takes.set(Duration::from_millis(1));
    assert_eq!(rig.take(2, [4, 3], 1_999), Err(Refusal::Expired.into()));
    assert_eq!(rig.screen.frame().image(), None);
    rig.knobs.fail.set(Some(Failure::Retire(-7)));
    assert_eq!(rig.take(3, [4, 3], 0), Err(Failure::Retire(-7)));
    assert_eq!(rig.screen.frame().image(), None);
}

#[test]
fn a_copy_that_fails_partway_after_a_good_frame_leaves_no_image() {
    let mut rig = Rig::new([4, 3], 7);
    for (generation, failure) in [(2, Failure::Fault(-5)), (4, Failure::Retire(-7))] {
        rig.knobs.fail.set(None);
        assert_eq!(
            rig.take(generation - 1, [4, 3], 0),
            Ok(meta(generation - 1, [4, 3], 0))
        );
        rig.knobs.fail.set(Some(failure));
        assert_eq!(rig.take(generation, [4, 3], 0), Err(failure));
        assert_eq!(rig.screen.frame().image(), None);
    }
}

#[test]
fn the_cpu_buffer_keeps_its_allocation_for_an_equal_or_smaller_frame() {
    let mut rig = Rig::new([4, 3], 1);
    assert_eq!(rig.take(1, [4, 3], 0), Ok(meta(1, [4, 3], 0)));
    let warm = rig.screen.frame().allocation();
    assert_eq!(rig.take(2, [4, 3], 0), Ok(meta(2, [4, 3], 0)));
    assert_eq!(rig.screen.frame().allocation(), warm);
    rig.knobs.size.set([2, 2]);
    rig.knobs.fill.set(5);
    assert_eq!(rig.take(3, [2, 2], 0), Ok(meta(3, [2, 2], 0)));
    assert_eq!(rig.screen.frame().allocation(), warm);
    assert_eq!(
        rig.screen.frame().image(),
        Some((meta(3, [2, 2], 0), [5_u8; 16].as_slice()))
    );
}

#[test]
fn a_frame_of_another_size_is_refused_as_a_changed_display() {
    let mut rig = Rig::new([3, 4], 7);
    assert_eq!(rig.take(1, [4, 3], 0), Err(Refusal::DisplayChanged.into()));
    assert_eq!(rig.screen.frame().image(), None);
}

#[test]
fn bounds_are_checked_before_the_platform_is_asked() {
    let mut rig = Rig::new([4, 3], 7);
    for (generation, size) in [
        (1, [0, 3]),
        (2, [4, 0]),
        (3, [MAX_SIDE + 1, 1]),
        (4, [1, MAX_SIDE + 1]),
    ] {
        assert_eq!(rig.take(generation, size, 0), Err(Refusal::Invalid.into()));
    }
    let overflow = DisplayRef(PhysicalRect {
        origin: [i32::MAX, 0],
        size: [1, 1],
    });
    assert_eq!(display_edges(&overflow), Err(Refusal::Invalid));
    let largest = DisplayRef(PhysicalRect {
        origin: [i32::MAX - 8192, -8192],
        size: [MAX_SIDE, MAX_SIDE],
    });
    assert_eq!(
        display_edges(&largest),
        Ok([i32::MAX - 8192, -8192, i32::MAX, 0])
    );
    assert_eq!(rig.knobs.calls.get(), 0);
}

#[test]
fn a_failed_open_is_a_fault_and_is_retried_on_the_next_ticket() {
    let mut screen = Screen::default();
    let start = Instant::now();
    let now = move || start;
    let take = |screen: &mut Screen, generation| {
        screen.capture(
            &ticket(generation),
            &rect([4, 3]),
            Exclusion::KumochiWindows,
            start,
            &now,
            (|| Err(-3), crate::screen::elements::tests::open_reader),
        )
    };
    assert_eq!(take(&mut screen, 1), Err(Failure::Fault(-3)));
    assert_eq!(take(&mut screen, 2), Err(Failure::Fault(-3)));
}

#[test]
fn no_carrier_prints_the_nonce_or_the_pixels() {
    let ticket = ticket(1);
    let mut frame = RawFrame::default();
    frame.begin(4, 3).expect("small frame").fill(0xAB);
    let body = crate::screen::wire::RequestBody::Capture {
        ticket: ticket.clone(),
        display: rect([4, 3]),
        exclude: Exclusion::KumochiWindows,
    };
    for text in [
        format!("{ticket:?}"),
        format!("{frame:?}"),
        format!("{body:?}"),
        format!("{:?}", Failure::Refused(Refusal::Expired)),
    ] {
        assert!(!text.contains("171"), "{text}");
        assert!(!text.as_bytes().contains(&0xAB), "{text}");
    }
    assert_eq!(
        format!("{ticket:?}"),
        "TicketWire { run: RunId(3), capture: CaptureId(10), generation: 1, .. }"
    );
    assert_eq!(
        format!("{frame:?}"),
        "RawFrame { bytes: 48, valid: None, .. }"
    );
}

#[path = "capture_elements_tests.rs"]
mod elements;
