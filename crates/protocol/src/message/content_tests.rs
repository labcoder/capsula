//! H159-1: the frames that carry what the user typed or a model wrote print its length, never
//! the text, and still carry it whole on the wire.

use super::{Content, DataToBrain, DataToShell, RunId, SceneId, ScreenChoice, Step};

const SENTINEL: &str = "zqxprivatetext";
const OTHER: &str = "abcotherdialog";

#[test]
fn a_frame_carrying_content_prints_its_length_never_its_text() {
    let submit = |text: &str| DataToBrain::TextSubmit {
        turn: 1,
        screen: ScreenChoice::None,
        thread: None,
        text: text.into(),
    };
    let answer = |text: &str| DataToShell::AssistantText {
        run: RunId(2),
        delta: text.into(),
    };
    let byte_list = format!("{:?}", SENTINEL.as_bytes());
    let byte_list = byte_list.trim_matches(['[', ']']);
    let debug = |text| [format!("{:?}", submit(text)), format!("{:?}", answer(text))];
    let shown = debug(SENTINEL);
    assert!(shown == debug(OTHER), "content affected frame Debug");
    for shown in shown {
        assert!(!shown.contains(SENTINEL), "the text leaked");
        assert!(!shown.contains(byte_list), "the bytes leaked");
        assert!(shown.contains("Content(len=14)"), "text length missing");
    }
    let wire = postcard::to_allocvec(&Content::from(SENTINEL)).expect("encodes");
    assert_eq!(wire, postcard::to_allocvec(SENTINEL).expect("encodes"));
}

/// OSC-228: a committed step's label is model text, so the frame prints its length, never the
/// label, and the wire still carries it whole.
#[test]
fn a_step_frame_prints_its_labels_length_never_the_label() {
    let frame = |label: &str| DataToShell::StepCommit {
        run: RunId(2),
        scene: SceneId(3),
        index: 0,
        step: Step {
            label: label.to_owned(),
        },
    };
    let debug = |label| format!("{:?}", frame(label));
    let shown = debug(SENTINEL);
    assert!(shown == debug(OTHER), "label affected frame Debug");
    let byte_list = format!("{:?}", SENTINEL.as_bytes());
    assert!(!shown.contains(SENTINEL), "the label leaked");
    assert!(
        !shown.contains(byte_list.trim_matches(['[', ']'])),
        "the bytes leaked"
    );
    assert!(shown.contains("label_len: 14"), "label length missing");
    let wire = postcard::to_allocvec(&Step {
        label: SENTINEL.to_owned(),
    })
    .expect("encodes");
    assert_eq!(wire, postcard::to_allocvec(SENTINEL).expect("encodes"));
}
