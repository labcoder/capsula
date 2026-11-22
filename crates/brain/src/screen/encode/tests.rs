use super::*;
use crate::screen::frame::RawFrame;
use crate::screen::mask::{MaskSet, MaskSink, Unavailable};
use crate::screen::sanitize::sanitize;
use crate::screen::wire::{CaptureId, FrameMeta};

fn rect(origin: [i32; 2], size: [u32; 2]) -> PhysicalRect {
    PhysicalRect { origin, size }
}

fn frame(display: PhysicalRect, pixel: impl Fn(u32, u32) -> [u8; 4]) -> RawFrame {
    let mut frame = RawFrame::default();
    let bytes = frame.begin(display.size[0], display.size[1]).unwrap();
    for y in 0..display.size[1] {
        for x in 0..display.size[0] {
            let at = usize::try_from((y * display.size[0] + x) * 4).unwrap();
            bytes[at..at + 4].copy_from_slice(&pixel(x, y));
        }
    }
    frame
        .commit(FrameMeta {
            capture: CaptureId(1),
            display,
            dpi: 96,
            acquired_after_ms: 0,
        })
        .unwrap();
    frame
}

fn decode(encoded: &Encoded) -> image::RgbImage {
    image::load_from_memory_with_format(&encoded.jpeg, image::ImageFormat::Jpeg)
        .unwrap()
        .into_rgb8()
}

#[test]
fn transforms_keep_the_origin_and_the_coordinate_spaces() {
    let mut t = CaptureTransform {
        display: rect([0, 0], [2560, 1440]),
        dpi: 96,
        overview_size: [1280, 720],
        crop: None,
    };
    assert_eq!(
        t.overview_to_physical(OverviewPoint([640.0, 360.0])),
        PhysicalPoint([1280, 720])
    );
    assert_eq!(
        t.physical_to_overview(PhysicalPoint([1280, 720])),
        Some(OverviewPoint([640.0, 360.0]))
    );
    assert_eq!(t.crop_to_physical(PhysicalPoint([4, 5])), None);
    t.display.origin = [-2560, -1440];
    t.crop = Some(rect([-2000, -1000], [1024, 576]));
    assert_eq!(
        t.overview_to_physical(OverviewPoint([640.0, 360.0])),
        PhysicalPoint([-1280, -720])
    );
    assert_eq!(
        t.physical_to_overview(PhysicalPoint([-1280, -720])),
        Some(OverviewPoint([640.0, 360.0]))
    );
    assert_eq!(
        t.crop_to_physical(PhysicalPoint([40, 50])),
        Some(PhysicalPoint([-1960, -950]))
    );
    assert_eq!(
        t.physical_to_logical(PhysicalPoint([-1360, -720])),
        LogicalPoint([1200.0, 720.0])
    );
    t.dpi = 120;
    assert_eq!(
        t.physical_to_logical(PhysicalPoint([-1360, -720])),
        LogicalPoint([960.0, 576.0])
    );
    t.dpi = 144;
    assert_eq!(
        t.physical_to_logical(PhysicalPoint([-1360, -720])),
        LogicalPoint([800.0, 480.0])
    );
    for point in [[-2561, -1440], [0, -1440], [-2560, -1441], [-2560, 0]] {
        assert_eq!(t.physical_to_overview(PhysicalPoint(point)), None);
    }
    assert_eq!(
        t.physical_to_overview(PhysicalPoint([-2560, -1440])),
        Some(OverviewPoint([0.0, 0.0]))
    );
    assert_eq!(
        t.physical_to_overview(PhysicalPoint([-1, -1])),
        Some(OverviewPoint([1279.5, 719.5]))
    );
}

