use super::{
    BODY_CEILING, JPEG_CAP, NOISE, SLACK, Set, halves, images, keep, matches, report, requests,
    sized,
};
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
    let (jpeg, height) = sized(256, 32, tallest).expect("JPEG at the cap");
    assert_eq!((jpeg.len(), height), (tallest, 32));
}

#[test]
#[ignore = "encodes megapixel JPEGs; run once before a live image run"]
fn the_three_requests_build_with_each_large_image_within_two_percent_under_its_target() {
    let built = requests(Set::Check).expect("built");
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
        report(
            1,
            "small",
            [256, 256],
            900,
            1_300,
            812,
            Ok((true, Some(1_840)))
        ),
        "request=1 image=small size=256x256 jpeg_bytes=900 body_bytes=1300 status=2xx code=- \
         param=- result=completed input_tokens=1840 ms=812 content_match=true"
    );
    assert_eq!(
        report(
            2,
            "flat-large",
            [1024, 1024],
            20_000,
            27_000,
            9,
            Ok((false, None))
        ),
        "request=2 image=flat-large size=1024x1024 jpeg_bytes=20000 body_bytes=27000 status=2xx \
         code=- param=- result=completed input_tokens=- ms=9 content_match=false"
    );
    let refused = Failure::Provider(ProviderError {
        status: 413,
        code: None,
        param: None,
    });
    assert_eq!(
        report(3, "ceiling", [7, 8], 3_000_000, 4_100_000, 5, Err(refused)),
        "request=3 image=ceiling size=7x8 jpeg_bytes=3000000 body_bytes=4100000 status=413 \
         code=- param=- result=refused input_tokens=- ms=5 content_match=-"
    );
    let unsupported = Failure::Provider(ProviderError {
        status: 400,
        code: Some(Code::Unsupported),
        param: Some(Param::Input),
    });
    assert_eq!(
        report(1, "small", [1, 2], 9, 10, 11, Err(unsupported)),
        "request=1 image=small size=1x2 jpeg_bytes=9 body_bytes=10 status=400 \
         code=Unsupported param=Input result=refused input_tokens=- ms=11 content_match=-"
    );
    assert_eq!(
        report(2, "cap", [3, 4], 2, 3, 4, Err(Failure::Transport)),
        "request=2 image=cap size=3x4 jpeg_bytes=2 body_bytes=3 status=- code=- param=- \
         result=transport input_tokens=- ms=4 content_match=-"
    );
}

#[test]
fn the_diagnostic_sends_the_small_image_noisy_and_the_flat_one_large() {
    let images = images(Set::Diagnostic).expect("built");
    let kinds: Vec<_> = images
        .iter()
        .map(|(kind, size, _)| (*kind, *size))
        .collect();
    assert_eq!(
        kinds,
        [("small-noisy", [256, 256]), ("flat-large", [1024, 1024])]
    );
    // Each half's mean over a block, per channel: noise moves a channel by up to 64.
    let means = |jpeg: &[u8], x: u32, y: u32| {
        let image = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
            .expect("a JPEG")
            .to_rgb8();
        let mut sum = [0_u32; 3];
        for dy in 0..32 {
            for dx in 0..32 {
                for (total, value) in sum.iter_mut().zip(image.get_pixel(x + dx, y + dy).0) {
                    *total += u32::from(value);
                }
            }
        }
        (image.dimensions(), sum.map(|total| total / 1024))
    };
    let small = &images[0].2;
    let (size, [r, g, b]) = means(small, 48, 112);
    assert_eq!(size, (256, 256));
    assert!(
        (208..=240).contains(&r) && g <= 48 && b <= 48,
        "left {r} {g} {b}"
    );
    let (_, [r, g, b]) = means(small, 176, 112);
    assert!(
        r <= 48 && g <= 48 && (208..=240).contains(&b),
        "right {r} {g} {b}"
    );
    let flat = &images[1].2;
    let (size, [r, g, b]) = means(flat, 240, 496);
    assert_eq!(size, (1024, 1024));
    assert!(r >= 248 && g <= 8 && b <= 8, "left {r} {g} {b}");
    let (_, [r, g, b]) = means(flat, 752, 496);
    assert!(r <= 8 && g <= 8 && b >= 248, "right {r} {g} {b}");
    assert_eq!(requests(Set::Diagnostic).expect("built").len(), 2);
}

#[test]
fn the_completed_responses_input_tokens_are_kept_beside_the_answer() {
    use crate::connections::model::sse::SseEvent;
    let event = |value| SseEvent::from_value(value).expect("an event");
    let (mut answer, mut tokens) = (String::new(), None);
    keep(
        &mut answer,
        &mut tokens,
        &event(serde_json::json!({ "type": "response.output_text.delta", "delta": "red blue" })),
    );
    assert_eq!((answer.as_str(), tokens), ("red blue", None));
    keep(
        &mut answer,
        &mut tokens,
        &event(serde_json::json!({
            "type": "response.completed",
            "response": { "status": "completed", "usage": { "input_tokens": 1234, "output_tokens": 3 } },
        })),
    );
    assert_eq!((answer.as_str(), tokens), ("red blue", Some(1234)));
    keep(
        &mut answer,
        &mut tokens,
        &event(serde_json::json!({ "type": "response.completed", "response": {} })),
    );
    assert_eq!(tokens, None, "a completion without usage counts nothing");
}
