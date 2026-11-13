use super::{BODY_CEILING, JPEG_CAP, NOISE, SLACK, halves, matches, report, requests, sized};
use crate::connections::model::error::{Code, Failure, Param, ProviderError};

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
    let (width, cap) = (256, 16_000);
    let (jpeg, height) = sized(width, 128, cap).expect("fits");
    assert!(jpeg.len() <= cap, "{}", jpeg.len());
    let decoded =
        image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).expect("a JPEG");
    assert_eq!((decoded.width(), decoded.height()), (width, height));
    let taller = halves(width, height + 1, NOISE).expect("encoded");
    assert!(taller.len() > cap, "{}", taller.len());
}

#[test]
fn an_image_that_cant_come_within_two_percent_of_its_cap_is_refused() {
    let tallest = halves(256, 32, NOISE).expect("encoded").len();
    assert_eq!(sized(256, 32, tallest + tallest / 10), None);
    assert!(sized(256, 32, tallest).is_some());
}

#[test]
#[ignore = "encodes megapixel JPEGs; run once before a live image run"]
fn the_three_requests_build_with_each_large_image_within_two_percent_under_its_target() {
    let built = requests().expect("built");
    let caps = [JPEG_CAP, (BODY_CEILING - SLACK) / 4 * 3];
    for (kind, [width, height], jpeg_bytes, _, body_bytes) in &built {
        println!(
            "image={kind} size={width}x{height} jpeg_bytes={jpeg_bytes} body_bytes={body_bytes}"
        );
        assert!(*body_bytes <= BODY_CEILING, "{body_bytes}");
    }
    assert_eq!(built.len(), 3);
    for ((_, _, jpeg_bytes, _, _), cap) in built[1..].iter().zip(caps) {
        assert!((cap - cap / 50..=cap).contains(jpeg_bytes), "{jpeg_bytes}");
    }
}

#[test]
fn a_line_holds_counts_and_fixed_words_only() {
    assert_eq!(
        report(1, "small", [256, 256], 900, 1_300, 812, Ok(true)),
        "request=1 image=small size=256x256 jpeg_bytes=900 body_bytes=1300 status=2xx code=- \
         param=- result=completed ms=812 content_match=true"
    );
    let refused = Failure::Provider(ProviderError {
        status: 413,
        code: None,
        param: None,
    });
    assert_eq!(
        report(3, "ceiling", [7, 8], 3_000_000, 4_100_000, 5, Err(refused)),
        "request=3 image=ceiling size=7x8 jpeg_bytes=3000000 body_bytes=4100000 status=413 \
         code=- param=- result=refused ms=5 content_match=-"
    );
    let unsupported = Failure::Provider(ProviderError {
        status: 400,
        code: Some(Code::Unsupported),
        param: Some(Param::Input),
    });
    assert_eq!(
        report(1, "small", [1, 2], 9, 10, 11, Err(unsupported)),
        "request=1 image=small size=1x2 jpeg_bytes=9 body_bytes=10 status=400 \
         code=Unsupported param=Input result=refused ms=11 content_match=-"
    );
    assert_eq!(
        report(2, "cap", [3, 4], 2, 3, 4, Err(Failure::Transport)),
        "request=2 image=cap size=3x4 jpeg_bytes=2 body_bytes=3 status=- code=- param=- \
         result=transport ms=4 content_match=-"
    );
}