#[test]
fn crops_clip_at_all_four_display_edges() {
    let display = rect([-2560, -100], [2560, 1440]);
    let mut raw = frame(display, |_, _| [20, 40, 60, 255]);
    let clean = sanitize(&mut raw, &MaskSet::new(CaptureId(1))).unwrap();
    for (input, expected) in [
        (rect([-2570, 0], [30, 40]), rect([-2560, 0], [20, 40])),
        (rect([-20, 0], [30, 40]), rect([-20, 0], [20, 40])),
        (rect([-2000, -110], [30, 40]), rect([-2000, -100], [30, 30])),
        (rect([-2000, 1320], [30, 40]), rect([-2000, 1320], [30, 20])),
    ] {
        let encoded = crop(&clean, input).unwrap();
        assert_eq!(encoded.transform.crop, Some(expected));
        assert_eq!(encoded.size, expected.size);
        assert_eq!(
            decode(&encoded).dimensions(),
            (expected.size[0], expected.size[1])
        );
    }
    let one = crop(&clean, rect([-2560, -100], [1, 1])).unwrap();
    assert_eq!(one.size, [1, 1]);
    assert_eq!(one.transform.crop, Some(rect([-2560, -100], [1, 1])));
    assert_eq!(decode(&one).dimensions(), (1, 1));
    for region in [
        rect([0, -100], [1, 1]),
        rect([-2561, -100], [1, 1]),
        rect([-2560, -101], [1, 1]),
        rect([-2560, 1340], [1, 1]),
        rect([-2000, 0], [0, 1]),
        rect([-2000, 0], [1, 0]),
    ] {
        assert!(
            matches!(
                crop(&clean, region),
                Err(Failure::Refused(Refusal::Invalid))
            ),
            "{region:?}"
        );
    }
}

#[test]
fn oversized_crop_is_centred_native_pixels_not_a_scaled_region() {
    let mut raw = frame(rect([-2560, 0], [2560, 1440]), |x, _| {
        let value = if x < 1024 { 0 } else { 255 };
        [value, value, value, 255]
    });
    let clean = sanitize(&mut raw, &MaskSet::new(CaptureId(1))).unwrap();
    let encoded = crop(&clean, rect([-2560, 0], [2000, 1000])).unwrap();
    assert_eq!(encoded.size, [1024, 576]);
    assert_eq!(
        encoded.transform.crop,
        Some(rect([-2072, 212], [1024, 576]))
    );
    let decoded = decode(&encoded);
    assert_eq!(decoded.dimensions(), (1024, 576));
    for y in 0..576 {
        for x in 0..1024 {
            let value = if x < 536 { 0 } else { 255 };
            assert_eq!(decoded.get_pixel(x, y).0, [value; 3], "{x},{y}");
        }
    }
}

#[test]
fn masks_clip_to_the_display_and_black_every_pixel_including_the_right_edge() {
    let mut raw = frame(rect([-4, -3], [6, 5]), |_, _| [13, 27, 41, 7]);
    let mut masks = MaskSet::new(CaptureId(1));
    masks.push(rect([-6, -4], [5, 4]));
    masks.push(rect([0, 0], [u32::MAX, u32::MAX]));
    masks.push(rect([i32::MIN, i32::MIN], [1, 1]));
    masks.push(rect([-2, -1], [0, 2]));
    masks.push(rect([i32::MAX - 1, 0], [u32::MAX, u32::MAX]));
    let clean = sanitize(&mut raw, &masks).unwrap();
    assert_eq!(clean.generation(), 5);
    let (_, pixels) = clean.image();
    for y in 0..5 {
        for x in 0..6 {
            let expected = if (x < 3 && y < 3) || (x >= 4 && y >= 3) {
                [0, 0, 0, 255]
            } else {
                [13, 27, 41, 7]
            };
            let offset = (y * 6 + x) * 4;
            assert_eq!(&pixels[offset..offset + 4], &expected, "{x},{y}");
        }
    }
    let clipped_crop = rect([-3, -2], [3, 3]);
    assert_eq!(
        resample(&clean, clipped_crop, [3, 3]),
        [
            0, 0, 0, 0, 0, 0, 41, 27, 13, 0, 0, 0, 0, 0, 0, 41, 27, 13, 41, 27, 13, 41, 27, 13, 41,
            27, 13,
        ]
    );

    let mut raw = frame(rect([-2002, 98], [6, 5]), |_, _| [255; 4]);
    let mut masks = MaskSet::new(CaptureId(1));
    masks.push(rect([-2000, 100], [2, 1]));
    masks.push(rect([i32::MAX - 1, 0], [u32::MAX, u32::MAX]));
    let clean = sanitize(&mut raw, &masks).unwrap();
    let (pixels, tail) = clean.image().1.as_chunks::<4>();
    assert!(tail.is_empty());
    for (at, pixel) in pixels.iter().enumerate() {
        let expected = if (14..16).contains(&at) {
            [0, 0, 0, 255]
        } else {
            [255; 4]
        };
        assert_eq!(*pixel, expected, "pixel {at}");
    }
}

