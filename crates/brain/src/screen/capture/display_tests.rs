use kumochi_protocol::geometry::PhysicalPoint;

use super::*;
use crate::screen::encode::JPEG_CAP;
use crate::screen::ink::InkBounds;

/// A display frame of `size` held under generation 1, with `ink` to paint, as the actor pins it.
fn held(size: [u32; 2], ink: Vec<PhysicalPoint>) -> Rig {
    let mut rig = Rig::new(size, 200);
    rig.take(1, size, 0).expect("a frame");
    rig.screen.pin(
        Shown::Display {
            bounds: rect(size),
            dpi: 144,
        },
        ink,
    );
    rig
}

fn seal(rig: &mut Rig) -> Result<ScreenContext, Failure> {
    rig.screen.seal(CaptureId(10), JPEG_CAP)
}

#[test]
fn a_display_that_moved_rescaled_or_vanished_refuses_and_releases_its_frame() {
    let moved: fn(PhysicalRect) -> Option<(PhysicalRect, u32)> = |bounds| {
        Some((
            PhysicalRect {
                origin: [0, 0],
                size: bounds.size,
            },
            144,
        ))
    };
    let rescaled: fn(PhysicalRect) -> Option<(PhysicalRect, u32)> = |bounds| Some((bounds, 96));
    let vanished: fn(PhysicalRect) -> Option<(PhysicalRect, u32)> = |_| None;
    for display_at in [moved, rescaled, vanished] {
        let mut rig = held([4, 3], Vec::new());
        rig.screen.display_at = display_at;
        assert_eq!(seal(&mut rig), Err(Refusal::DisplayChanged.into()));
        assert_eq!(rig.screen.frame().image(), None);
        assert_eq!(seal(&mut rig), Err(Refusal::Stale.into()));
    }
}

#[test]
fn a_sealed_path_is_painted_on_the_image_and_its_bounds_come_back() {
    let path = vec![PhysicalPoint([-2550, 10]), PhysicalPoint([-2540, 10])];
    let mut rig = held([40, 30], path);
    rig.screen.display_at = |bounds| Some((bounds, 144));
    let context = seal(&mut rig).expect("sealed");
    assert_eq!(
        context.ink(),
        Some(InkBounds {
            left: 8,
            top: 8,
            right: 23,
            bottom: 13
        })
    );
    let image = image::load_from_memory_with_format(context.jpeg(), image::ImageFormat::Jpeg)
        .expect("a JPEG")
        .to_rgb8();
    let [red, green, _] = image.get_pixel(15, 10).0;
    assert!(red > 200 && green < 100, "pink on the line: {red} {green}");
    let [red, green, blue] = image.get_pixel(30, 25).0;
    assert!(
        [red, green, blue]
            .iter()
            .all(|channel| channel.abs_diff(200) < 16)
    );
}

#[test]
fn neither_the_context_nor_its_bounds_print_where_the_user_pointed() {
    let path = vec![PhysicalPoint([-2550, 10]), PhysicalPoint([-2540, 10])];
    let mut rig = held([40, 30], path);
    rig.screen.display_at = |bounds| Some((bounds, 144));
    let context = seal(&mut rig).expect("sealed");
    let debug = format!("{context:?}");
    for sentinel in ["-2550", "-2540", "-2560"] {
        assert!(!debug.contains(sentinel));
    }
    let first = context.ink().expect("painted bounds");
    let second = InkBounds {
        left: 17,
        top: 19,
        right: 27,
        bottom: 29,
    };
    assert_eq!(format!("{:?}", Some(first)), format!("{:?}", Some(second)));
    let context = context.with_ink(Some(second));
    assert_eq!(debug, format!("{context:?}"));
    assert_eq!(debug, format!("{:?}", context.with_ink(None)));
    let shown_debug = |origin, size| {
        format!(
            "{:?}",
            Shown::Display {
                bounds: PhysicalRect { origin, size },
                dpi: 144
            }
        )
    };
    let shown = shown_debug([-2551, 17777], [2560, 1440]);
    for sentinel in ["-2551", "17777", "2560", "1440"] {
        assert!(!shown.contains(sentinel));
    }
    assert_eq!(shown, shown_debug([12345, -45678], [2048, 1080]));
}
