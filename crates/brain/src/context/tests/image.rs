//! Images in a request: what's accepted, where one goes, and nothing of it in `Debug`.

use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug, ScreenClass};

use crate::context::{
    Geometry, ImageRefused, InputItem, Jpeg, ModelRequest, NoCanvas, Role, Screen,
};
use crate::test_support::SENTINEL;

const SOI: [u8; 3] = [0xFF, 0xD8, 0xFF];

fn jpeg(bytes: &[u8]) -> Jpeg {
    Jpeg::new(bytes.to_vec()).expect("a JPEG's start")
}

fn message(role: Role, text: &str) -> InputItem {
    InputItem::Message {
        role,
        text: text.to_owned(),
    }
}

fn request(connection: Option<ConnectionKind>, input: Vec<InputItem>) -> ModelRequest {
    ModelRequest {
        choice: connection.map(|connection| ModelChoice {
            model: ModelSlug::new("gpt-fixture").expect("slug"),
            effort: Effort::Low,
            connection,
        }),
        instructions: String::new(),
        input,
        tools: Vec::new(),
    }
}

#[test]
fn only_bytes_that_start_like_a_jpeg_are_an_image() {
    assert_eq!(Jpeg::new(Vec::new()), None);
    assert_eq!(Jpeg::new(vec![0xFF, 0xD8]), None);
    assert_eq!(Jpeg::new(b"\x89PNG\r\n\x1a\n".to_vec()), None);
    assert!(Jpeg::new(SOI.to_vec()).is_some());
}

#[test]
fn an_image_goes_right_after_the_newest_user_message() {
    let image = jpeg(&SOI);
    let mut request = request(
        Some(ConnectionKind::ChatGpt),
        vec![
            message(Role::User, "old"),
            message(Role::Assistant, "reply"),
            message(Role::User, "now"),
            message(Role::Developer, "note"),
        ],
    );
    assert_eq!(request.attach(image.clone()), Ok(()));
    assert_eq!(
        request.input,
        [
            message(Role::User, "old"),
            message(Role::Assistant, "reply"),
            message(Role::User, "now"),
            InputItem::Image(image),
            message(Role::Developer, "note"),
        ]
    );
}

#[test]
fn only_the_responses_route_with_a_user_message_takes_an_image() {
    for connection in [
        None,
        Some(ConnectionKind::AnthropicKey),
        Some(ConnectionKind::ClaudeCode),
    ] {
        let mut refused = request(connection, vec![message(Role::User, "now")]);
        let before = refused.clone();
        assert_eq!(
            refused.attach(jpeg(&SOI)),
            Err(ImageRefused::Route),
            "{connection:?}"
        );
        assert_eq!(refused, before, "{connection:?}");
    }
    let mut refused = request(
        Some(ConnectionKind::ChatGpt),
        vec![message(Role::Developer, "rules")],
    );
    assert_eq!(refused.attach(jpeg(&SOI)), Err(ImageRefused::NoTurn));
    assert_eq!(refused.input, [message(Role::Developer, "rules")]);
}

#[test]
fn debug_shows_an_images_length_and_never_its_bytes() {
    let mut bytes = SOI.to_vec();
    bytes.extend_from_slice(SENTINEL.as_bytes());
    let image = jpeg(&bytes);
    let mut request = request(
        Some(ConnectionKind::ChatGpt),
        vec![message(Role::User, "now")],
    );
    request.attach(image.clone()).expect("attached");
    let shown = format!("{request:?} {image:?}");
    let listed = format!("{:?}", SENTINEL.as_bytes());
    let url = image.data_url();
    assert!(!shown.contains(SENTINEL), "{shown}");
    assert!(!shown.contains(listed.trim_matches(['[', ']'])), "{shown}");
    assert!(!shown.contains(&url[url.len() - 8..]), "{shown}");
    assert!(
        shown.contains(&format!("Image(Jpeg({}))", bytes.len())),
        "{shown}"
    );
}