#[test]
fn uncommitted_or_invalidated_frames_cannot_be_sanitized() {
    let mut raw = RawFrame::default();
    assert_eq!(
        sanitize(&mut raw, &MaskSet::new(CaptureId(1))).unwrap_err(),
        Refusal::Stale
    );
    raw.begin(1, 1).unwrap().fill(255);
    assert_eq!(
        sanitize(&mut raw, &MaskSet::new(CaptureId(1))).unwrap_err(),
        Refusal::Stale
    );
    let mut raw = frame(rect([0, 0], [1, 1]), |_, _| [255; 4]);
    raw.invalidate();
    assert_eq!(
        sanitize(&mut raw, &MaskSet::new(CaptureId(1))).unwrap_err(),
        Refusal::Stale
    );
}

#[test]
fn overview_dimensions_keep_aspect_and_never_enlarge() {
    for (input, expected) in [
        ([2560, 1440], [1280, 720]),
        ([1920, 1200], [1152, 720]),
        ([1000, 500], [1000, 500]),
        ([1280, 720], [1280, 720]),
        ([1281, 720], [1280, 719]),
        ([1366, 768], [1280, 719]),
        ([1, 8192], [1, 720]),
        ([8192, 1], [1280, 1]),
    ] {
        let mut raw = frame(rect([0, 0], input), |_, _| [255; 4]);
        let clean = sanitize(&mut raw, &MaskSet::new(CaptureId(1))).unwrap();
        let encoded = overview(&clean).unwrap();
        assert_eq!(encoded.size, expected);
        assert_eq!(encoded.transform.overview_size, expected);
        assert_eq!(decode(&encoded).dimensions(), (expected[0], expected[1]));
    }
}

#[test]
fn area_average_rounds_half_up_not_nearest_neighbour() {
    let mut raw = frame(rect([0, 0], [2560, 1440]), |x, y| {
        let value = if (x + y) % 2 == 0 { 0 } else { 255 };
        [value, value, value, 255]
    });
    let clean = sanitize(&mut raw, &MaskSet::new(CaptureId(1))).unwrap();
    // Each 2×2 footprint contains two 0s and two 255s: 127.5 rounds upward to 128.
    assert_eq!(
        resample(&clean, rect([0, 0], [2560, 1440]), [1280, 720]),
        vec![128; 1280 * 720 * 3]
    );
    let encoded = overview(&clean).unwrap();
    assert_eq!(decode(&encoded).into_raw(), vec![128; 1280 * 720 * 3]);
}

#[test]
fn fractional_footprints_are_area_weighted_and_bgra_becomes_rgb() {
    let strip = rect([-3, 4], [3, 1]);
    let mut raw = frame(strip, |x, _| {
        [[0, 30, 60, 7], [90, 120, 150, 7], [180, 210, 240, 7]][usize::try_from(x).unwrap()]
    });
    let clean = sanitize(&mut raw, &MaskSet::new(CaptureId(1))).unwrap();
    assert_eq!(resample(&clean, strip, [2, 1]), [90, 60, 30, 210, 180, 150]);
    let display = rect([-3, 4], [3, 3]);
    let mut raw = frame(display, |x, y| {
        let value = u8::try_from((y * 3 + x) * 18).unwrap();
        [value, value + 30, value + 60, 7]
    });
    let clean = sanitize(&mut raw, &MaskSet::new(CaptureId(1))).unwrap();
    assert_eq!(
        resample(&clean, display, [2, 2]),
        [84, 54, 24, 108, 78, 48, 156, 126, 96, 180, 150, 120]
    );
}

