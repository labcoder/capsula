use kumochi_protocol::geometry::{PhysicalPoint, PhysicalRect};

use super::{Desktop, Display, Screen, UnsupportedLayout};

fn screen(id: u32, origin: [f64; 2], points: [f64; 2], pixels: [usize; 2]) -> Screen {
    Screen {
        id,
        origin,
        points,
        pixels,
        mirrored: false,
    }
}

const BUILT_IN: Screen = Screen {
    id: 1,
    origin: [0.0, 0.0],
    points: [1728.0, 1117.0],
    pixels: [3456, 2234],
    mirrored: false,
};

fn two_side_by_side() -> Desktop {
    Desktop::map(&[
        BUILT_IN,
        screen(2, [-1280.0, 200.0], [1280.0, 720.0], [2560, 1440]),
    ])
    .unwrap()
}

#[test]
fn a_built_in_retina_display_maps_at_twice_its_points() {
    let desktop = Desktop::map(&[BUILT_IN]).unwrap();
    assert_eq!(
        *desktop.primary(),
        Display {
            id: 1,
            origin: [0, 0],
            points: [1728, 1117],
            bounds: PhysicalRect {
                origin: [0, 0],
                size: [3456, 2234],
            },
            scale: 2,
        }
    );
}

#[test]
fn a_display_left_of_the_primary_keeps_its_negative_origin() {
    let desktop = two_side_by_side();
    assert_eq!(desktop.primary().id, 1);
    assert_eq!(
        desktop.displays()[1].bounds,
        PhysicalRect {
            origin: [-2560, 400],
            size: [2560, 1440],
        }
    );
}

#[test]
fn arrangements_the_worker_refuses_are_refused() {
    let half = screen(2, [-1280.0, 0.0], [1280.0, 720.0], [1280, 720]);
    let fractional = screen(1, [0.0, 0.0], [1800.0, 1169.0], [3456, 2234]);
    let uneven = screen(1, [0.0, 0.0], [1728.0, 1117.0], [3456, 2235]);
    let fractional_points = screen(1, [0.0, 0.0], [1728.5, 1117.0], [3457, 2234]);
    let overlapping = screen(2, [1727.0, 0.0], [1280.0, 720.0], [2560, 1440]);
    let off_origin = screen(1, [0.5, 0.0], [1728.0, 1117.0], [3456, 2234]);
    let mirrored = Screen {
        mirrored: true,
        ..BUILT_IN
    };
    let no_primary = screen(2, [1728.0, 0.0], [1280.0, 720.0], [2560, 1440]);
    let zero = screen(1, [0.0, 0.0], [0.0, 1117.0], [0, 2234]);
    for screens in [
        &[][..],
        &[BUILT_IN, half][..],
        &[fractional][..],
        &[uneven][..],
        &[fractional_points][..],
        &[BUILT_IN, overlapping][..],
        &[off_origin][..],
        &[mirrored][..],
        &[no_primary][..],
        &[zero][..],
    ] {
        assert_eq!(Desktop::map(screens), Err(UnsupportedLayout), "{screens:?}");
    }
}

#[test]
fn displays_that_share_an_edge_are_not_an_overlap() {
    let right = screen(2, [1728.0, 0.0], [1280.0, 720.0], [2560, 1440]);
    let desktop = Desktop::map(&[BUILT_IN, right]).unwrap();
    assert_eq!(desktop.displays()[1].bounds.origin, [3456, 0]);
}

#[test]
fn a_point_is_given_to_its_display_in_points_before_scaling() {
    let desktop = two_side_by_side();
    let (display, point) = desktop.physical([-0.25, 300.0]).unwrap();
    assert_eq!((display.id, point), (2, PhysicalPoint([-1, 600])));
    let (display, point) = desktop.physical([0.0, 300.0]).unwrap();
    assert_eq!((display.id, point), (1, PhysicalPoint([0, 600])));
    let (display, point) = desktop.physical([100.75, 1116.5]).unwrap();
    assert_eq!((display.id, point), (1, PhysicalPoint([201, 2233])));
}

#[test]
fn a_point_on_no_display_or_not_a_number_has_none() {
    let desktop = two_side_by_side();
    for point in [
        [1728.0, 10.0],
        [10.0, 1117.0],
        [-10.0, 199.5],
        [-1280.5, 300.0],
        [f64::NAN, 10.0],
        [10.0, f64::INFINITY],
    ] {
        assert_eq!(desktop.physical(point), None, "{point:?}");
    }
}

#[test]
fn the_work_area_flips_appkit_points_onto_physical_pixels() {
    let desktop = two_side_by_side();
    assert_eq!(
        desktop.work_area([64.0, 0.0, 1664.0, 1080.0]),
        Some(PhysicalRect {
            origin: [128, 74],
            size: [3328, 2160],
        })
    );
}

#[test]
fn a_fractional_work_area_edge_rounds_inwards() {
    let desktop = Desktop::map(&[BUILT_IN]).unwrap();
    assert_eq!(
        desktop.work_area([63.5, 0.25, 1664.0, 1079.5]),
        Some(PhysicalRect {
            origin: [128, 76],
            size: [3326, 2156],
        })
    );
}

#[test]
fn a_work_area_that_is_empty_or_leaves_the_primary_has_none() {
    let desktop = Desktop::map(&[BUILT_IN]).unwrap();
    for visible in [
        [0.0, 0.0, 0.0, 1080.0],
        [0.0, 0.0, 1728.0, 0.4],
        [-1.0, 0.0, 1728.0, 1080.0],
        [0.0, -1.0, 1728.0, 1080.0],
        [1.0, 0.0, 1728.0, 1080.0],
        [0.0, 38.0, 1728.0, 1080.0],
        [f64::NAN, 0.0, 1728.0, 1080.0],
    ] {
        assert_eq!(desktop.work_area(visible), None, "{visible:?}");
    }
}
