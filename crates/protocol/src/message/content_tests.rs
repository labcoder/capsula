//! H159-1: the frames that carry what the user typed or a model wrote print its length, never
//! the text, and still carry it whole on the wire.

use super::{Content, DataToBrain, DataToShell, RunId, SceneId, ScreenChoice, Step};

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
        assert!(
            shown
                .split(|c: char| !c.is_ascii_digit())
                .any(|n| n == "14")
        );
    }
    let wire = postcard::to_allocvec(&Content::from(SENTINEL)).expect("encodes");
    assert_eq!(wire, postcard::to_allocvec(SENTINEL).expect("encodes"));
}

/// OSC-228: a committed step's label is model text, so the frame prints its length, never the
/// label, and the wire still carries it whole.
#[test]
fn a_step_frame_prints_its_labels_length_never_the_label() {
    let frame = DataToShell::StepCommit {
        run: RunId(2),
        scene: SceneId(3),
        index: 0,
        step: Step {
            label: SENTINEL.to_owned(),
        },
    };
    let shown = format!("{frame:?}");
    let byte_list = format!("{:?}", SENTINEL.as_bytes());
    assert!(!shown.contains(SENTINEL), "the label leaked: {shown}");
    assert!(
        !shown.contains(byte_list.trim_matches(['[', ']'])),
        "the bytes leaked"
    );
    assert!(
        shown
            .split(|c: char| !c.is_ascii_digit())
            .any(|n| n == "14")
    );
    let wire = postcard::to_allocvec(&Step {
        label: SENTINEL.to_owned(),
    })
    .expect("encodes");
    assert_eq!(wire, postcard::to_allocvec(SENTINEL).expect("encodes"));
}
