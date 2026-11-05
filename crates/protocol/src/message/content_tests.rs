//! H159-1: the frames that carry what the user typed or a model wrote print its length, never
//! the text, and still carry it whole on the wire.

use super::{Content, DataToBrain, DataToShell, RunId, ScreenChoice};

const SENTINEL: &str = "zqxprivatetext";

#[test]
fn a_frame_carrying_content_prints_its_length_never_its_text() {
    let submit = DataToBrain::TextSubmit {
        turn: 1,
        screen: ScreenChoice::None,
        thread: None,
        text: SENTINEL.into(),
    };
    let answer = DataToShell::AssistantText {
        run: RunId(2),
        delta: SENTINEL.into(),
    };
    let byte_list = format!("{:?}", SENTINEL.as_bytes());
    let byte_list = byte_list.trim_matches(['[', ']']);
    for shown in [format!("{submit:?}"), format!("{answer:?}")] {
        assert!(!shown.contains(SENTINEL), "the text leaked: {shown}");
        assert!(!shown.contains(byte_list), "the bytes leaked");
        assert!(shown.contains("Content(len=14)"), "{shown}");
    }
    let wire = postcard::to_allocvec(&Content::from(SENTINEL)).expect("encodes");
    assert_eq!(wire, postcard::to_allocvec(SENTINEL).expect("encodes"));
}
