use std::time::Instant;

use kumochi_protocol::geometry::{DisplayRef, PhysicalRect};
use kumochi_protocol::message::RunId;

use super::*;
use crate::screen::wire::{CaptureId, Exclusion, TicketWire};
use crate::screen::{Acquired, RawFrame};

/// A platform whose every capture fails with `FAILURE`.
struct Failing<const RETIRE: bool>;

impl<const RETIRE: bool> Capturer for Failing<RETIRE> {
    fn capture_into(
        &mut self,
        _: &DisplayRef,
        _: Exclusion,
        _: Instant,
        _: &mut RawFrame,
    ) -> Result<Acquired, Failure> {
        Err(if RETIRE {
            Failure::Retire(-7)
        } else {
            Failure::Fault(-5)
        })
    }
}

#[test]
fn a_capture_that_must_retire_answers_fault_and_serves_nothing_more() {
    let capture = RequestBody::Capture {
        ticket: TicketWire::new(RunId(1), CaptureId(1), 1, [0; 16], 2_000),
        display: DisplayRef(PhysicalRect {
            origin: [0, 0],
            size: [4, 3],
        }),
        exclude: Exclusion::KumochiWindows,
    };
    let mut input = Vec::new();
    write_message(&mut input, 1, &capture, ScreenKind::MAX_REQUEST).expect("capture");
    write_message(&mut input, 2, &RequestBody::Ping, ScreenKind::MAX_REQUEST).expect("ping");
    let retiring: Open = || -> Result<Box<dyn Capturer>, i32> { Ok(Box::new(Failing::<true>)) };
    let faulting: Open = || -> Result<Box<dyn Capturer>, i32> { Ok(Box::new(Failing::<false>)) };
    for (open, answered) in [
        (retiring, vec![(1, ReplyBody::Fault(-7))]),
        (
            faulting,
            vec![(1, ReplyBody::Fault(-5)), (2, ReplyBody::Pong)],
        ),
    ] {
        let mut output = Vec::new();
        serve_on(
            &mut input.as_slice(),
            &mut output,
            open,
            crate::screen::elements::tests::open_reader,
            &Instant::now,
        )
        .expect("served");
        let mut replies = output.as_slice();
        let mut read = Vec::new();
        while let Some(reply) =
            read_request::<ReplyBody>(&mut replies, ScreenKind::MAX_REPLY).expect("a reply")
        {
            read.push(reply);
        }
        assert_eq!(read, answered);
    }
}

#[path = "worker_elements_tests.rs"]
mod elements;
