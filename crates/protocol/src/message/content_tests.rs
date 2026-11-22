//! H159-1: the frames that carry what the user typed or a model wrote print its length, never
//! the text, and still carry it whole on the wire.

use super::{Content, DataToBrain, DataToShell, RunId, SceneId, ScreenChoice, Step};

const SENTINEL: &str = "zqxprivatetext";

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
    assert_only_length(|text| format!("{:?}", submit(text)));
    assert_only_length(|text| format!("{:?}", answer(text)));
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
    assert_only_length(|label| format!("{:?}", frame(label)));
    let wire = postcard::to_allocvec(&Step {
        label: SENTINEL.to_owned(),
    })
    .expect("encodes");
    assert_eq!(wire, postcard::to_allocvec(SENTINEL).expect("encodes"));
}

fn assert_only_length(debug: impl Fn(&str) -> String) {
    let shown = debug(SENTINEL);
    assert_eq!(shown, debug("synthetic-data"));
    assert!(!shown.contains(SENTINEL));
    assert!(!shown.contains(format!("{:?}", SENTINEL.as_bytes()).trim_matches(['[', ']'])));
    for (shown, expected) in [(shown, "14"), (debug("é1234567890123"), "15")] {
        assert!(
            shown
                .split(|c: char| !c.is_ascii_digit() && c != '-')
                .any(|n| n == expected)
        );
    }
}
