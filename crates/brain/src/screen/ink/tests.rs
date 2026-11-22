use kumochi_protocol::geometry::{PhysicalPoint, PhysicalRect};

use super::{InkBounds, PINK, WHITE, paint};

/// A blank image of `size` whose region is the same size at the origin, so a physical pixel is an
/// image pixel.
fn blank(size: [u32; 2]) -> (Vec<u8>, PhysicalRect) {
    let bytes = usize::try_from(size[0] * size[1] * 3).unwrap();
    (
        vec![0; bytes],
        PhysicalRect {
            origin: [0, 0],
            size,
        },
    )
}

fn points(path: &[[i32; 2]]) -> Vec<PhysicalPoint> {
    path.iter().copied().map(PhysicalPoint).collect()
}

/// Every pixel of `colour`, by position.
fn coloured(rgb: &[u8], width: u32, colour: [u8; 3]) -> Vec<[u32; 2]> {
    rgb.as_chunks::<3>()
        .0
        .iter()
        .enumerate()
        .filter(|(_, pixel)| **pixel == colour)
        .map(|(index, _)| {
            let index = u32::try_from(index).unwrap();
            [index % width, index / width]
        })
        .collect()
}

fn block(xs: std::ops::RangeInclusive<u32>, ys: std::ops::RangeInclusive<u32>) -> Vec<[u32; 2]> {
    ys.flat_map(|y| xs.clone().map(move |x| [x, y])).collect()
}

fn sorted(mut pixels: Vec<[u32; 2]>) -> Vec<[u32; 2]> {
    pixels.sort_by_key(|&[x, y]| (y, x));
    pixels
}

#[test]
fn a_two_point_path_colours_exactly_its_line_and_edge() {
    let (mut rgb, region) = blank([32, 24]);
    let bounds = paint(&mut rgb, [32, 24], region, &points(&[[10, 10], [20, 10]]));
    assert_eq!(
        bounds,
        Some(InkBounds {
            left: 8,
            top: 8,
            right: 23,
            bottom: 13
        })
    );
    let pink = coloured(&rgb, 32, PINK);
    assert_eq!(pink.len(), 39);
    assert_eq!(pink, block(9..=21, 9..=11));
    let mut white = block(9..=21, 8..=8);
    white.extend(block(9..=21, 12..=12));
    white.extend([[8, 9], [22, 9], [8, 10], [22, 10], [8, 11], [22, 11]]);
    let white_seen = coloured(&rgb, 32, WHITE);
    assert_eq!(white_seen.len(), 32);
    assert_eq!(white_seen, sorted(white));
    let untouched = rgb
        .as_chunks::<3>()
        .0
        .iter()
        .filter(|pixel| **pixel == [0, 0, 0])
        .count();
    assert_eq!(untouched, 32 * 24 - 71);
}

#[test]
fn an_empty_or_one_point_path_paints_nothing() {
    for path in [vec![], points(&[[5, 5]])] {
        let (mut rgb, region) = blank([16, 16]);
        assert_eq!(paint(&mut rgb, [16, 16], region, &path), None);
        assert!(rgb.iter().all(|byte| *byte == 0));
    }
}

#[test]
fn a_segment_that_leaves_the_image_is_painted_up_to_its_edge() {
    for (path, left, right, pink, cap) in [
        ([[5, 5], [20, 5]], 3, 10, 4..=9, 3),
        ([[-5, 5], [4, 5]], 0, 7, 0..=5, 6),
    ] {
        let (mut rgb, region) = blank([10, 10]);
        let bounds = paint(&mut rgb, [10, 10], region, &points(&path));
        assert_eq!(
            bounds,
            Some(InkBounds {
                left,
                top: 3,
                right,
                bottom: 8
            })
        );
        assert_eq!(coloured(&rgb, 10, PINK), block(pink.clone(), 4..=6));
        let mut white = block(pink.clone(), 3..=3);
        white.extend(block(pink, 7..=7));
        white.extend([[cap, 4], [cap, 5], [cap, 6]]);
        assert_eq!(coloured(&rgb, 10, WHITE), sorted(white));
    }
}

#[test]
fn a_segment_wholly_outside_paints_nothing() {
    let (mut rgb, region) = blank([10, 10]);
    assert_eq!(
        paint(&mut rgb, [10, 10], region, &points(&[[20, 20], [30, 30]])),
        None
    );
    assert!(rgb.iter().all(|byte| *byte == 0));
}

#[test]
fn a_full_path_between_the_corners_stays_inside_a_scaled_image() {
    let region = PhysicalRect {
        origin: [-2560, 0],
        size: [2560, 1440],
    };
    let size = [1280, 720];
    let mut rgb = vec![0; 1280 * 720 * 3];
    let path: Vec<_> = (0..2_048)
        .map(|index| {
            if index % 2 == 0 {
                PhysicalPoint([-2560, 0])
            } else {
                PhysicalPoint([-1, 1439])
            }
        })
        .collect();
    let bounds = paint(&mut rgb, size, region, &path).expect("a painted line");
    assert_eq!(
        bounds,
        InkBounds {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 720
        }
    );
    assert_eq!(&rgb[..3], PINK);
    assert_eq!(&rgb[rgb.len() - 3..], PINK);
}

#[test]
fn bounds_never_print_their_numbers() {
    let bounds = InkBounds {
        left: 987_654,
        top: 876_543,
        right: 765_432,
        bottom: 654_321,
    };
    let text = format!("{bounds:?}");
    for number in ["987654", "876543", "765432", "654321"] {
        assert!(!text.contains(number), "{text}");
    }
}
