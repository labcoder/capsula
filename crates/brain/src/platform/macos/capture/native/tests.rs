use objc2_core_graphics::CGBitmapContextCreateImage;

use super::*;

/// A premultiplied BGRA image of `pixels`, `width` wide, made through Quartz.
fn image(pixels: &mut [u8], width: usize) -> CFRetained<CGImage> {
    let height = pixels.len() / 4 / width;
    // SAFETY: Reads the framework's constant colour space name.
    let srgb = unsafe { kCGColorSpaceSRGB };
    let space = CGColorSpace::with_name(Some(srgb)).expect("sRGB exists");
    let info = CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0;
    // SAFETY: `pixels` is `width * 4 * height` writable bytes and outlives the context.
    let context = unsafe {
        CGBitmapContextCreate(
            pixels.as_mut_ptr().cast(),
            width,
            height,
            8,
            width * 4,
            Some(&space),
            info,
        )
    }
    .expect("a bitmap context over the fixture");
    CGBitmapContextCreateImage(Some(&context)).expect("an image of the fixture")
}

#[test]
fn drawing_clears_then_copies_every_pixel_in_order_over_old_pixels() {
    // Two columns, three rows, each pixel different: opaque, transparent and partial alpha.
    let source = [
        [0x10, 0x20, 0x30, 0xFF],
        [0x40, 0x50, 0x60, 0xFF],
        [0x00, 0x00, 0x00, 0x00],
        [0x08, 0x10, 0x18, 0x80],
        [0xFF, 0x00, 0x00, 0xFF],
        [0x00, 0x00, 0x40, 0x40],
    ]
    .concat();
    let image = image(&mut source.clone(), 2);
    let mut frame = vec![0xEE; source.len()];
    assert_eq!(draw(&image, [2, 3], &mut frame), Ok(()));
    assert_eq!(frame, source);
}

#[test]
fn drawing_refuses_an_image_of_another_size_or_a_frame_of_another_length() {
    let image = image(&mut [0x11; 2 * 3 * 4], 2);
    let mut frame = vec![0xEE; 2 * 3 * 4];
    assert_eq!(
        draw(&image, [3, 2], &mut frame),
        Err(Refusal::DisplayChanged.into())
    );
    assert_eq!(
        draw(&image, [2, 3], &mut frame[..20]),
        Err(Refusal::Invalid.into())
    );
    assert!(frame.iter().all(|&byte| byte == 0xEE));
}

#[test]
fn only_exactly_one_match_is_the_one() {
    assert_eq!(the_one([7].into_iter()), Some(7));
    assert_eq!(the_one(std::iter::empty::<u8>()), None);
    assert_eq!(the_one([7, 7].into_iter()), None);
}
