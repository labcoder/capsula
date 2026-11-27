use kumochi_protocol::geometry::PointerPath;

use super::*;

fn display(id: u64, point: PhysicalPoint, ink: Vec<PhysicalPoint>) -> Request {
    Request::Display {
        key: key(id),
        ticket: TicketWire::new(CaptureId(id), id, [91; 16], 30_000, FAR),
        point,
        excluded: 5,
        ink: PointerPath(ink),
    }
}

#[test]
fn a_display_frame_commits_with_its_monitors_bounds() {
    let rig = Rig::start(None, None, None);
    rig.send(
        1,
        &display(1, PhysicalPoint([1, 1]), vec![PhysicalPoint([2, 2]); 2]),
    );
    let (request, reply) = rig.reply();
    let Reply::Acquired(actual, meta) = reply else {
        panic!("not acquired: {reply:?}")
    };
    assert_eq!(
        (request, actual, meta.capture, meta.display, meta.dpi),
        (1, key(1), CaptureId(1), DISPLAY, 120)
    );
    assert_eq!(rig.reply(), (1, Reply::Complete(key(1))));
    rig.finish();
}

#[test]
fn a_sealed_display_paints_the_path_its_request_carried() {
    let rig = Rig::start(None, None, None);
    // The fake's 4×3 frame is its monitor at one image pixel per physical pixel, from x = -4.
    let corner = PhysicalPoint([-4, 0]);
    rig.send(1, &display(1, PhysicalPoint([1, 1]), vec![corner, corner]));
    assert!(matches!(rig.reply(), (1, Reply::Acquired(..))));
    assert_eq!(rig.reply(), (1, Reply::Complete(key(1))));
    rig.send(2, &Request::Seal(key(1)));
    let (id, reply) = rig.reply();
    let Reply::Sealed(sealed, context) = reply else {
        panic!("not sealed: {reply:?}")
    };
    assert_eq!((id, sealed), (2, key(1)));
    assert_eq!(
        context.ink(),
        Some(crate::screen::ink::InkBounds {
            left: 0,
            top: 0,
            right: 3,
            bottom: 3,
            focus: [0, 0],
        })
    );
    rig.scrubbed();
    rig.finish();
}

#[test]
fn a_display_that_changed_refuses_and_scrubs_its_frame() {
    let rig = Rig::start(None, None, None);
    rig.send(1, &display(1, PhysicalPoint([-1, 1]), Vec::new()));
    assert_eq!(rig.reply(), (1, Reply::Refused(Refusal::DisplayChanged)));
    rig.scrubbed();
    rig.finish();
}

#[test]
fn a_display_ticket_is_spent_once_and_an_older_one_is_stale() {
    let rig = Rig::start(None, None, None);
    rig.send(1, &display(2, PhysicalPoint([1, 1]), Vec::new()));
    assert!(matches!(rig.reply(), (1, Reply::Acquired(..))));
    assert_eq!(rig.reply(), (1, Reply::Complete(key(2))));
    rig.send(2, &display(2, PhysicalPoint([1, 1]), Vec::new()));
    assert_eq!(rig.reply(), (2, Reply::Refused(Refusal::Reused)));
    rig.send(3, &display(1, PhysicalPoint([1, 1]), Vec::new()));
    assert_eq!(rig.reply(), (3, Reply::Refused(Refusal::Stale)));
    rig.finish();
}

#[test]
fn a_path_over_the_limit_is_refused_and_still_spends_its_ticket() {
    let rig = Rig::start(None, None, None);
    let over = vec![PhysicalPoint([1, 1]); kumochi_protocol::geometry::MAX_PATH_POINTS + 1];
    rig.send(1, &display(1, PhysicalPoint([1, 1]), over));
    assert_eq!(rig.reply(), (1, Reply::Refused(Refusal::Invalid)));
    rig.send(2, &display(1, PhysicalPoint([1, 1]), Vec::new()));
    assert_eq!(rig.reply(), (2, Reply::Refused(Refusal::Reused)));
    let full = vec![PhysicalPoint([1, 1]); kumochi_protocol::geometry::MAX_PATH_POINTS];
    rig.send(3, &display(3, PhysicalPoint([1, 1]), full));
    assert!(matches!(rig.reply(), (3, Reply::Acquired(..))));
    assert_eq!(rig.reply(), (3, Reply::Complete(key(3))));
    rig.finish();
}