#[test]
fn decoded_overview_and_crop_keep_mask_interiors_black() {
    let mut raw = frame(rect([-2560, 0], [2560, 1440]), |_, _| [255; 4]);
    let mut masks = MaskSet::new(CaptureId(1));
    masks.push(rect([-2496, 64], [256, 256]));
    let clean = sanitize(&mut raw, &masks).unwrap();
    let encoded = overview(&clean).unwrap();
    // image 0.25.10 uses 4:4:4 (all SOF sampling factors 1×1); neutral pixels avoid chroma loss.
    let sof = encoded
        .jpeg
        .windows(2)
        .position(|bytes| bytes == [0xFF, 0xC0])
        .unwrap();
    assert_eq!(
        &encoded.jpeg[sof + 10..sof + 19],
        &[1, 0x11, 0, 2, 0x11, 1, 3, 0x11, 1]
    );
    let overview = decode(&encoded);
    let crop = decode(&crop(&clean, rect([-2480, 48], [256, 288])).unwrap());
    // Quality-85 JPEG may ring near a boundary: inset a full 8×8 block, tolerance 16/255.
    for (image, xs, ys, [left, top, right, bottom]) in [
        (overview, 40..152, 40..152, [32_u32, 32, 160, 160]),
        (crop, 8..232, 24..264, [0, 16, 240, 272]),
    ] {
        for (x, y, pixel) in image.enumerate_pixels() {
            if x < left.saturating_sub(8)
                || x >= right + 8
                || y < top.saturating_sub(8)
                || y >= bottom + 8
            {
                assert!(pixel.0.into_iter().all(|channel| channel >= 239), "{x},{y}");
            }
        }
        for y in ys {
            for x in xs.clone() {
                assert!(
                    image
                        .get_pixel(x, y)
                        .0
                        .into_iter()
                        .all(|channel| channel <= 16),
                    "{x},{y}"
                );
            }
        }
    }
}

#[test]
fn encoded_images_carry_their_capture_and_superseding_generation() {
    let mut raw = frame(rect([0, 0], [64, 64]), |_, _| [255; 4]);
    let mut masks = MaskSet::new(CaptureId(1));
    let first = overview(&sanitize(&mut raw, &masks).unwrap()).unwrap();
    assert_eq!(first.capture, CaptureId(1));
    assert_eq!(first.generation, 0);
    assert_eq!(first.completeness, Completeness::Pending);
    masks.push(rect([16, 16], [32, 32]));
    let second = overview(&sanitize(&mut raw, &masks).unwrap()).unwrap();
    assert_eq!(second.capture, CaptureId(1));
    assert_eq!(second.generation, 1);
    assert_eq!(decode(&first).get_pixel(32, 32).0, [255; 3]);
    assert_eq!(decode(&second).get_pixel(32, 32).0, [0; 3]);
    assert_eq!(decode(&second).get_pixel(0, 0).0, [255; 3]);
    let second = decode(&second);
    for [x, y] in [[8, 32], [56, 32], [32, 8], [32, 56]] {
        assert_eq!(second.get_pixel(x, y).0, [255; 3]);
    }
    masks.push(rect([0, 0], [16, 16]));
    let third = overview(&sanitize(&mut raw, &masks).unwrap()).unwrap();
    assert_eq!((third.capture, third.generation), (CaptureId(1), 2));
    let third = decode(&third);
    assert_eq!(third.dimensions(), (64, 64));
    assert_eq!(third.get_pixel(0, 0).0, [0; 3]);
    assert_eq!(third.get_pixel(32, 32).0, [0; 3]);
    assert_eq!(third.get_pixel(63, 63).0, [255; 3]);
    let mut raw = frame(rect([0, 0], [2, 1]), |_, _| [13, 27, 41, 7]);
    let mut masks = MaskSet::new(CaptureId(1));
    assert_eq!(masks.generation(), 0);
    assert_eq!(masks.rects(), []);
    for (mask_x, generation, bgra) in [
        (None, 0, [13, 27, 41, 7, 13, 27, 41, 7]),
        (Some(1), 1, [13, 27, 41, 7, 0, 0, 0, 255]),
        (Some(0), 2, [0, 0, 0, 255, 0, 0, 0, 255]),
    ] {
        if let Some(x) = mask_x {
            masks.push(rect([x, 0], [1, 1]));
        }
        let clean = sanitize(&mut raw, &masks).unwrap();
        assert_eq!(clean.generation(), generation);
        assert_eq!(clean.image().1, bgra);
        if generation == 0 {
            assert_eq!(decode(&overview(&clean).unwrap()).dimensions(), (2, 1));
        }
    }
}

