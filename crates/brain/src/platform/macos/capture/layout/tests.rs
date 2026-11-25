use super::*;

fn screen(id: u32, origin: [f64; 2], points: [f64; 2], scale: usize) -> Screen {
    Screen {
        id,
        origin,
        points,
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "test fixtures are small whole numbers"
        )]
        pixels: points.map(|side| side as usize * scale),
        mirrored: false,
    }
}

fn rect(origin: [i32; 2], size: [u32; 2]) -> PhysicalRect {
    PhysicalRect { origin, size }
}

#[test]
fn one_uniform_scale_maps_points_to_pixels_including_negative_origins() {
    let screens = [
        screen(1, [0.0, 0.0], [1440.0, 900.0], 2),
        screen(2, [-1280.0, -200.0], [1280.0, 800.0], 2),
    ];
    assert_eq!(
        display_at(&screens, PhysicalPoint([-1, -400])),
        Ok(Display {
            id: 2,
            origin: [-1280, -200],
            points: [1280, 800],
            bounds: rect([-2560, -400], [2560, 1600]),
            dpi: 192,
        })
    );
    assert_eq!(
        display_at(&screens, PhysicalPoint([2879, 1799])).map(|display| display.id),
        Ok(1)
    );
}

#[test]
fn an_arrangement_that_does_not_map_exactly_refuses_layout() {
    let main = screen(1, [0.0, 0.0], [1440.0, 900.0], 2);
    let mut mirrored = main;
    mirrored.mirrored = true;
    let mut fractional = main;
    fractional.pixels = [2160, 1350];
    let mut uneven = main;
    uneven.pixels = [2880, 1801];
    let cases: [&[Screen]; 7] = [
        &[],
        &[mirrored],
        &[fractional],
        &[uneven],
        &[main, screen(2, [3000.0, 0.0], [1920.0, 1080.0], 1)],
        &[main, screen(2, [1439.0, 0.0], [1280.0, 800.0], 2)],
        &[screen(1, [0.5, 0.0], [1440.0, 900.0], 2)],
    ];
    for screens in cases {
        assert_eq!(
            display_at(screens, PhysicalPoint([0, 0])),
            Err(Refusal::Layout),
            "{screens:?}"
        );
    }
}

#[test]
fn a_point_on_no_display_including_the_far_edge_refuses_layout() {
    let screens = [screen(1, [0.0, 0.0], [1440.0, 900.0], 2)];
    for point in [[2880, 0], [0, 1800], [-1, 0], [0, -1]] {
        assert_eq!(
            display_at(&screens, PhysicalPoint(point)),
            Err(Refusal::Layout)
        );
    }
}

#[test]
fn the_seal_finds_the_same_display_only_while_the_arrangement_holds() {
    let bounds = rect([0, 0], [2880, 1800]);
    let screens = [screen(1, [0.0, 0.0], [1440.0, 900.0], 2)];
    assert_eq!(covering(&screens, 1, bounds), Some((bounds, 192)));
    let moved = [screen(1, [0.0, 0.0], [1280.0, 800.0], 2)];
    assert_eq!(covering(&moved, 1, bounds), None);
    let mixed = [screens[0], screen(2, [3000.0, 0.0], [1920.0, 1080.0], 1)];
    assert_eq!(covering(&mixed, 1, bounds), None);
    let swapped = [
        screen(2, [0.0, 0.0], [1440.0, 900.0], 2),
        screen(1, [1440.0, 0.0], [1440.0, 900.0], 2),
    ];
    assert_eq!(covering(&swapped, 1, bounds), None);
    assert_eq!(covering(&swapped, 2, bounds), Some((bounds, 192)));
}
