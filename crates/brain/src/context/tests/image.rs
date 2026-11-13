//! Images in a request: what's accepted, where one goes, and nothing of it in `Debug`.

use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};

use crate::context::{ImageRefused, InputItem, Jpeg, ModelRequest, Role};
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
fn the_data_url_is_the_prefix_and_padded_base64() {
    for (len, expected) in [(3, 27), (4, 31), (5, 31), (6, 31), (7, 35)] {
        let mut bytes = SOI.to_vec();
        bytes.resize(len, 0);
        let image = jpeg(&bytes);
        assert_eq!(image.data_url_len(), expected, "{len}");
        assert_eq!(image.data_url().len(), expected, "{len}");
        assert_eq!(
            image.data_url().capacity(),
            expected,
            "built without growing: {len}"
        );
    }
    assert_eq!(
        jpeg(&SOI).data_url().as_str(),
        "data:image/jpeg;base64,/9j/"
    );
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