#[test]
fn masks_are_capture_bound_and_completeness_reaches_both_encodings() {
    let mut raw = frame(rect([0, 0], [64, 64]), |_, _| [255; 4]);
    raw.commit(FrameMeta {
        capture: CaptureId(2),
        display: rect([0, 0], [64, 64]),
        dpi: 96,
        acquired_after_ms: 0,
    })
    .unwrap();
    let wrong = MaskSet::new(CaptureId(1));
    assert_eq!(sanitize(&mut raw, &wrong).unwrap_err(), Refusal::Stale);
    for completeness in [
        Completeness::Unknown(Unavailable::Busy),
        Completeness::Partial,
        Completeness::Complete,
    ] {
        let sink: MaskSink = std::sync::Arc::new(std::sync::Mutex::new(MaskSet::new(CaptureId(2))));
        let mut masks = sink.lock().unwrap();
        masks.finish(completeness);
        let clean = sanitize(&mut raw, &masks).unwrap();
        assert_eq!(clean.completeness(), completeness);
        assert_eq!(clean.generation(), 1);
        for encoded in [
            overview(&clean).unwrap(),
            crop(&clean, rect([0, 0], [64, 64])).unwrap(),
        ] {
            assert_eq!(encoded.capture, CaptureId(2));
            assert_eq!(encoded.generation, 1);
            assert_eq!(encoded.completeness, completeness);
        }
    }
}

#[test]
fn the_first_finish_wins_so_a_late_complete_cannot_hide_unknown_masks() {
    let mut masks = MaskSet::new(CaptureId(3));
    masks.finish(Completeness::Unknown(Unavailable::Busy));
    masks.finish(Completeness::Complete);
    assert_eq!(
        masks.completeness(),
        Completeness::Unknown(Unavailable::Busy)
    );
    assert_eq!(masks.generation(), 1);
    let mut cut = MaskSet::new(CaptureId(3));
    cut.finish(Completeness::Partial);
    cut.finish(Completeness::Complete);
    assert_eq!(cut.completeness(), Completeness::Partial);
}

#[test]
fn crop_caps_each_axis_independently() {
    let mut raw = frame(rect([0, 0], [2560, 1440]), |_, _| [255; 4]);
    let clean = sanitize(&mut raw, &MaskSet::new(CaptureId(1))).unwrap();
    for (region, expected) in [
        (rect([0, 0], [2000, 100]), rect([488, 0], [1024, 100])),
        (rect([0, 0], [100, 1000]), rect([0, 212], [100, 576])),
        (rect([0, 0], [1024, 576]), rect([0, 0], [1024, 576])),
        (rect([0, 0], [1025, 577]), rect([0, 0], [1024, 576])),
    ] {
        let encoded = crop(&clean, region).unwrap();
        assert_eq!(encoded.transform.crop, Some(expected));
        assert_eq!(encoded.size, expected.size);
        assert_eq!(
            decode(&encoded).dimensions(),
            (expected.size[0], expected.size[1])
        );
    }
}

#[test]
fn image_debug_never_contains_content_as_text_or_byte_lists() {
    let marker = b"PIXEL_SECRET";
    let mut raw = frame(rect([0, 0], [3, 1]), |x, _| {
        let at = usize::try_from(x * 4).unwrap();
        marker[at..at + 4].try_into().unwrap()
    });
    let clean = sanitize(&mut raw, &MaskSet::new(CaptureId(1))).unwrap();
    let mut encoded = overview(&clean).unwrap();
    encoded.jpeg.extend_from_slice(marker);
    for text in [format!("{clean:?}"), format!("{encoded:?}")] {
        assert!(!text.contains("PIXEL_SECRET"), "{text}");
        assert!(
            !text.contains("80, 73, 88, 69, 76, 95, 83, 69, 67, 82, 69, 84"),
            "{text}"
        );
    }
    assert_eq!(
        format!("{clean:?}"),
        "SanitizedFrame { size: [3, 1], bytes: 12, generation: 0 }"
    );
    assert_eq!(
        format!("{encoded:?}"),
        format!(
            "Encoded {{ bytes: {}, size: [3, 1], generation: 0, .. }}",
            encoded.jpeg.len()
        )
    );
}

#[test]
fn the_jpeg_cap_admits_its_exact_size_and_refuses_one_byte_less() {
    let mut raw = frame(rect([0, 0], [64, 48]), |x, y| {
        [
            u8::try_from(x * 4).unwrap(),
            u8::try_from(y * 5).unwrap(),
            u8::try_from((x + y) % 256).unwrap(),
            255,
        ]
    });
    let clean = sanitize(&mut raw, &MaskSet::new(CaptureId(1))).unwrap();
    let len = overview_within(&clean, JPEG_CAP).unwrap().jpeg.len();
    assert_eq!(overview_within(&clean, len).unwrap().jpeg.len(), len);
    assert!(matches!(
        overview_within(&clean, len - 1),
        Err(Failure::Refused(Refusal::TooLarge))
    ));
}
