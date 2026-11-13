use super::{NOISE, WIDTH, halves, matches, report, sized};
use crate::connections::model::error::{Failure, ProviderError};

#[test]
fn only_red_then_blue_matches() {
    for (answer, expected) in [
        ("red blue", true),
        ("Red, Blue.", true),
        ("red\nblue", true),
        ("blue red", false),
        ("red", false),
        ("red blue green", false),
        ("reddish blue", false),
        ("", false),
    ] {
        assert_eq!(matches(answer), expected, "{answer:?}");
    }
}

#[test]
fn the_image_is_red_on_the_left_and_blue_on_the_right() {
    let jpeg = halves(256, 256, 0).expect("encoded");
    let image = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
        .expect("a JPEG")
        .to_rgb8();
    assert_eq!(image.dimensions(), (256, 256));
    let [r, g, b] = image.get_pixel(64, 128).0;
    assert!(r >= 240 && g <= 16 && b <= 16, "left {r} {g} {b}");
    let [r, g, b] = image.get_pixel(192, 128).0;
    assert!(r <= 16 && g <= 16 && b >= 240, "right {r} {g} {b}");
}

#[test]
fn a_sized_image_is_the_tallest_under_its_cap() {
    let cap = 150_000;
    let jpeg = sized(cap).expect("fits");
    assert!(jpeg.len() <= cap, "{}", jpeg.len());
    let height = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
        .expect("a JPEG")
        .height();
    let taller = halves(WIDTH, height + 1, NOISE).expect("encoded");
    assert!(taller.len() > cap, "{}", taller.len());
}

#[test]
fn a_line_holds_counts_and_fixed_words_only() {
    assert_eq!(
        report(1, "small", 900, 1_300, 812, Ok(true)),
        "request=1 image=small jpeg_bytes=900 body_bytes=1300 status=2xx code=- \
         result=completed ms=812 content_match=true"
    );
    let refused = Failure::Provider(ProviderError {
        status: 413,
        code: None,
        param: None,
    });
    assert_eq!(
        report(3, "ceiling", 3_000_000, 4_100_000, 5, Err(refused)),
        "request=3 image=ceiling jpeg_bytes=3000000 body_bytes=4100000 status=413 code=- \
         result=refused ms=5 content_match=-"
    );
    assert_eq!(
        report(2, "cap", 2, 3, 4, Err(Failure::Transport)),
        "request=2 image=cap jpeg_bytes=2 body_bytes=3 status=- code=- result=transport ms=4 \
         content_match=-"
    );
}
