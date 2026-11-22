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
fn a_sealed_display_paints_the_path_its_request_carried() {
    use crate::screen::ink::InkBounds;

    let corner = PhysicalPoint([-4, 0]);
    for (ink, expected, contrasting_pixel) in [
        (Vec::new(), None, None),
        (
            vec![corner, corner],
            Some(InkBounds {
                left: 0,
                top: 0,
                right: 3,
                bottom: 3,
            }),
            None,
        ),
        (
            vec![PhysicalPoint([6, 10]), PhysicalPoint([16, 10])],
            Some(InkBounds {
                left: 8,
                top: 8,
                right: 23,
                bottom: 13,
            }),
            Some((15, 10)),
        ),
    ] {
        let rig = Rig::start(None, None, None);
        rig.send(1, &display(1, PhysicalPoint([1, 1]), ink));
        let (id, reply) = rig.reply();
        let Reply::Acquired(acquired, meta) = reply else {
            panic!("not acquired: {reply:?}")
        };
        assert_eq!(
            (id, acquired, meta.capture, meta.display, meta.dpi),
            (1, key(1), CaptureId(1), DISPLAY, 120)
        );
        assert_eq!(rig.reply(), (1, Reply::Complete(key(1))));
        rig.send(2, &Request::Seal(key(1)));
        let (id, reply) = rig.reply();
        let Reply::Sealed(sealed, context) = reply else {
            panic!("not sealed: {reply:?}")
        };
        assert_eq!((id, sealed), (2, key(1)));
        assert_eq!(context.ink(), expected);
        let geometry = context.geometry();
        assert_eq!(
            (
                geometry.display.origin,
                geometry.display.size,
                geometry.display.dpi,
                geometry.picture
            ),
            ([-4, 0], [40, 30], 120, [40, 30])
        );
        let image = image::load_from_memory_with_format(context.jpeg(), image::ImageFormat::Jpeg)
            .expect("a JPEG")
            .to_rgb8();
        assert_eq!(image.dimensions(), (40, 30));
        let background = |x, y| {
            assert!(
                image
                    .get_pixel(x, y)
                    .0
                    .iter()
                    .all(|channel| channel.abs_diff(91) < 16),
                "plain background at {x},{y}"
            );
        };
        background(30, 25);
        if let Some((x, y)) = contrasting_pixel {
            let [red, green, _] = image.get_pixel(x, y).0;
            assert!(red > 200 && green < 100, "pink on the line: {red} {green}");
        } else {
            background(15, 10);
        }
        rig.scrubbed_bytes(40 * 30 * 4);
        rig.send(3, &Request::Seal(key(1)));
        assert_eq!(rig.reply(), (3, Reply::Refused(Refusal::Stale)));
        rig.finish();
    }
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
    rig.send(4, &display(3, PhysicalPoint([1, 1]), Vec::new()));
    assert_eq!(rig.reply(), (4, Reply::Refused(Refusal::Reused)));
    rig.send(5, &display(2, PhysicalPoint([1, 1]), Vec::new()));
    assert_eq!(rig.reply(), (5, Reply::Refused(Refusal::Stale)));
    rig.finish();
}