#[test]
fn a_screen_scene_is_drawn_only_on_the_picture_the_request_attaches() {
    for (len, expected) in [
        (3, "data:image/jpeg;base64,/9j/"),
        (4, "data:image/jpeg;base64,/9j/Qg=="),
        (5, "data:image/jpeg;base64,/9j/QkI="),
        (6, "data:image/jpeg;base64,/9j/QkJC"),
        (7, "data:image/jpeg;base64,/9j/QkJCQg=="),
    ] {
        let mut bytes = SOI.to_vec();
        bytes.resize(len, 0x42);
        let image = jpeg(&bytes);
        assert_eq!(image.data_url_len(), expected.len(), "{len} bytes");
        assert_eq!(image.data_url().as_str(), expected, "{len} bytes");
    }

    let image = jpeg(&[0xFF, 0xD8, 0xFF, 0x42]);
    let picture = Screen::Picture(image.clone(), None, Geometry::FIXTURE);
    let input = vec![
        message(Role::User, "look"),
        message(Role::Assistant, "earlier reply"),
        message(Role::User, "look again"),
        message(Role::Developer, "later note"),
    ];
    let screen = "The image is a picture of the user's screen, 1280 by 720 pixels, taken when they \
                  let go of the talk keys, with their mouse pointer where it was then.";
    for connection in [
        None,
        Some(ConnectionKind::ChatGpt),
        Some(ConnectionKind::AnthropicKey),
        Some(ConnectionKind::ClaudeCode),
    ] {
        for pictured in [false, true] {
            let mut request = request(connection, input.clone());
            let before = request.clone();
            let attached = request.add_screen(&picture, pictured);
            let sent = pictured && connection.is_some();
            if sent {
                assert_eq!(attached, Ok(()), "{connection:?}");
                assert_eq!(
                    request.input,
                    [
                        message(Role::User, "look"),
                        message(Role::Assistant, "earlier reply"),
                        message(Role::User, "look again"),
                        InputItem::Image(image.clone()),
                        message(Role::Developer, screen),
                        message(Role::Developer, "later note"),
                    ],
                    "{connection:?}"
                );
            } else {
                assert_eq!(attached, Err(ImageRefused::Route), "{connection:?}");
                assert_eq!(request, before);
            }
            if connection == Some(ConnectionKind::ChatGpt) && pictured {
                let sent = crate::engines::responses::body(&request).expect("small request");
                assert_eq!(
                    sent["input"][3],
                    serde_json::json!({
                        "type": "message", "role": "user", "content": [{
                            "type": "input_image",
                            "image_url": "data:image/jpeg;base64,/9j/Qg==",
                            "detail": "high",
                        }],
                    })
                );
            }
            let canvas = if sent {
                Ok(Geometry::FIXTURE)
            } else {
                Err(NoCanvas::NoPicture)
            };
            assert_eq!(Screen::canvas(Some(&picture), attached.is_ok()), canvas);
        }
    }
}

#[test]
fn a_run_without_a_sent_picture_of_the_first_display_has_no_canvas() {
    let mut beside = Geometry::FIXTURE;
    beside.display.origin = [-1920, 0];
    for (screen, expected) in [
        (None, NoCanvas::NoPicture),
        (Some(Screen::Image(jpeg(&SOI))), NoCanvas::NoPicture),
        (
            Some(Screen::NotIncluded(ScreenClass::NotReady)),
            NoCanvas::NoPicture,
        ),
        (
            Some(Screen::PictureNotIncluded(ScreenClass::NotReady)),
            NoCanvas::NoPicture,
        ),
        (Some(Screen::Withdrawn), NoCanvas::Off),
        (
            Some(Screen::Picture(jpeg(&SOI), None, beside)),
            NoCanvas::OtherDisplay,
        ),
    ] {
        assert_eq!(
            Screen::canvas(screen.as_ref(), true),
            Err(expected),
            "{screen:?}"
        );
    }
}
