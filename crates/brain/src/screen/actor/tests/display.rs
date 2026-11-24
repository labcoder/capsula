use std::num::NonZeroU64;

use kumochi_protocol::geometry::PointerPath;

use super::*;

pub(super) const TYPED: TypedDisplay = TypedDisplay {
    id: NonZeroU64::new(9).unwrap(),
    bounds: DISPLAY,
    dpi: 120,
};
const TYPED_POINT: PhysicalPoint = PhysicalPoint([-3, 1]);
const REPLACED_POINT: PhysicalPoint = PhysicalPoint([-2, 1]);

pub(super) fn typed_at(point: PhysicalPoint) -> Option<TypedDisplay> {
    if point == REPLACED_POINT {
        return Some(TypedDisplay {
            id: NonZeroU64::new(10).unwrap(),
            ..TYPED
        });
    }
    TYPED.contains(point).then_some(TYPED)
}

fn typed_key(id: u64) -> Key {
    Key {
        owner: None,
        capture: CaptureId(id),
    }
}

fn typed(id: u64, point: PhysicalPoint, expected: TypedDisplay) -> Request {
    Request::TypedDisplay {
        ticket: TicketWire::new(CaptureId(id), id, [91; 16], 30_000, FAR),
        point,
        expected,
        excluded: 5,
    }
}

fn typed_acquired(rig: &Rig, id: u64) {
    let (request, reply) = rig.reply();
    let Reply::Acquired(key, meta) = reply else {
        panic!("not acquired: {reply:?}");
    };
    assert_eq!(
        (request, key, meta.display, meta.dpi),
        (id, typed_key(id), DISPLAY, 120)
    );
    assert_eq!(rig.reply(), (id, Reply::Complete(key)));
}

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
fn typed_selection_refuses_changed_identity_geometry_dpi_and_outside_point() {
    let rig = Rig::start(None, None, None);
    for (i, (point, expected)) in [
        (
            TYPED_POINT,
            TypedDisplay {
                id: NonZeroU64::new(8).unwrap(),
                ..TYPED
            },
        ),
        (TYPED_POINT, TypedDisplay { dpi: 96, ..TYPED }),
        (
            TYPED_POINT,
            TypedDisplay {
                bounds: PhysicalRect {
                    origin: [-5, 0],
                    size: [5, 3],
                },
                ..TYPED
            },
        ),
        (PhysicalPoint([0, 1]), TYPED),
    ]
    .into_iter()
    .enumerate()
    {
        let id = u64::try_from(i).unwrap() + 1;
        rig.send(id, &typed(id, point, expected));
        assert_eq!(rig.reply(), (id, Reply::Refused(Refusal::DisplayChanged)));
    }
    rig.send(5, &typed(5, REPLACED_POINT, TYPED));
    typed_acquired(&rig, 5);
    rig.send(6, &Request::Seal(typed_key(5)));
    assert_eq!(rig.reply(), (6, Reply::Refused(Refusal::DisplayChanged)));
    rig.scrubbed();
    rig.send(7, &typed(0, TYPED_POINT, TYPED));
    assert_eq!(rig.reply(), (7, Reply::Refused(Refusal::Invalid)));
    rig.finish();
}

#[test]
fn typed_revocation_and_hold_retirement_do_not_cross_capture_owners() {
    let rig = Rig::start(None, None, None);
    rig.send(1, &context(1));
    rig.acquired(1);
    rig.send(2, &Request::Drop(typed_key(1)));
    assert_eq!(rig.reply(), (2, Reply::Refused(Refusal::Stale)));
    rig.send(3, &typed(2, TYPED_POINT, TYPED));
    assert_eq!(rig.reply(), (3, Reply::Busy));
    rig.send(4, &Request::Seal(key(1)));
    assert!(matches!(rig.reply(), (4, Reply::Sealed(key, _)) if key.capture == CaptureId(1)));

    rig.send(5, &typed(5, TYPED_POINT, TYPED));
    typed_acquired(&rig, 5);
    let owner = Owner {
        epoch: 2,
        gesture: 8,
    };
    rig.send(6, &Request::Retire(owner));
    assert_eq!(rig.reply(), (6, Reply::Released(owner, None)));
    rig.send(7, &Request::Drop(typed_key(2)));
    assert_eq!(rig.reply(), (7, Reply::Refused(Refusal::Stale)));
    rig.send(8, &Request::Seal(typed_key(5)));
    let (id, Reply::Sealed(key, picture)) = rig.reply() else {
        panic!("typed frame did not seal");
    };
    assert_eq!((id, key, picture.ink()), (8, typed_key(5), None));
    rig.finish();
}

#[test]
fn typed_drop_during_acquisition_discards_the_returning_pixels() {
    let (gate, entered, release) = gate();
    let rig = Rig::start(None, Some(gate), None);
    rig.send(1, &typed(1, TYPED_POINT, TYPED));
    entered.recv_timeout(PATIENCE).unwrap();
    rig.send(2, &Request::Drop(typed_key(1)));
    assert_eq!(rig.reply(), (2, Reply::Revoked(typed_key(1))));
    release.send(()).unwrap();
    assert_eq!(rig.reply(), (1, Reply::Refused(Refusal::Stale)));
    rig.scrubbed();
    rig.send(3, &Request::Seal(typed_key(1)));
    assert_eq!(rig.reply(), (3, Reply::Refused(Refusal::Stale)));
    rig.finish();
}

#[test]
fn typed_frame_cannot_seal_at_its_worker_ttl() {
    let rig = Rig::start(None, None, None);
    rig.send(1, &typed(1, TYPED_POINT, TYPED));
    typed_acquired(&rig, 1);
    *rig.clock.lock().unwrap() += MAX_TTL;
    rig.send(2, &Request::Seal(typed_key(1)));
    assert_eq!(rig.reply(), (2, Reply::Refused(Refusal::Stale)));
    rig.scrubbed();
    rig.finish();
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
            bottom: 3
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
